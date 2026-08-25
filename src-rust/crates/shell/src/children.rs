//! Finding and killing the processes a timed-out command left behind.
//!
//! The shell runs inside this process now, so there is no shell pid to kill.
//! What is left when a command's time runs out is whatever it started, and
//! those are this process's own children. Each one leads its own process
//! group, because [`crate::ShellSession::run`] asks brush for
//! `NewProcessGroup`, so killing the group reaches the grandchildren a bare
//! `kill` would leave orphaned.
//!
//! Both listings shell out. That is one process on a path that only runs when
//! a command has already overrun its time, and it buys a correct kill without
//! a platform API crate.

use std::collections::HashSet;

/// The pids of this process's direct children, as best the platform will say.
///
/// An empty set is the honest answer when the platform will not say: killing
/// nothing is better than killing a pid that was read out of a malformed line.
pub(crate) fn direct_children() -> HashSet<u32> {
    list(std::process::id())
}

/// Kill everything that appeared since `before` was taken.
///
/// Only the new ones: a background task the user started earlier through
/// `run_in_background` is also a child of this process, and a timeout on an
/// unrelated command must not take it down.
pub(crate) fn kill_new_since(before: &HashSet<u32>) {
    for pid in direct_children().difference(before) {
        kill_group(*pid);
    }
}

#[cfg(unix)]
fn list(parent: u32) -> HashSet<u32> {
    let Ok(output) = std::process::Command::new("pgrep")
        .arg("-P")
        .arg(parent.to_string())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

#[cfg(windows)]
fn list(parent: u32) -> HashSet<u32> {
    // `wmic` is deprecated but still present on supported Windows versions,
    // and it is the only listing available without linking a platform API
    // crate for this one path. When it is gone the set is empty, and a
    // timed-out command's child outlives the call rather than the call
    // failing.
    let Ok(output) = std::process::Command::new("wmic")
        .args([
            "process",
            "where",
            &format!("(ParentProcessId={parent})"),
            "get",
            "ProcessId",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| *pid != parent)
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn list(_parent: u32) -> HashSet<u32> {
    HashSet::new()
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    // A group that has already exited answers ESRCH, which is the ordinary
    // outcome of racing a process that finished on its own.
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: `killpg` takes a process group id and a signal number and
    // touches no memory this program owns. An invalid group is reported
    // through errno, which is the case ignored above.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_group(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(any(unix, windows)))]
fn kill_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_with_no_children_lists_none() {
        // Whatever this test binary has running, a pid that cannot exist has
        // no children, and the listing must answer that rather than guess.
        assert!(list(u32::MAX).is_empty());
    }

    #[test]
    fn a_child_appears_in_the_listing_and_can_be_killed() {
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["/C", "timeout", "/T", "30"]
            } else {
                vec!["30"]
            })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");

        let listed = direct_children();
        assert!(
            listed.contains(&child.id()),
            "the child {} was not listed among {listed:?}",
            child.id()
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn only_what_appeared_since_the_snapshot_is_killed() {
        let before = direct_children();
        let mut kept = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["/C", "timeout", "/T", "30"]
            } else {
                vec!["30"]
            })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");

        // `kept` started after the snapshot, so it is exactly what a timeout
        // would take down. Taking the snapshot again afterwards proves the
        // difference is what drives the kill rather than the whole set.
        let after = direct_children();
        assert!(after.contains(&kept.id()));
        assert!(!before.contains(&kept.id()));

        let _ = kept.kill();
        let _ = kept.wait();
    }
}
