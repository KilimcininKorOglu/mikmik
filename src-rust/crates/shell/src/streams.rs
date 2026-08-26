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

/// Run one utility called `name` with the three streams installed for this
/// thread.
///
/// The previous state comes back afterwards, on a panic as well, so one
/// utility can run inside another's pipeline.
///
/// `name` is what the utility prints its complaints under, and it also clears
/// the exit code the previous utility on this thread left behind. Both used to
/// come from the utility being a process of its own.
pub fn with_streams<T>(name: &str, streams: Streams, body: impl FnOnce() -> T) -> T {
    uucore::streams::with_streams(name, streams.stdin, streams.stdout, streams.stderr, body)
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

        with_streams("probe", three(dir.path(), "out"), || {
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

        with_streams("probe", three(dir.path(), "out"), || {
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

        let (first, rest) = with_streams("probe", streams, || {
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

        with_streams("probe", three(dir.path(), "outer"), || {
            write!(uucore::streams::stdout(), "outer ").expect("write");
            with_streams(
                "probe",
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
            with_streams("probe", three(dir.path(), "out"), || panic!("on purpose"));
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
        let code = with_streams("sort", three(dir.path(), "out"), || {
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

    /// Run a bundled utility with the three streams pointing into `dir`, and
    /// answer its exit code together with what it wrote to stderr.
    fn complain(dir: &std::path::Path, util: &str, args: &[&str]) -> (i32, String) {
        let mut argv = vec![std::ffi::OsString::from(util)];
        argv.extend(args.iter().map(std::ffi::OsString::from));

        let code = with_streams(util, three(dir, "out"), || {
            let run = crate::bundled::registry().get(util).expect("bundled");
            run(argv)
        });
        (code, read_back(dir, "err"))
    }

    #[cfg(unix)]
    #[test]
    fn a_utility_leaves_the_hosts_signals_alone() {
        // Every `uumain` sets SIGPIPE, SIGSEGV and SIGBUS back to their
        // defaults on the way in, because a standalone utility is the whole
        // process and dying on a broken pipe is what `seq inf | head -1`
        // needs. Here the process is MikMik: one `ls` would leave every later
        // write in it able to kill it, and would take away the message a
        // stack overflow prints.
        fn defaulted(signal: i32) -> bool {
            let mut current = std::mem::MaybeUninit::<libc::sigaction>::uninit();
            // SAFETY: querying with a null new-action only reads the current
            // disposition; nothing is installed.
            let queried =
                unsafe { libc::sigaction(signal, std::ptr::null(), current.as_mut_ptr()) };
            if queried != 0 {
                return false;
            }
            // SAFETY: the query succeeded, so the value is initialised.
            unsafe { current.assume_init() }.sa_sigaction == libc::SIG_DFL
        }

        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!defaulted(libc::SIGPIPE), "SIGPIPE was already the default");

        let code = with_streams("ls", three(dir.path(), "out"), || {
            let run = crate::bundled::registry().get("ls").expect("ls is bundled");
            run(vec![
                std::ffi::OsString::from("ls"),
                std::ffi::OsString::from(dir.path().display().to_string()),
            ])
        });

        assert_eq!(code, 0);
        assert!(!defaulted(libc::SIGPIPE), "`ls` changed SIGPIPE");
        assert!(!defaulted(libc::SIGSEGV), "`ls` changed SIGSEGV");
        assert!(!defaulted(libc::SIGBUS), "`ls` changed SIGBUS");
    }

    #[test]
    fn one_utilitys_failure_is_not_the_next_ones_answer() {
        // Upstream keeps the exit code in a process-wide static, because a
        // utility used to be a process of its own. Here they share one, and a
        // stale 1 would make every later command in the session look failed.
        // `ls` reports a missing name by setting the code and answering `Ok`,
        // which is the shape that leaks: the next utility answering `Ok` picks
        // the same code up and looks failed.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("source"), "one\ntwo\n").expect("write");
        let source = dir.path().join("source").display().to_string();

        let (failed, _) = complain(dir.path(), "ls", &["no-such-file"]);
        assert_ne!(failed, 0);

        let (after, _) = complain(dir.path(), "ls", &[&source]);
        assert_eq!(after, 0);
    }

    #[test]
    fn the_most_written_command_writes_where_it_was_told() {
        // `ls` kept its own `use std::io::stdout`, so its listing went to the
        // process's real output while the shell read an empty file. It is the
        // command a model writes most, and the one whose escape was silent.
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory of its own, so the scratch files the streams point at do
        // not turn up in the listing.
        std::fs::create_dir(dir.path().join("listed")).expect("mkdir");
        std::fs::write(dir.path().join("listed/only-file"), "x").expect("write");
        let listed = dir.path().join("listed").display().to_string();

        let code = with_streams("ls", three(dir.path(), "out"), || {
            let run = crate::bundled::registry().get("ls").expect("ls is bundled");
            run(vec![
                std::ffi::OsString::from("ls"),
                std::ffi::OsString::from(&listed),
            ])
        });

        assert_eq!(code, 0);
        assert_eq!(read_back(dir.path(), "out").trim(), "only-file");
    }

    #[test]
    fn a_utility_complains_under_its_own_name() {
        // Upstream reads the name out of `argv[0]`, which in this process is
        // the host's binary. Without the installed name every message would
        // start `mikmik:`.
        let dir = tempfile::tempdir().expect("tempdir");

        let (_, complaint) = complain(dir.path(), "cat", &["no-such-file"]);

        assert!(complaint.starts_with("cat: "), "{complaint}");
    }

    #[test]
    fn a_utility_gets_its_own_messages_rather_than_the_previous_ones() {
        // The localizer used to be set once per thread, so the second utility
        // on a thread looked its messages up in the first one's bundle and
        // printed raw keys such as `head-error-cannot-open`.
        let dir = tempfile::tempdir().expect("tempdir");

        complain(dir.path(), "sort", &["no-such-file"]);
        let (_, complaint) = complain(dir.path(), "head", &["no-such-file"]);

        assert!(
            complaint.contains("cannot open"),
            "head printed a raw message key: {complaint}"
        );
    }

    #[test]
    fn a_utility_that_counts_reports_through_the_same_path() {
        // `wc` takes the locked handle rather than the plain one, which is a
        // separate route through the fork.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("source"), "one\ntwo\nthree\n").expect("write");
        let source = dir.path().join("source").display().to_string();

        let code = with_streams("wc", three(dir.path(), "out"), || {
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
