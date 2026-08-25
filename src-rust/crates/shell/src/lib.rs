//! The shell MikMik runs commands in.
//!
//! Every Bash tool call used to spawn `bash -c <script>` and read the state
//! back out of a sentinel block printed at the end. That cost a fork and an
//! exec per call, it lost anything a command changed that the sentinel did not
//! name, and on Windows there was no bash to spawn at all, so the tool ran
//! `cmd /C` under a name that promised otherwise.
//!
//! This crate embeds [`brush`](https://github.com/reubeno/brush), a bash
//! implementation written in Rust, as a library. One [`ShellSession`] lives
//! for as long as the MikMik session does, so `cd`, `export`, aliases,
//! functions and `$?` are the shell's own state rather than something copied
//! forward. The same code runs on macOS, Linux and Windows.
//!
//! What does *not* change: an external program is still a real process. brush
//! removes the shell process and its built-ins from the hot path, not the
//! `cargo` or `git` the model asked for.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use brush_core::ExecutionResult;

mod children;

/// `brush_core::ShellFd` is a plain `i32`, so the three standard descriptors
/// are named here rather than repeated as bare numbers.
const STDIN: brush_core::ShellFd = 0;
const STDOUT: brush_core::ShellFd = 1;
const STDERR: brush_core::ShellFd = 2;

/// A shell that outlives one command.
pub struct ShellSession {
    shell: brush_core::Shell,
}

/// Where a command's output goes.
///
/// Both variants carry a real descriptor, so a program the command starts
/// inherits it. A caller on Unix hands in the slave side of a pty and the
/// programs still see a terminal; a caller on Windows hands in a pipe.
pub enum Sink {
    /// A file or a pty slave.
    File(std::fs::File),
    /// The writing half of a pipe.
    Pipe(std::io::PipeWriter),
}

impl From<std::fs::File> for Sink {
    fn from(file: std::fs::File) -> Self {
        Self::File(file)
    }
}

impl From<std::io::PipeWriter> for Sink {
    fn from(writer: std::io::PipeWriter) -> Self {
        Self::Pipe(writer)
    }
}

impl From<Sink> for brush_core::openfiles::OpenFile {
    fn from(sink: Sink) -> Self {
        match sink {
            Sink::File(file) => Self::from(file),
            Sink::Pipe(writer) => Self::from(writer),
        }
    }
}

/// What one command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// The exit status the shell reported.
    pub exit_code: i32,
    /// Whether the command was still running when its time ran out. The
    /// processes it had started are killed before this is answered.
    pub timed_out: bool,
}

impl ShellSession {
    /// Open a shell rooted at `working_dir`.
    ///
    /// Neither the profile nor the rc files are read. A user's `.bashrc` can
    /// print a banner, define an alias, or block on a prompt, and none of that
    /// belongs in a session the model drives; `bash -c` did not read them
    /// either, so this keeps the behaviour the tool already had.
    pub async fn new(working_dir: &Path) -> anyhow::Result<Self> {
        let shell = brush_core::Shell::builder()
            .working_dir(working_dir.to_path_buf())
            .profile(brush_core::ProfileLoadBehavior::Skip)
            .rc(brush_core::RcLoadBehavior::Skip)
            .interactive(false)
            .no_editing(true)
            .builtins(brush_builtins::default_builtins(
                brush_builtins::BuiltinSet::BashMode,
            ))
            .build()
            .await
            .map_err(|error| anyhow::anyhow!("could not start the shell: {error}"))?;
        Ok(Self { shell })
    }

    /// Where the shell is now, which is wherever the last `cd` left it.
    pub fn working_dir(&self) -> &Path {
        self.shell.working_dir()
    }

    /// The value of one shell variable, if it is set.
    pub fn var(&self, name: &str) -> Option<String> {
        self.shell
            .env()
            .get(name)
            .map(|(_, variable)| variable.value().to_cow_str(&self.shell).to_string())
    }

    /// Run `command`, writing what it prints to `stdout` and `stderr`.
    ///
    /// The two files are handed to the shell as real descriptors, so a program
    /// the command starts inherits them. That is what keeps a PTY working:
    /// pass the slave side here and `cargo`, `npm` and `git` still see a
    /// terminal.
    ///
    /// stdin is `/dev/null`. A command that waits for input in a session the
    /// model drives waits for something that is never coming, and the timeout
    /// is a worse answer than an immediate end of file.
    pub async fn run(
        &mut self,
        command: &str,
        stdout: impl Into<Sink>,
        stderr: impl Into<Sink>,
        timeout: Duration,
    ) -> anyhow::Result<RunOutcome> {
        let mut params = self.shell.default_exec_params();
        // Explicit rather than inherited: `default_exec_params` answers
        // `SameProcessGroup` while job control is off, and killing that group
        // on a timeout would kill this process with it.
        params.process_group_policy = brush_core::ProcessGroupPolicy::NewProcessGroup;
        params.set_fd(
            STDIN,
            brush_core::openfiles::null()
                .map_err(|error| anyhow::anyhow!("could not open the null device: {error}"))?,
        );
        params.set_fd(STDOUT, brush_core::openfiles::OpenFile::from(stdout.into()));
        params.set_fd(STDERR, brush_core::openfiles::OpenFile::from(stderr.into()));

        let source_info = brush_core::SourceInfo::from("mikmik");
        let before = children::direct_children();

        match tokio::time::timeout(
            timeout,
            self.shell.run_string(command, &source_info, &params),
        )
        .await
        {
            Ok(Ok(result)) => Ok(RunOutcome {
                exit_code: i32::from(u8::from(result.exit_code)),
                timed_out: false,
            }),
            Ok(Err(error)) => Err(anyhow::anyhow!("{error}")),
            Err(_elapsed) => {
                // The future is dropped, which stops the shell waiting, but
                // the process it started is not ours to leave running.
                children::kill_new_since(&before);
                Ok(RunOutcome {
                    exit_code: 124,
                    timed_out: true,
                })
            }
        }
    }
}

impl std::fmt::Debug for ShellSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellSession")
            .field("working_dir", &self.working_dir())
            .finish()
    }
}

/// A path the shell should start in, falling back to the current directory.
///
/// A working directory that has been deleted underneath the session would stop
/// the shell from opening at all, and a session that cannot run anything is a
/// worse answer than one rooted somewhere else.
pub fn usable_working_dir(preferred: &Path) -> PathBuf {
    if preferred.is_dir() {
        return preferred.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run one command and give back what it printed plus its outcome.
    async fn run(session: &mut ShellSession, command: &str) -> (String, RunOutcome) {
        run_with_timeout(session, command, Duration::from_secs(30)).await
    }

    async fn run_with_timeout(
        session: &mut ShellSession,
        command: &str,
        timeout: Duration,
    ) -> (String, RunOutcome) {
        let out = tempfile::NamedTempFile::new().expect("temp file");
        let err = tempfile::NamedTempFile::new().expect("temp file");
        let outcome = session
            .run(
                command,
                out.reopen().expect("reopen"),
                err.reopen().expect("reopen"),
                timeout,
            )
            .await
            .expect("the shell ran");
        let mut text = std::fs::read_to_string(out.path()).expect("read stdout");
        text.push_str(&std::fs::read_to_string(err.path()).expect("read stderr"));
        (text, outcome)
    }

    async fn session() -> (tempfile::TempDir, ShellSession) {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = ShellSession::new(dir.path()).await.expect("shell");
        (dir, session)
    }

    #[tokio::test]
    async fn a_command_prints_what_it_prints() {
        let (_dir, mut shell) = session().await;
        let (text, outcome) = run(&mut shell, "echo hello").await;

        assert_eq!(text.trim(), "hello");
        assert_eq!(outcome.exit_code, 0);
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn a_directory_change_outlives_the_command_that_made_it() {
        // This is the whole reason the shell is held open. The old tool read
        // the directory back out of a sentinel block and replayed it into the
        // next child.
        let (dir, mut shell) = session().await;
        std::fs::create_dir(dir.path().join("inner")).expect("mkdir");

        let (_, first) = run(&mut shell, "cd inner").await;
        assert_eq!(first.exit_code, 0);
        let (text, _) = run(&mut shell, "pwd").await;

        assert!(text.trim().ends_with("inner"), "{text}");
        assert!(shell.working_dir().ends_with("inner"));
    }

    #[tokio::test]
    async fn an_exported_variable_outlives_the_command_that_set_it() {
        let (_dir, mut shell) = session().await;

        run(&mut shell, "export GREETING=hello").await;
        let (text, _) = run(&mut shell, "echo $GREETING").await;

        assert_eq!(text.trim(), "hello");
        assert_eq!(shell.var("GREETING").as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn a_shell_function_outlives_the_command_that_defined_it() {
        // A sentinel block could never have carried this one at all.
        let (_dir, mut shell) = session().await;

        run(&mut shell, "greet() { echo hi from a function; }").await;
        let (text, outcome) = run(&mut shell, "greet").await;

        assert_eq!(text.trim(), "hi from a function");
        assert_eq!(outcome.exit_code, 0);
    }

    #[tokio::test]
    async fn an_exit_status_is_reported() {
        let (_dir, mut shell) = session().await;
        let (_, outcome) = run(&mut shell, "exit 3").await;

        assert_eq!(outcome.exit_code, 3);
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_status_without_ending_the_session() {
        let (_dir, mut shell) = session().await;

        let (_, failed) = run(&mut shell, "false").await;
        assert_eq!(failed.exit_code, 1);

        let (text, after) = run(&mut shell, "echo still here").await;
        assert_eq!(text.trim(), "still here");
        assert_eq!(after.exit_code, 0);
    }

    #[tokio::test]
    async fn a_pipeline_runs_end_to_end() {
        let (_dir, mut shell) = session().await;
        let (text, outcome) = run(&mut shell, "printf 'b\\na\\n' | sort | head -1").await;

        assert_eq!(text.trim(), "a");
        assert_eq!(outcome.exit_code, 0);
    }

    #[tokio::test]
    async fn a_redirection_writes_the_file_it_names() {
        let (dir, mut shell) = session().await;
        let target = dir.path().join("written.txt");

        run(&mut shell, &format!("echo written > {}", target.display())).await;

        assert_eq!(
            std::fs::read_to_string(&target).expect("read").trim(),
            "written"
        );
    }

    #[tokio::test]
    async fn stderr_and_stdout_arrive_separately() {
        let (_dir, mut shell) = session().await;
        let out = tempfile::NamedTempFile::new().expect("temp file");
        let err = tempfile::NamedTempFile::new().expect("temp file");

        shell
            .run(
                "echo to-stdout; echo to-stderr 1>&2",
                out.reopen().expect("reopen"),
                err.reopen().expect("reopen"),
                Duration::from_secs(30),
            )
            .await
            .expect("ran");

        let stdout = std::fs::read_to_string(out.path()).expect("read");
        let stderr = std::fs::read_to_string(err.path()).expect("read");
        assert_eq!(stdout.trim(), "to-stdout");
        assert_eq!(stderr.trim(), "to-stderr");
    }

    #[tokio::test]
    async fn a_command_that_waits_for_input_reads_end_of_file() {
        // stdin is the null device, so a command that reads gets nothing and
        // finishes instead of holding the session until the timeout.
        let (_dir, mut shell) = session().await;
        let (_, outcome) =
            run_with_timeout(&mut shell, "read line; echo done", Duration::from_secs(10)).await;

        assert!(
            !outcome.timed_out,
            "the shell waited for input nobody sends"
        );
    }

    #[tokio::test]
    async fn a_command_that_runs_too_long_is_stopped() {
        // Reported *and* killed. The shell runs in this process, so dropping
        // the future stops the waiting and nothing else; without the kill the
        // `sleep` would outlive the call that started it.
        let (_dir, mut shell) = session().await;
        let before = children::direct_children();

        let (_, outcome) =
            run_with_timeout(&mut shell, "sleep 30", Duration::from_millis(500)).await;

        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, 124);

        // The kill is not awaited, so give the signal a moment to land.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = children::direct_children();
        let left: Vec<&u32> = after.difference(&before).collect();
        assert!(
            left.is_empty(),
            "the timed-out command left {left:?} running"
        );
    }

    #[tokio::test]
    async fn the_session_still_works_after_a_timeout() {
        let (_dir, mut shell) = session().await;
        run_with_timeout(&mut shell, "sleep 30", Duration::from_millis(500)).await;

        let (text, outcome) = run(&mut shell, "echo alive").await;
        assert_eq!(text.trim(), "alive");
        assert_eq!(outcome.exit_code, 0);
    }

    #[tokio::test]
    async fn a_missing_working_directory_falls_back_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("never-created");

        assert_eq!(usable_working_dir(dir.path()), dir.path());
        assert_ne!(usable_working_dir(&gone), gone);
    }
}
