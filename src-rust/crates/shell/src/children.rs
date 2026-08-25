//! Finding and killing the processes a timed-out command left behind.
//!
//! The shell runs inside this process now, so there is no shell pid to kill.
//! What is left when a command's time runs out is whatever it started, and
//! those are this process's own children. Each one leads its own process
//! group, because [`crate::ShellSession::run`] asks brush for
//! `NewProcessGroup`, so killing the group reaches the grandchildren a bare
//! `kill` would leave orphaned.
//!
//! The listing is read from the operating system rather than taken from a
//! spawned `pgrep`. Every command takes it once, before it runs, and on this
//! machine `pgrep -P` cost about 27 ms against a shell command that takes
//! under one. Asking the platform directly costs about 11 µs.

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

/// Every pid whose parent is `parent`.
///
/// An empty set is the honest answer when the platform will not say: killing
/// nothing is better than killing a pid that was read out of a bad listing.
///
/// One implementation per platform, because every command pays for this once
/// and the portable listings are far too slow for that. `pgrep -P` costs about
/// 27 ms on this machine and `sysinfo` about 10 ms, against a shell command
/// that takes under one.
#[cfg(target_os = "linux")]
fn list(parent: u32) -> HashSet<u32> {
    // The kernel keeps the answer in one small file. It is present whenever
    // `CONFIG_PROC_CHILDREN` is set, which is the usual build, and the scan
    // below covers the kernels where it is not.
    let direct = format!("/proc/{parent}/task/{parent}/children");
    if let Ok(text) = std::fs::read_to_string(&direct) {
        return parse_children_file(&text);
    }
    scan_proc(parent)
}

/// The pids in a `/proc/<pid>/task/<tid>/children` file, which are separated
/// by spaces and end with one.
///
/// Compiled everywhere so its tests run everywhere: the parsing is the part
/// that can be wrong, and only Linux machines would otherwise ever check it.
#[cfg(any(target_os = "linux", test))]
fn parse_children_file(text: &str) -> HashSet<u32> {
    text.split_whitespace()
        .filter_map(|pid| pid.parse::<u32>().ok())
        .collect()
}

/// Read every `/proc/<pid>/stat` and keep the ones naming `parent`.
///
/// The fallback for a kernel without `CONFIG_PROC_CHILDREN`.
#[cfg(target_os = "linux")]
fn scan_proc(parent: u32) -> HashSet<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            (parent_from_stat(&stat)? == parent).then_some(pid)
        })
        .collect()
}

/// The parent pid in a `/proc/<pid>/stat` line.
///
/// The fourth field, and the second one holds the executable name in
/// parentheses, which may itself contain spaces. Everything up to the last
/// `)` is therefore skipped rather than split on.
///
/// Compiled everywhere so its tests run everywhere.
#[cfg(any(target_os = "linux", test))]
fn parent_from_stat(stat: &str) -> Option<u32> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn list(parent: u32) -> HashSet<u32> {
    /// Enough for any real process, and a stop for the growth below.
    const CEILING: usize = 1 << 16;

    let Ok(parent) = libc::pid_t::try_from(parent) else {
        return HashSet::new();
    };
    let width = std::mem::size_of::<libc::pid_t>();
    let mut capacity = 64;

    loop {
        let mut pids: Vec<libc::pid_t> = vec![0; capacity];
        let Ok(size) = libc::c_int::try_from(capacity * width) else {
            return HashSet::new();
        };

        // SAFETY: the buffer holds `capacity` pids, which is what the size
        // argument says in bytes, and every one is initialised to zero
        // beforehand. The call answers how many pids it wrote, and writes
        // nothing beyond the size it was given.
        let written = unsafe { libc::proc_listchildpids(parent, pids.as_mut_ptr().cast(), size) };
        if written <= 0 {
            // Zero is a process with no children and -1 is a process that is
            // gone. Both mean there is nothing to kill.
            return HashSet::new();
        }

        let count = (written as usize).min(capacity);
        // A full buffer may have been truncated, and a child that was cut off
        // would outlive a timeout. Ask again with room.
        if count == capacity && capacity < CEILING {
            capacity *= 2;
            continue;
        }

        return pids[..count]
            .iter()
            .filter(|pid| **pid > 0)
            .filter_map(|pid| u32::try_from(*pid).ok())
            .collect();
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
fn list(parent: u32) -> HashSet<u32> {
    // Windows and the rest. `sysinfo` reads the whole process table, which is
    // slower than the two paths above but still an order below spawning
    // `wmic`, and it is safe code that needs no platform API of our own.
    let mut system = sysinfo::System::new();
    // `nothing()` because only the parent link is read. Asking for the command
    // line, the environment or the user of every process on the machine would
    // cost far more than the answer is worth.
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::nothing(),
    );
    let parent = sysinfo::Pid::from_u32(parent);
    system
        .processes()
        .iter()
        .filter(|(_, process)| process.parent() == Some(parent))
        .map(|(pid, _)| pid.as_u32())
        .collect()
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
    fn the_children_file_is_read_as_a_list_of_pids() {
        assert_eq!(
            parse_children_file("123 456 789 "),
            HashSet::from([123, 456, 789])
        );
        assert_eq!(parse_children_file(""), HashSet::new());
        assert_eq!(parse_children_file("\n"), HashSet::new());
        // A line the kernel would never write must not become a pid nobody
        // meant, because the pid that comes out of here is one that gets
        // killed.
        assert_eq!(
            parse_children_file("123 nonsense 456"),
            HashSet::from([123, 456])
        );
    }

    #[test]
    fn the_parent_is_read_past_a_name_that_holds_spaces_and_brackets() {
        // Field two is the executable name in parentheses and the kernel does
        // not escape it, so a process called `foo ) bar` puts both a space and
        // a bracket inside the field that everything else is counted from.
        assert_eq!(parent_from_stat("42 (bash) S 7 42 42 0"), Some(7));
        assert_eq!(parent_from_stat("42 (foo ) bar) S 7 42 42 0"), Some(7));
        assert_eq!(parent_from_stat("42 (a b c) R 1234 1 1"), Some(1234));
        assert_eq!(parent_from_stat("nothing useful"), None);
        assert_eq!(parent_from_stat(""), None);
    }

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
