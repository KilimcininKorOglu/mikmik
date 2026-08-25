//! Pointing a bundled utility's output somewhere the shell chose.
//!
//! The utilities in `vendor/coreutils/` reach for their streams through
//! `uucore::streams` rather than `std::io`, which is the one change the fork
//! makes. This module is the other half: it installs the descriptors a utility
//! should use, and it is the only place that knows the rule the fork depends
//! on.
//!
//! # The rule
//!
//! The override is per thread, and a utility must run entirely on the thread
//! that installed it. A `uumain` is synchronous from start to finish, so there
//! is no await boundary for the thread to change across; that is what makes
//! the thread the right scope. Moving any part of a utility onto another
//! thread breaks it silently, because the moved part writes to the process's
//! real standard output instead.

use std::fs::File;
use std::sync::Arc;

/// The three descriptors a utility reads and writes.
pub struct Streams {
    /// Where the utility reads its input.
    pub stdin: Arc<File>,
    /// Where the utility writes its output.
    pub stdout: Arc<File>,
    /// Where the utility writes its complaints.
    pub stderr: Arc<File>,
}

/// Run `body` with the three streams installed for this thread.
///
/// The previous state comes back afterwards, on a panic as well, so one
/// utility can run inside another's pipeline.
pub fn with_streams<T>(streams: Streams, body: impl FnOnce() -> T) -> T {
    uucore::streams::with_streams(streams.stdin, streams.stdout, streams.stderr, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A file the test can write through and read back.
    fn scratch(dir: &std::path::Path, name: &str) -> Arc<File> {
        Arc::new(
            std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(true)
                .open(dir.join(name))
                .expect("open"),
        )
    }

    fn read_back(dir: &std::path::Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).expect("read")
    }

    fn three(dir: &std::path::Path, out: &str) -> Streams {
        Streams {
            stdin: scratch(dir, "in"),
            stdout: scratch(dir, out),
            stderr: scratch(dir, "err"),
        }
    }

    #[test]
    fn with_nothing_installed_the_real_streams_are_handed_out() {
        // A machine running `mikmik` normally must print to its own terminal.
        // Only a utility the shell is driving gets an override.
        assert!(!uucore::streams::is_redirected());
    }

    #[test]
    fn output_lands_in_the_file_the_shell_supplied() {
        let dir = tempfile::tempdir().expect("tempdir");

        with_streams(three(dir.path(), "out"), || {
            write!(uucore::streams::stdout(), "to stdout").expect("write");
            write!(uucore::streams::stderr(), "to stderr").expect("write");
        });

        assert_eq!(read_back(dir.path(), "out"), "to stdout");
        assert_eq!(read_back(dir.path(), "err"), "to stderr");
    }

    #[test]
    fn a_locked_handle_writes_to_the_same_place() {
        // Most utilities take the lock once and write through it for the whole
        // run, so the lock has to follow the override too.
        let dir = tempfile::tempdir().expect("tempdir");

        with_streams(three(dir.path(), "out"), || {
            let mut locked = uucore::streams::stdout().lock();
            write!(locked, "through the lock").expect("write");
            locked.flush().expect("flush");
        });

        assert_eq!(read_back(dir.path(), "out"), "through the lock");
    }

    #[test]
    fn input_is_read_from_the_file_the_shell_supplied() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("source"), "a line\nanother\n").expect("write");

        let streams = Streams {
            stdin: Arc::new(File::open(dir.path().join("source")).expect("open")),
            stdout: scratch(dir.path(), "out"),
            stderr: scratch(dir.path(), "err"),
        };

        let (first, rest) = with_streams(streams, || {
            let mut line = String::new();
            uucore::streams::stdin()
                .read_line(&mut line)
                .expect("read_line");
            let mut remainder = String::new();
            uucore::streams::stdin()
                .read_to_string(&mut remainder)
                .expect("read");
            (line, remainder)
        });

        assert_eq!(first, "a line\n");
        assert_eq!(rest, "another\n");
    }

    #[test]
    fn the_previous_state_comes_back_afterwards() {
        // One utility can run inside another's pipeline, so the override has
        // to nest rather than replace.
        let dir = tempfile::tempdir().expect("tempdir");

        with_streams(three(dir.path(), "outer"), || {
            write!(uucore::streams::stdout(), "outer ").expect("write");
            with_streams(
                Streams {
                    stdin: scratch(dir.path(), "in2"),
                    stdout: scratch(dir.path(), "inner"),
                    stderr: scratch(dir.path(), "err2"),
                },
                || {
                    write!(uucore::streams::stdout(), "inner").expect("write");
                },
            );
            write!(uucore::streams::stdout(), "again").expect("write");
        });

        assert_eq!(read_back(dir.path(), "outer"), "outer again");
        assert_eq!(read_back(dir.path(), "inner"), "inner");
        assert!(!uucore::streams::is_redirected());
    }

    #[test]
    fn a_panic_does_not_leave_the_override_behind() {
        // A utility that panics mid-pipeline would otherwise leave every later
        // write on this thread pointing at a file nobody is reading.
        let dir = tempfile::tempdir().expect("tempdir");
        let caught = std::panic::catch_unwind(|| {
            with_streams(three(dir.path(), "out"), || panic!("on purpose"));
        });

        assert!(caught.is_err());
        assert!(!uucore::streams::is_redirected());
    }

    #[test]
    fn a_real_utility_writes_where_it_was_told() {
        // The whole point of the fork. `sort` obtains its output through
        // `uucore::streams`, so it lands in the file rather than on the
        // process's own standard output.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("source"), "ccc\naaa\nbbb\n").expect("write");

        // The file is named on the command line rather than piped in, so a
        // regression in the output path fails here instead of blocking on the
        // process's real standard input.
        let source = dir.path().join("source").display().to_string();
        let code = with_streams(three(dir.path(), "out"), || {
            let run = crate::bundled::registry()
                .get("sort")
                .expect("sort is bundled");
            run(vec![
                std::ffi::OsString::from("sort"),
                std::ffi::OsString::from(&source),
            ])
        });

        assert_eq!(code, 0);
        assert_eq!(read_back(dir.path(), "out"), "aaa\nbbb\nccc\n");
    }

    #[test]
    fn a_utility_that_counts_reports_through_the_same_path() {
        // `wc` takes the locked handle rather than the plain one, which is a
        // separate route through the fork.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("source"), "one\ntwo\nthree\n").expect("write");
        let source = dir.path().join("source").display().to_string();

        let code = with_streams(three(dir.path(), "out"), || {
            let run = crate::bundled::registry().get("wc").expect("wc is bundled");
            run(vec![
                std::ffi::OsString::from("wc"),
                std::ffi::OsString::from("-l"),
                std::ffi::OsString::from(&source),
            ])
        });

        assert_eq!(code, 0);
        assert!(
            read_back(dir.path(), "out").starts_with("3 "),
            "{}",
            read_back(dir.path(), "out")
        );
    }
}
