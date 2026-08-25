// This file is not part of upstream uutils. It is a MikMik patch; see
// `vendor/coreutils/README.md`.
//
// The rest of the uutils source is MIT licensed and its copyright notices are
// untouched.

//! The three standard streams, redirectable for the current thread.
//!
//! Upstream reaches for `std::io::stdout()` directly, which is the process's
//! real standard output. A utility called that way cannot sit in the middle of
//! a pipeline inside a host process: it would write over whatever else the
//! process is printing.
//!
//! Every place a utility obtains one of the three streams goes through this
//! module instead. With no override installed it hands back the real handles,
//! so a standalone `uu_*` binary behaves exactly as it did. With an override
//! installed it hands back the descriptors the host supplied, so the utility's
//! output lands wherever the host redirected it.
//!
//! # The one rule
//!
//! The override is **per thread**. A utility must run entirely on the thread
//! that installed it. That holds here because a `uumain` is synchronous from
//! start to finish, so there is no await boundary for the thread to change
//! across. Anything that moves part of a utility onto another thread breaks
//! this silently: the moved part writes to the real standard output.

use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::sync::Arc;

#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, RawHandle};

thread_local! {
    static OVERRIDE: std::cell::RefCell<Option<Installed>> =
        const { std::cell::RefCell::new(None) };
}

/// The descriptors a host installed for the current thread.
#[derive(Clone)]
struct Installed {
    stdin: Arc<File>,
    stdout: Arc<File>,
    stderr: Arc<File>,
}

/// Run `body` with the three streams pointing at the given files.
///
/// The previous state is restored afterwards, including on unwind, so nesting
/// works and a panicking utility cannot leave the override behind.
///
/// The files are what the host wants the utility to read and write: a pipe, a
/// terminal, a real file. They are borrowed for the call only.
pub fn with_streams<T>(
    stdin: Arc<File>,
    stdout: Arc<File>,
    stderr: Arc<File>,
    body: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Installed>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }

    let previous = OVERRIDE.with(|slot| {
        slot.borrow_mut().replace(Installed {
            stdin,
            stdout,
            stderr,
        })
    });
    let _restore = Restore(previous);
    body()
}

/// Whether the current thread is writing somewhere the host chose.
///
/// Useful to a utility that wants to know it is not talking to the process's
/// own streams; nothing in upstream needs it.
pub fn is_redirected() -> bool {
    OVERRIDE.with(|slot| slot.borrow().is_some())
}

fn installed() -> Option<Installed> {
    OVERRIDE.with(|slot| slot.borrow().clone())
}

// ---------------------------------------------------------------------------
// Standard output
// ---------------------------------------------------------------------------

/// The current thread's standard output.
///
/// Stands in for [`std::io::stdout`] and answers the same shapes: it writes,
/// it locks, it reports whether it is a terminal, and it carries a borrowable
/// descriptor.
pub fn stdout() -> Stdout {
    match installed() {
        Some(streams) => Stdout(Sink::Redirected(streams.stdout)),
        None => Stdout(Sink::Out(std::io::stdout())),
    }
}

/// A handle on the current thread's standard output.
pub struct Stdout(Sink);

/// A locked handle on the current thread's standard output.
///
/// Owned rather than borrowed, so `stdout().lock()` compiles the way it does
/// with [`std::io::Stdout`], where the lock is `'static`.
pub struct StdoutLock(SinkLock);

impl Stdout {
    /// Lock the stream for as long as the returned handle lives.
    ///
    /// Borrows and answers an owned handle, which is what
    /// [`std::io::Stdout::lock`] does: the lock outlives the temporary in
    /// `stdout().lock()`, and the stream itself stays usable afterwards.
    #[must_use]
    pub fn lock(&self) -> StdoutLock {
        StdoutLock(self.0.lock())
    }
}

impl Write for Stdout {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Write for StdoutLock {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

// ---------------------------------------------------------------------------
// Standard error
// ---------------------------------------------------------------------------

/// The current thread's standard error.
pub fn stderr() -> Stderr {
    match installed() {
        Some(streams) => Stderr(Sink::Redirected(streams.stderr)),
        None => Stderr(Sink::Err(std::io::stderr())),
    }
}

/// A handle on the current thread's standard error.
pub struct Stderr(Sink);

/// A locked handle on the current thread's standard error.
pub struct StderrLock(SinkLock);

impl Stderr {
    /// Lock the stream for as long as the returned handle lives.
    #[must_use]
    pub fn lock(&self) -> StderrLock {
        StderrLock(self.0.lock())
    }
}

impl Write for Stderr {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Write for StderrLock {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

// ---------------------------------------------------------------------------
// Standard input
// ---------------------------------------------------------------------------

/// The current thread's standard input.
pub fn stdin() -> Stdin {
    match installed() {
        Some(streams) => Stdin(Source::Redirected(streams.stdin)),
        None => Stdin(Source::Real(std::io::stdin())),
    }
}

/// A handle on the current thread's standard input.
pub struct Stdin(Source);

/// A locked handle on the current thread's standard input.
pub struct StdinLock(SourceLock);

impl Stdin {
    /// Lock the stream for as long as the returned handle lives.
    #[must_use]
    pub fn lock(&self) -> StdinLock {
        StdinLock(self.0.lock())
    }
}

impl Stdin {
    /// Read one line, including its trailing newline, into `line`.
    ///
    /// Stands in for [`std::io::Stdin::read_line`], which is inherent there
    /// too. A redirected stream is read one byte at a time, because the
    /// descriptor is shared with whatever else the host connected to it and a
    /// buffered reader would swallow bytes the next reader is owed.
    ///
    /// # Errors
    ///
    /// Whatever the underlying read answers, and
    /// [`std::io::ErrorKind::InvalidData`] when the line is not UTF-8.
    pub fn read_line(&mut self, line: &mut String) -> std::io::Result<usize> {
        if let Source::Real(handle) = &mut self.0 {
            return handle.read_line(line);
        }

        let mut bytes = Vec::new();
        let mut one = [0u8; 1];
        loop {
            match self.0.read(&mut one)? {
                0 => break,
                _ => {
                    bytes.push(one[0]);
                    if one[0] == b'\n' {
                        break;
                    }
                }
            }
        }
        let read = bytes.len();
        match String::from_utf8(bytes) {
            Ok(text) => {
                line.push_str(&text);
                Ok(read)
            }
            Err(error) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        }
    }
}

impl Read for Stdin {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Read for StdinLock {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

// ---------------------------------------------------------------------------
// The two shapes underneath
// ---------------------------------------------------------------------------

enum Sink {
    Out(std::io::Stdout),
    Err(std::io::Stderr),
    Redirected(Arc<File>),
}

enum SinkLock {
    Out(std::io::StdoutLock<'static>),
    Err(std::io::StderrLock<'static>),
    Redirected(Arc<File>),
}

impl Sink {
    fn lock(&self) -> SinkLock {
        match self {
            Self::Out(handle) => SinkLock::Out(handle.lock()),
            Self::Err(handle) => SinkLock::Err(handle.lock()),
            Self::Redirected(file) => SinkLock::Redirected(file.clone()),
        }
    }
}

impl Write for Sink {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            // A `&File` writes without needing the file itself to be mutable,
            // which is what lets the handle be shared.
            Self::Out(handle) => handle.write(buffer),
            Self::Err(handle) => handle.write(buffer),
            Self::Redirected(file) => (&**file).write(buffer),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Out(handle) => handle.flush(),
            Self::Err(handle) => handle.flush(),
            Self::Redirected(file) => (&**file).flush(),
        }
    }
}

impl Write for SinkLock {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Out(handle) => handle.write(buffer),
            Self::Err(handle) => handle.write(buffer),
            Self::Redirected(file) => (&**file).write(buffer),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Out(handle) => handle.flush(),
            Self::Err(handle) => handle.flush(),
            Self::Redirected(file) => (&**file).flush(),
        }
    }
}

enum Source {
    Real(std::io::Stdin),
    Redirected(Arc<File>),
}

enum SourceLock {
    Real(std::io::StdinLock<'static>),
    // Buffered, because a locked standard input reads line by line and an
    // unbuffered descriptor would turn that into one syscall per byte. Taking
    // the lock is the utility saying it owns the stream from here on, which is
    // what makes the buffering safe.
    Redirected(BufReader<Shared>),
}

/// A file handle several stream objects can hold at once.
///
/// `Arc<File>` cannot be read from directly, but `&File` can, so this is the
/// one line that bridges them.
struct Shared(Arc<File>);

impl Read for Shared {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(buffer)
    }
}

impl Source {
    fn lock(&self) -> SourceLock {
        match self {
            Self::Real(handle) => SourceLock::Real(handle.lock()),
            Self::Redirected(file) => SourceLock::Redirected(BufReader::new(Shared(file.clone()))),
        }
    }
}

impl Read for Source {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Real(handle) => handle.read(buffer),
            Self::Redirected(file) => (&**file).read(buffer),
        }
    }
}

impl Read for SourceLock {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Real(handle) => handle.read(buffer),
            Self::Redirected(reader) => reader.read(buffer),
        }
    }
}

impl BufRead for SourceLock {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        match self {
            Self::Real(handle) => handle.fill_buf(),
            Self::Redirected(reader) => reader.fill_buf(),
        }
    }
    fn consume(&mut self, amount: usize) {
        match self {
            Self::Real(handle) => handle.consume(amount),
            Self::Redirected(reader) => reader.consume(amount),
        }
    }
}

impl BufRead for StdinLock {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.0.fill_buf()
    }
    fn consume(&mut self, amount: usize) {
        self.0.consume(amount);
    }
}

// ---------------------------------------------------------------------------
// Descriptors and terminal detection
// ---------------------------------------------------------------------------
//
// A utility asks whether its output is a terminal to decide on columns and
// colour, and hands the descriptor to `fstat` to refuse `cat f > f`. Both have
// to answer for the stream the utility is really using, not for the process's.

macro_rules! borrowed_descriptor {
    ($name:ty) => {
        #[cfg(unix)]
        impl AsFd for $name {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.0.as_fd()
            }
        }

        // A few utilities hand the descriptor to something that maps or stats
        // it, so the raw number has to be reachable as well.
        #[cfg(unix)]
        impl AsRawFd for $name {
            fn as_raw_fd(&self) -> RawFd {
                self.0.as_fd().as_raw_fd()
            }
        }

        #[cfg(windows)]
        impl AsRawHandle for $name {
            fn as_raw_handle(&self) -> RawHandle {
                self.0.as_handle().as_raw_handle()
            }
        }

        #[cfg(windows)]
        impl AsHandle for $name {
            fn as_handle(&self) -> BorrowedHandle<'_> {
                self.0.as_handle()
            }
        }

        impl $name {
            /// Whether this stream is a terminal.
            ///
            /// An inherent method rather than an implementation of
            /// [`std::io::IsTerminal`]: that trait is sealed, so only the
            /// standard library's own types may implement it. The call site
            /// reads the same either way, because an inherent method is found
            /// before a trait one.
            #[must_use]
            pub fn is_terminal(&self) -> bool {
                self.0.is_terminal()
            }
        }
    };
}

borrowed_descriptor!(Stdout);
borrowed_descriptor!(StdoutLock);
borrowed_descriptor!(Stderr);
borrowed_descriptor!(StderrLock);
borrowed_descriptor!(Stdin);
borrowed_descriptor!(StdinLock);

#[cfg(unix)]
impl AsFd for Sink {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Out(handle) => handle.as_fd(),
            Self::Err(handle) => handle.as_fd(),
            Self::Redirected(file) => file.as_fd(),
        }
    }
}

#[cfg(unix)]
impl AsFd for SinkLock {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Out(handle) => handle.as_fd(),
            Self::Err(handle) => handle.as_fd(),
            Self::Redirected(file) => file.as_fd(),
        }
    }
}

#[cfg(unix)]
impl AsFd for Source {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Real(handle) => handle.as_fd(),
            Self::Redirected(file) => file.as_fd(),
        }
    }
}

#[cfg(unix)]
impl AsFd for SourceLock {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Real(handle) => handle.as_fd(),
            Self::Redirected(reader) => reader.get_ref().0.as_fd(),
        }
    }
}

#[cfg(windows)]
impl AsHandle for Sink {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        match self {
            Self::Out(handle) => handle.as_handle(),
            Self::Err(handle) => handle.as_handle(),
            Self::Redirected(file) => file.as_handle(),
        }
    }
}

#[cfg(windows)]
impl AsHandle for SinkLock {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        match self {
            Self::Out(handle) => handle.as_handle(),
            Self::Err(handle) => handle.as_handle(),
            Self::Redirected(file) => file.as_handle(),
        }
    }
}

#[cfg(windows)]
impl AsHandle for Source {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        match self {
            Self::Real(handle) => handle.as_handle(),
            Self::Redirected(file) => file.as_handle(),
        }
    }
}

#[cfg(windows)]
impl AsHandle for SourceLock {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        match self {
            Self::Real(handle) => handle.as_handle(),
            Self::Redirected(reader) => reader.get_ref().0.as_handle(),
        }
    }
}

impl Sink {
    fn is_terminal(&self) -> bool {
        match self {
            Self::Out(handle) => handle.is_terminal(),
            Self::Err(handle) => handle.is_terminal(),
            Self::Redirected(file) => file.is_terminal(),
        }
    }
}

impl SinkLock {
    fn is_terminal(&self) -> bool {
        match self {
            Self::Out(handle) => handle.is_terminal(),
            Self::Err(handle) => handle.is_terminal(),
            Self::Redirected(file) => file.is_terminal(),
        }
    }
}

impl Source {
    fn is_terminal(&self) -> bool {
        match self {
            Self::Real(handle) => handle.is_terminal(),
            Self::Redirected(file) => file.is_terminal(),
        }
    }
}

impl SourceLock {
    fn is_terminal(&self) -> bool {
        match self {
            Self::Real(handle) => handle.is_terminal(),
            Self::Redirected(reader) => reader.get_ref().0.is_terminal(),
        }
    }
}
