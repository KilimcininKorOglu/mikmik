//! Running a Bash tool call in the background, in a shell of its own.
//!
//! A background command used to be `bash -c <script>` on Unix and `cmd /C` on
//! Windows. That gave Windows no pipelines at all, and it gave the command
//! none of the session's state: a `cd` or an `export` the model had made in
//! the foreground was not there.
//!
//! It runs in the embedded shell now, in a **second** [`ShellSession`] seeded
//! from the foreground one's working directory and exported variables. A
//! session of its own rather than the foreground one, for two reasons.
//!
//! It is what bash does. `command &` runs in a subshell, so what the command
//! changes does not come back; a background `cd` that moved the foreground
//! session would be a surprise nobody asked for.
//!
//! And it is what lets a background command outlive the foreground one. The
//! foreground session is behind a mutex that a running command holds, so
//! sharing it would mean a background command could only run while nothing
//! else did.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mikmik_core::tasks::{global_registry, BackgroundTask, TaskStatus};

/// Start `command` in the background and answer the task's id.
///
/// Everything happens inside the task: reading the seed, opening the shell,
/// running the command. The caller is told the id straight away.
///
/// Reading the seed takes the foreground session's lock, which a running
/// foreground command holds, so a background command started while one is in
/// flight begins when that one ends. The tool call itself does not wait, and
/// neither does `monitor`, which reads the task registry rather than the
/// shell. Copying the seed after every foreground command instead would cost
/// 21 us on each of them, against 164 us for a whole bundled `ls`.
pub(crate) fn run(
    command: String,
    session_id: &str,
    working_dir: &Path,
    bundled: mikmik_core::config::BundledUtilities,
    timeout: Duration,
) -> String {
    let name = format!("bg: {}", &command[..command.len().min(60)]);
    let mut task = BackgroundTask::new(&name);
    // No process id: the shell is this process, and what it starts is its own
    // to report.
    task.pid = None;
    let id = global_registry().register(task);

    // What `monitor cancel` reaches. The command's shell is this process, so
    // there is no pid to signal; the token is how a cancel stops the command
    // and kills whatever it started.
    let cancel = tokio_util::sync::CancellationToken::new();
    global_registry().set_cancel_token(&id, cancel.clone());

    tokio::spawn({
        let id = id.clone();
        let session_id = session_id.to_string();
        let working_dir = working_dir.to_path_buf();
        async move {
            let outcome = match seed(&session_id, &working_dir, bundled).await {
                Ok((directory, environment)) => {
                    carry_out(
                        &command,
                        &directory,
                        environment,
                        bundled,
                        timeout,
                        &id,
                        &cancel,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            report(&id, outcome, timeout);
        }
    });

    id
}

/// What the background shell starts from: the foreground session's directory
/// and the variables it would hand to a child.
async fn seed(
    session_id: &str,
    working_dir: &Path,
    bundled: mikmik_core::config::BundledUtilities,
) -> anyhow::Result<(PathBuf, Vec<(String, String)>)> {
    let foreground = crate::session_brush_shell(session_id, working_dir, bundled).await?;
    let foreground = foreground.lock().await;
    Ok((
        foreground.working_dir().to_path_buf(),
        foreground.exported_env(),
    ))
}

/// Open the command's own shell, run it, and answer what it did.
async fn carry_out(
    command: &str,
    directory: &Path,
    environment: Vec<(String, String)>,
    bundled: mikmik_core::config::BundledUtilities,
    timeout: Duration,
    id: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<mikmik_shell::RunOutcome> {
    let mut shell = mikmik_shell::ShellSession::new(
        &mikmik_shell::usable_working_dir(directory),
        crate::bundled_policy(bundled),
    )
    .await?;
    for (name, value) in environment {
        shell.export(&name, &value)?;
    }

    // A pipe rather than a pty: nothing here is a terminal, and the old
    // background path used a pipe too.
    let (reader, writer) = std::io::pipe()?;
    let errors = writer.try_clone()?;
    let stop = Arc::new(AtomicBool::new(false));
    let draining = drain(reader, id.to_string(), stop.clone());

    let outcome = shell
        .run_cancellable(command, writer, errors, timeout, cancel)
        .await;

    // The shell's copies go with it, which ends the stream in the ordinary
    // case. The flag is for the other one: a command that deliberately left a
    // process running has handed it a copy of the pipe, and a reader waiting
    // on that would never come back.
    drop(shell);
    stop.store(true, Ordering::Relaxed);
    let joined = tokio::task::spawn_blocking(move || draining.join()).await;
    let _ = joined;

    outcome
}

/// Read the command's output line by line into the task, as it arrives.
///
/// Line by line rather than in one piece at the end, because `monitor` is what
/// a model reads while the command is still running.
fn drain(
    reader: std::io::PipeReader,
    id: String,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut source = BufReader::new(reader);
        let mut line = String::new();
        loop {
            #[cfg(unix)]
            if !crate::brush_bash::readable(source.get_ref(), POLL_MS) {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                continue;
            }
            line.clear();
            match source.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => global_registry().append_output(&id, line.trim_end_matches(['\r', '\n'])),
            }
        }
    })
}

/// How long one poll waits before the stop flag is looked at again.
#[cfg(unix)]
const POLL_MS: i32 = 50;

/// Record how the command ended.
fn report(id: &str, outcome: anyhow::Result<mikmik_shell::RunOutcome>, timeout: Duration) {
    // A cancelled task is already recorded as such by the registry, and this
    // would otherwise overwrite that with the 130 the shell answered.
    if matches!(
        global_registry().get(id).map(|task| task.status),
        Some(TaskStatus::Cancelled)
    ) {
        return;
    }

    let status = match outcome {
        Ok(outcome) if outcome.timed_out => {
            TaskStatus::Failed(format!("timed out after {}ms", timeout.as_millis()))
        }
        Ok(outcome) if outcome.exit_code == 0 => TaskStatus::Completed,
        Ok(outcome) => TaskStatus::Failed(format!("exit code {}", outcome.exit_code)),
        Err(error) => TaskStatus::Failed(error.to_string()),
    };
    global_registry().update_status(id, status);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait for a task to reach a terminal status, or give up.
    async fn settled(id: &str, limit: Duration) -> BackgroundTask {
        let started = std::time::Instant::now();
        loop {
            let task = global_registry().get(id).expect("task");
            if task.status != TaskStatus::Running || started.elapsed() > limit {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn session_id() -> String {
        format!("bg-test-{}", uuid::Uuid::new_v4())
    }

    #[tokio::test]
    async fn a_background_command_sees_what_the_foreground_exported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = session_id();
        let bundled = mikmik_core::config::BundledUtilities::Prefer;

        {
            let foreground = crate::session_brush_shell(&session, dir.path(), bundled)
                .await
                .expect("shell");
            let mut foreground = foreground.lock().await;
            foreground
                .export("FROM_FOREGROUND", "carried")
                .expect("export");
        }

        let id = run(
            "echo $FROM_FOREGROUND".to_string(),
            &session,
            dir.path(),
            bundled,
            Duration::from_secs(10),
        );

        let task = settled(&id, Duration::from_secs(10)).await;
        assert_eq!(task.status, TaskStatus::Completed, "{:?}", task.output);
        assert_eq!(task.output, vec!["carried".to_string()]);
        crate::clear_session_shell_state(&session);
    }

    #[tokio::test]
    async fn a_background_directory_change_does_not_move_the_foreground() {
        // `command &` runs in a subshell in bash, and a background `cd` that
        // moved the session would be a surprise nobody asked for.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("elsewhere")).expect("mkdir");
        let session = session_id();
        let bundled = mikmik_core::config::BundledUtilities::Prefer;

        let before = {
            let foreground = crate::session_brush_shell(&session, dir.path(), bundled)
                .await
                .expect("shell");
            let foreground = foreground.lock().await;
            foreground.working_dir().to_path_buf()
        };

        let id = run(
            "cd elsewhere && pwd".to_string(),
            &session,
            dir.path(),
            bundled,
            Duration::from_secs(10),
        );

        let task = settled(&id, Duration::from_secs(10)).await;
        assert_eq!(task.status, TaskStatus::Completed, "{:?}", task.output);
        assert!(
            task.output.iter().any(|line| line.ends_with("elsewhere")),
            "{:?}",
            task.output
        );

        let foreground = crate::session_brush_shell(&session, dir.path(), bundled)
            .await
            .expect("shell");
        let foreground = foreground.lock().await;
        assert_eq!(foreground.working_dir(), before);
        drop(foreground);
        crate::clear_session_shell_state(&session);
    }

    #[tokio::test]
    async fn a_background_pipeline_runs_on_every_platform() {
        // The old Windows path was `cmd /C`, which fails on the first
        // pipeline the model writes. This machine cannot tell the two apart,
        // because `bash -c` handles a pipeline too; what it pins is that the
        // command reaches a shell that does.
        let dir = tempfile::tempdir().expect("tempdir");
        let session = session_id();

        let id = run(
            "printf 'b\\na\\n' | sort | head -1".to_string(),
            &session,
            dir.path(),
            mikmik_core::config::BundledUtilities::Prefer,
            Duration::from_secs(10),
        );

        let task = settled(&id, Duration::from_secs(10)).await;
        assert_eq!(task.status, TaskStatus::Completed, "{:?}", task.output);
        assert_eq!(task.output, vec!["a".to_string()]);
        crate::clear_session_shell_state(&session);
    }

    #[tokio::test]
    async fn a_failing_background_command_says_what_it_answered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = session_id();

        let id = run(
            "exit 3".to_string(),
            &session,
            dir.path(),
            mikmik_core::config::BundledUtilities::Prefer,
            Duration::from_secs(10),
        );

        let task = settled(&id, Duration::from_secs(10)).await;
        assert_eq!(task.status, TaskStatus::Failed("exit code 3".to_string()));
        crate::clear_session_shell_state(&session);
    }

    #[tokio::test]
    async fn output_reaches_the_task_while_the_command_is_still_running() {
        // `monitor` is read while the command runs, so a line has to arrive
        // before the command ends rather than in one piece afterwards.
        let dir = tempfile::tempdir().expect("tempdir");
        let session = session_id();

        let id = run(
            "echo first; /bin/sleep 2; echo last".to_string(),
            &session,
            dir.path(),
            mikmik_core::config::BundledUtilities::Prefer,
            Duration::from_secs(20),
        );

        let mut seen_early = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let task = global_registry().get(&id).expect("task");
            if task.status == TaskStatus::Running && task.output == vec!["first".to_string()] {
                seen_early = true;
                break;
            }
        }
        assert!(seen_early, "no output arrived before the command ended");

        let task = settled(&id, Duration::from_secs(20)).await;
        assert_eq!(task.status, TaskStatus::Completed, "{:?}", task.output);
        crate::clear_session_shell_state(&session);
    }

    /// Whether any process's command line still holds `marker`.
    #[cfg(not(windows))]
    fn still_running(marker: &str) -> bool {
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .map(|out| !out.stdout.is_empty())
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_timed_out_background_command_takes_its_children_with_it() {
        // The shell is this process and cannot be killed, so what the timeout
        // has to reach is the process the command started. `/bin/sleep` is
        // spelled out because the carried `sleep` starts no process at all.
        let dir = tempfile::tempdir().expect("tempdir");
        let session = session_id();
        // A duration no other run can be using: a fixed one made the test read
        // an earlier run's leftover as this run's.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let marker = format!("999337.{}", nanos % 1_000_000_000);

        let id = run(
            format!("/bin/sleep {marker}"),
            &session,
            dir.path(),
            mikmik_core::config::BundledUtilities::Prefer,
            Duration::from_millis(500),
        );

        let task = settled(&id, Duration::from_secs(10)).await;
        assert!(
            matches!(task.status, TaskStatus::Failed(ref why) if why.contains("timed out")),
            "{:?}",
            task.status
        );

        // Give the signal a moment to land.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !still_running(&marker),
            "the shell's child outlived the timeout"
        );
        crate::clear_session_shell_state(&session);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_command_that_leaves_a_process_behind_still_reaches_an_end() {
        // The process it left holds a copy of the pipe, so a reader waiting
        // for the end of the stream would never come back and the task would
        // stay `Running` for as long as that process lived.
        let dir = tempfile::tempdir().expect("tempdir");
        let session = session_id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let marker = format!("999336.{}", nanos % 1_000_000_000);

        let id = run(
            format!("/bin/sleep {marker} & echo started"),
            &session,
            dir.path(),
            mikmik_core::config::BundledUtilities::Prefer,
            Duration::from_secs(20),
        );

        let task = settled(&id, Duration::from_secs(10)).await;
        assert_eq!(task.status, TaskStatus::Completed, "{:?}", task.output);
        assert_eq!(task.output, vec!["started".to_string()]);

        // Tidy up: the process was deliberately left running.
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(&marker)
            .status();
        crate::clear_session_shell_state(&session);
    }

    #[tokio::test]
    async fn starting_one_does_not_wait_for_the_command_to_run() {
        // `run_in_background` answers with an id, not with a result. Opening a
        // shell and seeding it takes longer than the caller should ever wait.
        let dir = tempfile::tempdir().expect("tempdir");
        let session = session_id();

        let started = std::time::Instant::now();
        let id = run(
            "/bin/sleep 3".to_string(),
            &session,
            dir.path(),
            mikmik_core::config::BundledUtilities::Prefer,
            Duration::from_secs(20),
        );
        let answered = started.elapsed();

        assert!(
            answered < Duration::from_millis(50),
            "the caller waited {answered:?}"
        );
        assert_eq!(
            global_registry().get(&id).expect("task").status,
            TaskStatus::Running
        );
        crate::clear_session_shell_state(&session);
    }

    #[tokio::test]
    async fn a_background_command_that_runs_too_long_is_reported_as_such() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = session_id();

        let id = run(
            "/bin/sleep 30".to_string(),
            &session,
            dir.path(),
            mikmik_core::config::BundledUtilities::Prefer,
            Duration::from_millis(300),
        );

        let task = settled(&id, Duration::from_secs(10)).await;
        assert_eq!(
            task.status,
            TaskStatus::Failed("timed out after 300ms".to_string())
        );
        crate::clear_session_shell_state(&session);
    }
}
