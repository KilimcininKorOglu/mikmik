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
//! directory the shell is in. A request for a different directory waits, so
//! two sessions in two directories take turns.
//!
//! The wait is asynchronous, and that is not a detail. Two commands running at
//! once can be two futures on one task; a wait that blocked the thread would
//! stop the task that has to poll the first one to completion, and neither
//! would ever finish.
//!
//! The wait is also bounded. A utility that never finishes would otherwise
//! hold the directory and stop every other session from running one, and a
//! command that reports a plain error beats a session that stops answering.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
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
    freed: tokio::sync::Notify,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| State {
        lent: Mutex::new(None),
        freed: tokio::sync::Notify::new(),
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
pub(crate) async fn borrow(directory: &Path) -> std::io::Result<Borrow> {
    let state = state();
    let deadline = tokio::time::Instant::now() + PATIENCE;

    let notified = state.freed.notified();
    tokio::pin!(notified);

    loop {
        // Registered before the directory is checked, so a release between the
        // check and the wait still wakes this caller.
        notified.as_mut().enable();

        match take(directory) {
            Taken::Got(borrow) => return Ok(borrow),
            Taken::Failed(error) => return Err(error),
            Taken::Busy => {}
        }

        if tokio::time::timeout_at(deadline, notified.as_mut())
            .await
            .is_err()
        {
            return Err(std::io::Error::other(
                "another command is still using the working directory",
            ));
        }
        notified.set(state.freed.notified());
    }
}

/// The outcome of one attempt to borrow, with the lock held for the attempt
/// only. Nothing awaits while it is held.
enum Taken {
    Got(Borrow),
    Busy,
    Failed(std::io::Error),
}

fn take(directory: &Path) -> Taken {
    let Ok(mut lent) = state().lent.lock() else {
        return Taken::Failed(std::io::Error::other(
            "the working directory is in an unknown state",
        ));
    };

    match lent.as_mut() {
        None => {
            let restore = match std::env::current_dir() {
                Ok(restore) => restore,
                Err(error) => return Taken::Failed(error),
            };
            if let Err(error) = std::env::set_current_dir(directory) {
                return Taken::Failed(error);
            }
            *lent = Some(Lent {
                directory: directory.to_path_buf(),
                holders: 1,
                restore,
            });
            Taken::Got(Borrow { _private: () })
        }
        Some(current) if current.directory == directory => {
            current.holders += 1;
            Taken::Got(Borrow { _private: () })
        }
        Some(_) => Taken::Busy,
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
            drop(lent);
            state.freed.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_directory_changes_for_the_length_of_the_borrow() {
        let before = std::env::current_dir().expect("cwd");
        let dir = tempfile::tempdir().expect("tempdir");
        // macOS puts the temporary directory behind a symlink, and
        // `current_dir` answers the resolved path.
        let target = dir.path().canonicalize().expect("canonicalize");

        {
            let _borrow = borrow(&target).await.expect("borrow");
            assert_eq!(std::env::current_dir().expect("cwd"), target);
        }

        assert_eq!(std::env::current_dir().expect("cwd"), before);
    }

    #[tokio::test]
    async fn the_same_directory_is_shared_rather_than_queued() {
        // A pipeline's stages run at the same time and all of them want the
        // directory the shell is in. If the second borrow waited for the
        // first, the pipeline would deadlock.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().canonicalize().expect("canonicalize");

        let first = borrow(&target).await.expect("first");
        let second = borrow(&target).await.expect("second");
        assert_eq!(std::env::current_dir().expect("cwd"), target);

        drop(second);
        // Still borrowed by the first holder.
        assert_eq!(std::env::current_dir().expect("cwd"), target);
        drop(first);
    }

    #[tokio::test]
    async fn a_directory_that_is_not_there_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("never-created");

        assert!(borrow(&gone).await.is_err());
        // And the failed borrow left nothing behind.
        let target = dir.path().canonicalize().expect("canonicalize");
        let borrowed = borrow(&target).await.expect("borrow");
        assert_eq!(std::env::current_dir().expect("cwd"), target);
        drop(borrowed);
    }

    #[tokio::test]
    async fn a_second_directory_takes_its_turn_rather_than_deadlocking() {
        // Two sessions in two directories, both wanted at once on one task.
        // A wait that blocked the thread would stop this task polling the
        // first borrow's release, and neither would ever be answered.
        let first_dir = tempfile::tempdir().expect("tempdir");
        let second_dir = tempfile::tempdir().expect("tempdir");
        let first_path = first_dir.path().canonicalize().expect("canonicalize");
        let second_path = second_dir.path().canonicalize().expect("canonicalize");

        let held = borrow(&first_path).await.expect("first");
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(held);
        });

        let second = borrow(&second_path).await.expect("second");
        assert_eq!(std::env::current_dir().expect("cwd"), second_path);
        drop(second);
        release.await.expect("release");
    }
}
