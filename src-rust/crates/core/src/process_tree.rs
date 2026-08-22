//! Killing a spawned command and everything it started.
//!
//! A shell wrapper (`sh -c`, `cmd /C`, `powershell -Command`) is rarely the
//! process that matters. `sh -c "python train.py"` leaves a `python` running
//! when only the `sh` is killed, and the user who cancelled the turn watches it
//! keep working. Both `tokio`'s `kill_on_drop` and a plain `Child::kill` reach
//! the direct child and stop there.
//!
//! # The Unix precondition
//!
//! On Unix the tree is addressed as a process group, so a command must be
//! spawned through [`spawn_in_own_group`] for [`kill_tree`] and
//! [`terminate_tree`] to reach past the wrapper. Signalling the group of a
//! child that was never placed in its own group would reach this process and
//! everything beside it, so the pid handed to those two functions must be a pid
//! that [`spawn_in_own_group`] prepared.
//!
//! Windows needs no such preparation: `taskkill /T` walks the parent-child
//! links itself.
//!
//! The PTY path in `mikmik-tools` is deliberately not a caller. A pty child is
//! already a session leader carrying its own foreground group, and moving it
//! would change what its existing kill does.

/// Spawn `cmd`'s child in its own process group on Unix.
///
/// This detaches the child from the terminal's job control, so a Ctrl-C typed
/// at the terminal no longer reaches it. Every caller runs its child with piped
/// stdio and cancels through a guard rather than through the terminal, so
/// nothing depended on that path.
pub fn spawn_in_own_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // 0 means "a new group whose id is the child's pid", which is what
        // makes the pid usable as a group id below.
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Same for a command that will be spawned synchronously.
pub fn spawn_std_in_own_group(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Kill `pid` and everything it started, without waiting for the kill to land.
///
/// For the cancel and timeout paths, where the caller has already decided the
/// work is over. Not waiting matches what the PTY guard in `mikmik-tools`
/// does with its own `SIGKILL`, and keeps this callable from a `Drop`, which
/// cannot await.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    signal_group(pid, nix::sys::signal::Signal::SIGKILL);
    #[cfg(windows)]
    taskkill_tree(pid);
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

/// Ask `pid` and everything it started to stop.
///
/// For the explicit "stop this task" path, where the process is being taken
/// away from a user who may want it to clean up after itself. On Windows there
/// is no gentler tree kill that a console program would honour, so this is
/// [`kill_tree`] there.
pub fn terminate_tree(pid: u32) {
    #[cfg(unix)]
    signal_group(pid, nix::sys::signal::Signal::SIGTERM);
    #[cfg(windows)]
    taskkill_tree(pid);
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: nix::sys::signal::Signal) {
    // A group that has already exited answers ESRCH, which is the ordinary
    // outcome of racing a process that finished on its own.
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), signal);
}

/// Console-less spawn flag, so killing a tree never flashes a window on a
/// desktop app. `crates/core/src/lsp.rs` sets the same flag for the same
/// reason.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn taskkill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    // Spawned and not awaited: this is called from `Drop`, which cannot await,
    // and blocking a runtime worker thread on a `taskkill` would stall every
    // other task on it.
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Kills a command's whole process tree if the future running it is dropped
/// before the command finished, e.g. the turn was cancelled or the task was
/// aborted mid-command.
///
/// Disarm it on normal completion, and fire it explicitly with
/// [`ProcessTreeKillGuard::kill_now`] on the timeout path.
pub struct ProcessTreeKillGuard {
    pid: Option<u32>,
    armed: bool,
}

impl ProcessTreeKillGuard {
    /// `pid` comes from `Child::id()`, which answers `None` once the child has
    /// been waited on; there is nothing left to kill in that case.
    pub fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    /// The command finished on its own — there is nothing left to kill.
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// Kill the tree now (timeout path) and disarm so `Drop` is a no-op.
    pub fn kill_now(&mut self) {
        if self.armed {
            if let Some(pid) = self.pid {
                kill_tree(pid);
            }
            self.armed = false;
        }
    }
}

impl Drop for ProcessTreeKillGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Some(pid) = self.pid {
                kill_tree(pid);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

    /// How long a spawned process is given to appear or to go away.
    ///
    /// Generous because the whole workspace's tests share one machine: under
    /// that load `sh` can take well over a second to fork its child.
    const DEADLINE: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(50);

    /// Whether the marked `sleep` is still running.
    ///
    /// Matches the whole `sleep <marker>` command line rather than the marker
    /// alone, so an unrelated process that merely carries those digits (a test
    /// binary's path hash, for instance) is not mistaken for the child.
    fn still_running(marker: &str) -> bool {
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(format!("sleep {marker}"))
            .output()
            .map(|out| !out.stdout.is_empty())
            .unwrap_or(false)
    }

    /// Wait until the marked process is running, or give up at the deadline.
    ///
    /// Polling rather than one fixed sleep: a fixed wait is a race that passes
    /// on an idle machine and fails under the load of a full workspace run,
    /// which is exactly when it is least informative.
    async fn wait_until_running(marker: &str) {
        wait_for(marker, true).await;
    }

    /// Wait until the marked process has gone, or give up at the deadline.
    async fn wait_until_gone(marker: &str) {
        wait_for(marker, false).await;
    }

    async fn wait_for(marker: &str, want_running: bool) {
        let start = std::time::Instant::now();
        while start.elapsed() < DEADLINE {
            if still_running(marker) == want_running {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// A sleep duration no other run and no sibling test can be using.
    ///
    /// A fixed marker read a leftover from an earlier run as this run's
    /// process, and `31337x` markers shared a prefix with the ones in
    /// `mikmik-tools`, so one crate's `pgrep` matched the other's children.
    /// `pty_bash.rs` already numbers its markers this way for the same reason.
    fn unique_marker() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        // Fractional seconds keep the number one `sleep` accepts.
        format!("999336.{}", nanos % 1_000_000_000)
    }

    /// Spawn `sh -c` with a background child, so the wrapper and its child are
    /// two different processes. `wait` keeps the wrapper alive alongside it.
    fn spawn_wrapper_with_child(marker: &str) -> tokio::process::Child {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep {marker} & wait"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        spawn_in_own_group(&mut cmd);
        cmd.spawn().expect("spawn")
    }

    #[tokio::test]
    async fn a_dropped_guard_takes_the_child_of_the_wrapper_too() {
        // The whole defect in one case: killing the wrapper alone leaves the
        // process the user actually started running.
        let marker = &unique_marker();
        let child = spawn_wrapper_with_child(marker);
        wait_until_running(marker).await;
        assert!(still_running(marker), "the child never started");

        {
            let _guard = ProcessTreeKillGuard::new(child.id());
        }
        wait_until_gone(marker).await;

        assert!(
            !still_running(marker),
            "the wrapper's child outlived the kill"
        );
    }

    #[tokio::test]
    async fn killing_now_is_the_same_kill() {
        let marker = &unique_marker();
        let child = spawn_wrapper_with_child(marker);
        wait_until_running(marker).await;
        assert!(still_running(marker), "the child never started");

        let mut guard = ProcessTreeKillGuard::new(child.id());
        guard.kill_now();
        wait_until_gone(marker).await;

        assert!(!still_running(marker));
    }

    #[tokio::test]
    async fn a_disarmed_guard_kills_nothing() {
        // A guard that fires after the command completed would cut short a
        // process the caller meant to keep.
        let marker = &unique_marker();
        let child = spawn_wrapper_with_child(marker);
        wait_until_running(marker).await;
        assert!(still_running(marker), "the child never started");

        {
            let mut guard = ProcessTreeKillGuard::new(child.id());
            guard.disarm();
        }
        // A disarmed guard must leave the tree alone, so there is nothing to
        // wait for. Give it the same window a kill would have had, and then
        // check the process is still there.
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(still_running(marker), "a disarmed guard killed the tree");
        kill_tree(child.id().expect("pid"));
        wait_until_gone(marker).await;
        assert!(!still_running(marker), "cleanup failed");
    }

    #[tokio::test]
    async fn terminating_a_group_reaches_the_child_as_well() {
        let marker = &unique_marker();
        let child = spawn_wrapper_with_child(marker);
        wait_until_running(marker).await;
        assert!(still_running(marker), "the child never started");

        terminate_tree(child.id().expect("pid"));
        wait_until_gone(marker).await;

        assert!(!still_running(marker));
    }

    #[tokio::test]
    async fn signalling_a_group_that_has_gone_is_not_an_error() {
        // Racing a process that finished on its own is the ordinary case, not
        // a failure worth surfacing.
        let mut cmd = Command::new("true");
        spawn_in_own_group(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("pid");
        let _ = child.wait().await;

        kill_tree(pid);
        terminate_tree(pid);
    }
}
