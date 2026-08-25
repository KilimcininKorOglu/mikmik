//! Running a Bash tool call in the session's embedded shell.
//!
//! The shell itself lives in `mikmik-shell` and outlives the command. What
//! this module owns is the other half: giving the command somewhere to write
//! that a program it starts can inherit, and reading that back.
//!
//! On Unix the somewhere is a pty, so `cargo`, `npm`, `git` and `pytest` still
//! answer `isatty()` the way they did when the tool spawned `bash` inside one.
//! On Windows it is a pipe: there is no pty to hand out, and a pipe is what
//! the old `cmd /C` path used.
//!
//! Output is read while the command runs, not after it. A pty buffer holds
//! about 64 KiB and a pipe rather less; a command that fills one and finds
//! nobody reading blocks until its timeout.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// What a command left behind.
pub(crate) struct Ran {
    /// Everything it wrote, stdout and stderr interleaved as a terminal would
    /// have shown them.
    pub output: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

/// Run `command` in the shell belonging to `session_id`.
pub(crate) async fn run(
    command: &str,
    session_id: &str,
    working_dir: &Path,
    timeout: Duration,
) -> anyhow::Result<Ran> {
    let shell = crate::session_brush_shell(session_id, working_dir).await?;
    let capture = Capture::open()?;
    let (stdout, stderr) = capture.sinks()?;

    let outcome = {
        let mut shell = shell.lock().await;
        shell.run(command, stdout, stderr, timeout).await
    };

    // The output is collected whichever way the command ended: a command that
    // failed or timed out has usually printed the reason, and dropping it
    // would leave the model with an exit code and nothing else.
    let output = capture.finish();
    let outcome = outcome?;

    Ok(Ran {
        output,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
    })
}

/// The reading half of the command's output, plus the thread draining it.
struct Capture {
    #[cfg(unix)]
    slave: std::fs::File,
    #[cfg(not(unix))]
    writer: std::io::PipeWriter,
    collected: Arc<parking_lot::Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    reader: std::thread::JoinHandle<()>,
}

impl Capture {
    /// Open the channel and start draining it.
    fn open() -> anyhow::Result<Self> {
        let collected = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let stop = Arc::new(AtomicBool::new(false));

        #[cfg(unix)]
        let (read_end, slave) = {
            let pty = nix::pty::openpty(None, None)
                .map_err(|error| anyhow::anyhow!("could not open a pty: {error}"))?;
            (
                std::fs::File::from(pty.master),
                std::fs::File::from(pty.slave),
            )
        };
        #[cfg(not(unix))]
        let (read_end, writer) =
            std::io::pipe().map_err(|error| anyhow::anyhow!("could not open a pipe: {error}"))?;

        let reader = std::thread::spawn({
            let collected = collected.clone();
            let stop = stop.clone();
            move || drain(read_end, &collected, &stop)
        });

        Ok(Self {
            #[cfg(unix)]
            slave,
            #[cfg(not(unix))]
            writer,
            collected,
            stop,
            reader,
        })
    }

    /// Two handles on the writing side, one for each stream.
    ///
    /// Both point at the same channel on purpose: the model reads the output
    /// as a terminal would have shown it, with an error in the place the
    /// command printed it rather than in a block at the end.
    fn sinks(&self) -> anyhow::Result<(mikmik_shell::Sink, mikmik_shell::Sink)> {
        #[cfg(unix)]
        {
            let out = self.slave.try_clone()?;
            let err = self.slave.try_clone()?;
            Ok((out.into(), err.into()))
        }
        #[cfg(not(unix))]
        {
            let out = self.writer.try_clone()?;
            let err = self.writer.try_clone()?;
            Ok((out.into(), err.into()))
        }
    }

    /// Stop reading and answer what was read.
    ///
    /// The writing side is dropped first, so the reader sees the end of the
    /// stream in the ordinary case. The flag is for the other case: a command
    /// that left a background process holding the channel open would keep the
    /// reader blocked forever, and a thread per such command is a leak.
    fn finish(self) -> String {
        #[cfg(unix)]
        drop(self.slave);
        #[cfg(not(unix))]
        drop(self.writer);

        self.stop.store(true, Ordering::Relaxed);
        let _ = self.reader.join();

        let bytes = std::mem::take(&mut *self.collected.lock());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// How long one poll waits before the stop flag is looked at again.
#[cfg(unix)]
const POLL_MS: i32 = 50;

/// Read the pty until its last writer is gone or the caller says to stop.
///
/// Polled rather than blocked: a command that leaves a background process
/// holding the pty open would keep a blocked read waiting forever, and a
/// thread per such command is a leak.
#[cfg(unix)]
fn drain(
    mut source: std::fs::File,
    collected: &Arc<parking_lot::Mutex<Vec<u8>>>,
    stop: &Arc<AtomicBool>,
) {
    let mut chunk = [0u8; 8192];
    loop {
        if !readable(&source, POLL_MS) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            continue;
        }
        match source.read(&mut chunk) {
            // Zero is a clean end of stream; an error is what a pty answers
            // once its last slave is closed, and both mean the same thing.
            Ok(0) | Err(_) => return,
            Ok(read) => collected.lock().extend_from_slice(&chunk[..read]),
        }
    }
}

/// Read the pipe until every writer is dropped.
///
/// No poll: a pipe answers the end of stream as soon as the last writer goes,
/// and `finish` drops ours before it joins.
#[cfg(not(unix))]
fn drain(
    mut source: std::io::PipeReader,
    collected: &Arc<parking_lot::Mutex<Vec<u8>>>,
    _stop: &Arc<AtomicBool>,
) {
    let mut chunk = [0u8; 8192];
    loop {
        match source.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => collected.lock().extend_from_slice(&chunk[..read]),
        }
    }
}

#[cfg(unix)]
fn readable(source: &std::fs::File, timeout_ms: i32) -> bool {
    use std::os::fd::AsRawFd;
    let mut poll_fd = libc::pollfd {
        fd: source.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll` reads the one descriptor named in the array and writes
    // `revents` back into it. The array outlives the call.
    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    // A negative answer is an interrupted or failed poll; treating it as
    // readable lets `read` report the real error rather than spinning here.
    ready != 0
}
