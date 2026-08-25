//! Lending the process's working directory to a bundled utility.
//!
//! brush keeps its own working directory and hands it to a child through
//! `Command::current_dir`; it never changes the process's. That is right for a
//! child and wrong for a utility running in this process, which resolves
//! `sort list` and a bare `ls` against whatever directory the process is in.
//!
//! There is no per-thread working directory to set on macOS, so the process's
//! is borrowed for the length of the call and put back afterwards. A borrow is
//! shared by everything asking for the same directory, which is what a
//! pipeline needs: its stages run at the same time and all of them want the
//! directory the shell is in. A request for a different directory waits.
//!
//! The wait is bounded. A utility that never finishes would otherwise hold the
//! directory and stop every other session from running one, and a command that
//! reports a plain error beats a session that stops answering.

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

/// How long to wait for another directory's borrow to end.
const PATIENCE: Duration = Duration::from_secs(60);

/// What the process's working directory is lent out as right now.
struct Lent {
    /// The directory it was changed to.
    directory: PathBuf,
    /// How many callers are using it.
    holders: usize,
    /// Where to put it back when the last one is done.
    restore: PathBuf,
}

struct State {
    lent: Mutex<Option<Lent>>,
    freed: Condvar,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| State {
        lent: Mutex::new(None),
        freed: Condvar::new(),
    })
}

/// A borrow of the process's working directory.
///
/// The directory goes back to what it was when the last borrow is dropped.
pub(crate) struct Borrow {
    _private: (),
}

/// Borrow the process's working directory as `directory`.
///
/// Answers an error rather than waiting forever when a different directory is
/// borrowed and does not come free.
pub(crate) fn borrow(directory: &Path) -> std::io::Result<Borrow> {
    let state = state();
    let Ok(mut lent) = state.lent.lock() else {
        return Err(std::io::Error::other(
            "the working directory is in an unknown state",
        ));
    };

    loop {
        match lent.as_mut() {
            None => {
                let restore = std::env::current_dir()?;
                std::env::set_current_dir(directory)?;
                *lent = Some(Lent {
                    directory: directory.to_path_buf(),
                    holders: 1,
                    restore,
                });
                return Ok(Borrow { _private: () });
            }
            Some(current) if current.directory == directory => {
                current.holders += 1;
                return Ok(Borrow { _private: () });
            }
            Some(_) => {
                let (guard, timed_out) = state
                    .freed
                    .wait_timeout(lent, PATIENCE)
                    .map_err(|_| std::io::Error::other("the working directory lock is poisoned"))?;
                lent = guard;
                if timed_out.timed_out() {
                    return Err(std::io::Error::other(
                        "another command is still using the working directory",
                    ));
                }
            }
        }
    }
}

impl Drop for Borrow {
    fn drop(&mut self) {
        let state = state();
        let Ok(mut lent) = state.lent.lock() else {
            return;
        };
        let Some(current) = lent.as_mut() else {
            return;
        };
        current.holders -= 1;
        if current.holders == 0 {
            let _ = std::env::set_current_dir(&current.restore);
            *lent = None;
            state.freed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_changes_for_the_length_of_the_borrow() {
        let before = std::env::current_dir().expect("cwd");
        let dir = tempfile::tempdir().expect("tempdir");
        // macOS puts the temporary directory behind a symlink, and
        // `current_dir` answers the resolved path.
        let target = dir.path().canonicalize().expect("canonicalize");

        {
            let _borrow = borrow(&target).expect("borrow");
            assert_eq!(std::env::current_dir().expect("cwd"), target);
        }

        assert_eq!(std::env::current_dir().expect("cwd"), before);
    }

    #[test]
    fn the_same_directory_is_shared_rather_than_queued() {
        // A pipeline's stages run at the same time and all of them want the
        // directory the shell is in. If the second borrow waited for the
        // first, the pipeline would deadlock.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().canonicalize().expect("canonicalize");

        let first = borrow(&target).expect("first");
        let second = borrow(&target).expect("second");
        assert_eq!(std::env::current_dir().expect("cwd"), target);

        drop(second);
        // Still borrowed by the first holder.
        assert_eq!(std::env::current_dir().expect("cwd"), target);
        drop(first);
    }

    #[test]
    fn a_directory_that_is_not_there_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("never-created");

        assert!(borrow(&gone).is_err());
        // And the failed borrow left nothing behind.
        let target = dir.path().canonicalize().expect("canonicalize");
        let borrowed = borrow(&target).expect("borrow");
        assert_eq!(std::env::current_dir().expect("cwd"), target);
        drop(borrowed);
    }
}
