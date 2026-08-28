//! The `hub` tool: supervise long-lived processes.
//!
//! A named process is started, watched, read, stopped, restarted, and written
//! to. The name is the handle: it persists for the process's life and maps to a
//! `TaskRegistry` entry so the rest of the app sees the process too. Output is
//! streamed into a bounded buffer by a reader task, so `logs` never blocks on
//! the process and a chatty process cannot grow without limit.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};

/// The most log lines one process keeps. Old lines fall off the front, so a
/// process that runs for days costs a bounded amount of memory.
const MAX_LOG_LINES: usize = 5000;
/// How long `start` waits for a `ready` marker before giving up on it.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// What a process was started from, kept so `restart` can repeat it exactly.
#[derive(Debug, Clone)]
struct Spec {
    application: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
}

/// One supervised process: its spec, its live handles, and its logs.
struct Supervised {
    spec: Spec,
    pid: Option<u32>,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    logs: Arc<Mutex<Vec<String>>>,
    task_id: String,
}

/// Supervised processes by name, shared across every call in the process.
static PROCS: Lazy<DashMap<String, Supervised>> = Lazy::new(DashMap::new);

pub struct HubTool;

#[derive(Debug, Deserialize)]
struct HubInput {
    /// One of start, ps, logs, stop, restart, send.
    op: String,
    /// The process name most ops act on.
    #[serde(default)]
    name: Option<String>,
    /// start: the program to run.
    #[serde(default)]
    application: Option<String>,
    /// start: its arguments.
    #[serde(default)]
    args: Option<Vec<String>>,
    /// start: environment variables to add, as `["KEY=value", ...]`.
    #[serde(default)]
    env: Option<Vec<String>>,
    /// start: the working directory.
    #[serde(default)]
    cwd: Option<String>,
    /// start: a substring to wait for in the output before returning.
    #[serde(default)]
    ready: Option<String>,
    /// logs: return only the last N lines.
    #[serde(default)]
    lines: Option<usize>,
    /// logs: return the first N lines instead of the last.
    #[serde(default)]
    head: Option<usize>,
    /// logs: return only lines containing this substring.
    #[serde(default)]
    grep: Option<String>,
    /// logs: skip this many lines from the start of the window.
    #[serde(default)]
    cursor: Option<usize>,
    /// send: a line to write to the process's stdin.
    #[serde(default)]
    input: Option<String>,
    /// send: a signal name (unix), for example `TERM` or `HUP`.
    #[serde(default)]
    signal: Option<String>,
}

#[async_trait]
impl Tool for HubTool {
    fn name(&self) -> &str {
        "hub"
    }

    fn description(&self) -> &str {
        "Supervise long-lived processes by name. Ops:\n\
         - start: run a named process (application, args, env, cwd, ready).\n\
         - ps: list supervised processes and whether each is still running.\n\
         - logs: a window of a process's output (lines, head, grep, cursor).\n\
         - stop: stop a process and its tree.\n\
         - restart: stop and start again with the same spec, giving a new pid.\n\
         - send: write a line to a process's stdin, or send it a signal."
    }

    fn permission_level(&self) -> PermissionLevel {
        // Starts and signals arbitrary processes; the same level as the shell.
        PermissionLevel::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["start", "ps", "logs", "stop", "restart", "send"],
                    "description": "Which supervision action to take."
                },
                "name": { "type": "string", "description": "The process name most ops act on." },
                "application": { "type": "string", "description": "start: the program to run." },
                "args": { "type": "array", "items": { "type": "string" }, "description": "start: its arguments." },
                "env": { "type": "array", "items": { "type": "string" }, "description": "start: env additions as KEY=value." },
                "cwd": { "type": "string", "description": "start: the working directory." },
                "ready": { "type": "string", "description": "start: wait for this substring in the output before returning." },
                "lines": { "type": "number", "description": "logs: return only the last N lines." },
                "head": { "type": "number", "description": "logs: return the first N lines instead of the last." },
                "grep": { "type": "string", "description": "logs: only lines containing this substring." },
                "cursor": { "type": "number", "description": "logs: skip this many lines from the start of the window." },
                "input": { "type": "string", "description": "send: a line to write to stdin." },
                "signal": { "type": "string", "description": "send: a signal name such as TERM or HUP (unix)." }
            },
            "required": ["op"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: HubInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        match params.op.as_str() {
            "start" => op_start(params).await,
            "ps" => op_ps().await,
            "logs" => op_logs(params).await,
            "stop" => op_stop(params).await,
            "restart" => op_restart(params).await,
            "send" => op_send(params).await,
            other => ToolResult::error(format!(
                "unknown op {other:?}; use start, ps, logs, stop, restart or send"
            )),
        }
    }
}

/// Start a named process and, when asked, wait for it to announce readiness.
async fn op_start(params: HubInput) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("start needs a name".to_string());
    };
    if is_running(&name).await {
        return ToolResult::error(format!("a process named {name:?} is already running"));
    }
    let Some(application) = params.application.clone() else {
        return ToolResult::error("start needs an application to run".to_string());
    };
    let spec = Spec {
        application,
        args: params.args.clone().unwrap_or_default(),
        env: parse_env(params.env.as_deref()),
        cwd: params.cwd.clone(),
    };

    match spawn(&name, spec).await {
        Ok(pid) => {
            if let Some(marker) = params.ready.as_deref() {
                if !wait_for_marker(&name, marker).await {
                    return ToolResult::error(format!(
                        "process {name:?} (pid {pid}) did not print {marker:?} within the timeout"
                    ));
                }
            }
            ToolResult::success(format!("Started {name:?} (pid {pid})."))
        }
        Err(error) => ToolResult::error(error),
    }
}

/// List supervised processes and whether each is still running.
async fn op_ps() -> ToolResult {
    let names: Vec<String> = PROCS.iter().map(|entry| entry.key().clone()).collect();
    if names.is_empty() {
        return ToolResult::success("No supervised processes.".to_string());
    }
    let mut lines = Vec::new();
    for name in names {
        let running = is_running(&name).await;
        let pid = PROCS.get(&name).and_then(|entry| entry.pid);
        let state = if running { "running" } else { "exited" };
        lines.push(format!(
            "{name}\t{state}\tpid {}",
            pid.map(|pid| pid.to_string())
                .unwrap_or_else(|| "?".to_string())
        ));
    }
    ToolResult::success(lines.join("\n"))
}

/// Return a window of a process's output.
async fn op_logs(params: HubInput) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("logs needs a name".to_string());
    };
    let Some(logs) = PROCS.get(&name).map(|entry| entry.logs.clone()) else {
        return ToolResult::error(format!("no process named {name:?}"));
    };
    let snapshot = logs.lock().await.clone();
    let window = log_window(
        &snapshot,
        params.head,
        params.lines,
        params.grep.as_deref(),
        params.cursor,
    );
    if window.is_empty() {
        ToolResult::success("(no matching output)".to_string())
    } else {
        ToolResult::success(window.join("\n"))
    }
}

/// Stop a process and its tree.
async fn op_stop(params: HubInput) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("stop needs a name".to_string());
    };
    if stop_process(&name).await {
        ToolResult::success(format!("Stopped {name:?}."))
    } else {
        ToolResult::error(format!("no process named {name:?}"))
    }
}

/// Stop a process and start it again from the same spec.
async fn op_restart(params: HubInput) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("restart needs a name".to_string());
    };
    let Some(spec) = PROCS.get(&name).map(|entry| entry.spec.clone()) else {
        return ToolResult::error(format!("no process named {name:?}"));
    };
    stop_process(&name).await;
    match spawn(&name, spec).await {
        Ok(pid) => ToolResult::success(format!("Restarted {name:?} (pid {pid}).")),
        Err(error) => ToolResult::error(error),
    }
}

/// Write to a process's stdin, or send it a signal.
async fn op_send(params: HubInput) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("send needs a name".to_string());
    };
    if let Some(signal) = params.signal.as_deref() {
        return send_signal(&name, signal).await;
    }
    let Some(line) = params.input.as_deref() else {
        return ToolResult::error("send needs `input` to write or a `signal` to send".to_string());
    };
    let Some(stdin) = PROCS.get(&name).map(|entry| entry.stdin.clone()) else {
        return ToolResult::error(format!("no process named {name:?}"));
    };
    let mut guard = stdin.lock().await;
    let Some(pipe) = guard.as_mut() else {
        return ToolResult::error(format!("process {name:?} has no open stdin"));
    };
    let payload = format!("{line}\n");
    match pipe.write_all(payload.as_bytes()).await {
        Ok(()) => {
            let _ = pipe.flush().await;
            ToolResult::success(format!("Wrote to {name:?}."))
        }
        Err(error) => ToolResult::error(format!("write to {name:?} failed: {error}")),
    }
}

/// Turn `["KEY=value", ...]` into pairs, dropping entries with no `=`.
fn parse_env(env: Option<&[String]>) -> Vec<(String, String)> {
    env.unwrap_or_default()
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// The window `logs` returns, applied purely to a snapshot of the lines.
///
/// `cursor` skips from the front, `grep` keeps only matching lines, then either
/// `head` takes the first N or `lines` takes the last N. Applied in that order
/// so a grep-then-tail reads the last matches, which is what a log tail wants.
fn log_window(
    lines: &[String],
    head: Option<usize>,
    tail: Option<usize>,
    grep: Option<&str>,
    cursor: Option<usize>,
) -> Vec<String> {
    let skipped = lines.iter().skip(cursor.unwrap_or(0));
    let matched: Vec<&String> = match grep {
        Some(needle) => skipped.filter(|line| line.contains(needle)).collect(),
        None => skipped.collect(),
    };
    let windowed: Vec<&String> = match (head, tail) {
        (Some(n), _) => matched.into_iter().take(n).collect(),
        (None, Some(n)) => {
            let start = matched.len().saturating_sub(n);
            matched.into_iter().skip(start).collect()
        }
        (None, None) => matched,
    };
    windowed.into_iter().cloned().collect()
}

/// Whether the named process is still running (its child has not exited).
async fn is_running(name: &str) -> bool {
    let Some(child) = PROCS.get(name).map(|entry| entry.child.clone()) else {
        return false;
    };
    let mut guard = child.lock().await;
    matches!(guard.try_wait(), Ok(None))
}

/// Spawn the process, wire its streams to the log buffer, and register it.
async fn spawn(name: &str, spec: Spec) -> Result<u32, String> {
    let mut builder = Command::new(&spec.application);
    builder
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &spec.cwd {
        builder.current_dir(cwd);
    }
    for (key, value) in &spec.env {
        builder.env(key, value);
    }
    // Its own group, so stopping the process reaches what it started.
    mikmik_core::process_tree::spawn_in_own_group(&mut builder);

    let mut child = builder
        .spawn()
        .map_err(|error| format!("failed to start {:?}: {error}", spec.application))?;
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take();
    let logs = Arc::new(Mutex::new(Vec::new()));
    if let Some(stdout) = child.stdout.take() {
        stream_into(logs.clone(), stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        stream_into(logs.clone(), stderr);
    }

    let task_id = mikmik_core::tasks::global_registry().register(
        mikmik_core::tasks::BackgroundTask::new(format!("hub:{name}")),
    );
    if pid != 0 {
        mikmik_core::tasks::global_registry().set_pid(&task_id, pid);
    }

    PROCS.insert(
        name.to_string(),
        Supervised {
            spec,
            pid: (pid != 0).then_some(pid),
            child: Arc::new(Mutex::new(child)),
            stdin: Arc::new(Mutex::new(stdin)),
            logs,
            task_id,
        },
    );
    Ok(pid)
}

/// Read a stream line by line into the shared log buffer, bounded in length.
fn stream_into<R>(logs: Arc<Mutex<Vec<String>>>, reader: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut buffer = logs.lock().await;
            buffer.push(line);
            if buffer.len() > MAX_LOG_LINES {
                let overflow = buffer.len() - MAX_LOG_LINES;
                buffer.drain(0..overflow);
            }
        }
    });
}

/// Wait, bounded, for a marker substring to appear in the process's logs.
async fn wait_for_marker(name: &str, marker: &str) -> bool {
    let Some(logs) = PROCS.get(name).map(|entry| entry.logs.clone()) else {
        return false;
    };
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if logs.lock().await.iter().any(|line| line.contains(marker)) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline || !is_running(name).await {
            // A last look, in case the marker and the exit raced.
            return logs.lock().await.iter().any(|line| line.contains(marker));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Stop a process, kill its tree, and drop it from the registry.
async fn stop_process(name: &str) -> bool {
    let Some((_, supervised)) = PROCS.remove(name) else {
        return false;
    };
    if let Some(pid) = supervised.pid {
        mikmik_core::process_tree::kill_tree(pid);
    }
    let _ = supervised.child.lock().await.kill().await;
    mikmik_core::tasks::global_registry().cancel(&supervised.task_id);
    true
}

/// Send a unix signal to a process by name.
#[cfg(unix)]
async fn send_signal(name: &str, signal: &str) -> ToolResult {
    let Some(pid) = PROCS.get(name).and_then(|entry| entry.pid) else {
        return ToolResult::error(format!("no running process named {name:?}"));
    };
    let Some(sig) = signal_from_name(signal) else {
        return ToolResult::error(format!("unknown signal {signal:?}"));
    };
    // SAFETY: `kill(2)` with a validated signal number and a pid this tool
    // started. A stale pid at worst signals nothing; it cannot corrupt memory.
    let result = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if result == 0 {
        ToolResult::success(format!("Sent SIG{signal} to {name:?}."))
    } else {
        ToolResult::error(format!("failed to signal {name:?}"))
    }
}

#[cfg(not(unix))]
async fn send_signal(_name: &str, _signal: &str) -> ToolResult {
    ToolResult::error("signals are only supported on unix; use stop instead".to_string())
}

/// The signal number for a name, for the handful worth sending to a child.
#[cfg(unix)]
fn signal_from_name(signal: &str) -> Option<libc::c_int> {
    match signal
        .trim_start_matches("SIG")
        .to_ascii_uppercase()
        .as_str()
    {
        "TERM" => Some(libc::SIGTERM),
        "KILL" => Some(libc::SIGKILL),
        "INT" => Some(libc::SIGINT),
        "HUP" => Some(libc::SIGHUP),
        "USR1" => Some(libc::SIGUSR1),
        "USR2" => Some(libc::SIGUSR2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_pairs_drop_entries_without_an_equals() {
        let env = vec!["A=1".to_string(), "bad".to_string(), "B=x=y".to_string()];
        let pairs = parse_env(Some(&env));
        assert_eq!(
            pairs,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "x=y".to_string())
            ]
        );
    }

    fn sample() -> Vec<String> {
        (1..=6).map(|n| format!("line {n}")).collect()
    }

    #[test]
    fn tail_returns_the_last_n_lines() {
        assert_eq!(
            log_window(&sample(), None, Some(2), None, None),
            vec!["line 5".to_string(), "line 6".to_string()]
        );
    }

    #[test]
    fn head_returns_the_first_n_lines() {
        assert_eq!(
            log_window(&sample(), Some(2), None, None, None),
            vec!["line 1".to_string(), "line 2".to_string()]
        );
    }

    #[test]
    fn grep_keeps_only_matching_lines() {
        let lines = vec![
            "info: start".to_string(),
            "error: boom".to_string(),
            "info: done".to_string(),
        ];
        assert_eq!(
            log_window(&lines, None, None, Some("error"), None),
            vec!["error: boom".to_string()]
        );
    }

    #[test]
    fn grep_then_tail_reads_the_last_matches() {
        let lines = vec![
            "hit 1".to_string(),
            "miss".to_string(),
            "hit 2".to_string(),
            "hit 3".to_string(),
        ];
        assert_eq!(
            log_window(&lines, None, Some(2), Some("hit"), None),
            vec!["hit 2".to_string(), "hit 3".to_string()]
        );
    }

    #[test]
    fn the_cursor_skips_from_the_front() {
        assert_eq!(
            log_window(&sample(), None, None, None, Some(4)),
            vec!["line 5".to_string(), "line 6".to_string()]
        );
    }

    /// The whole supervision path on a real process: start it, see it running,
    /// read the line it printed, then stop it and see it gone. A unit test on
    /// `log_window` alone would not catch a broken spawn, reader, or kill.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_is_started_read_and_stopped() {
        let name = "hub-test-proc";
        // Clean any leftover from a previous run in the same process.
        stop_process(name).await;

        let spec = Spec {
            application: "sh".to_string(),
            args: vec!["-c".to_string(), "echo READY; sleep 30".to_string()],
            env: Vec::new(),
            cwd: None,
        };
        let pid = spawn(name, spec).await.expect("the process starts");
        assert!(pid > 0, "a real pid");
        assert!(wait_for_marker(name, "READY").await, "it printed READY");
        assert!(is_running(name).await, "it is still running");

        let logs = PROCS
            .get(name)
            .map(|entry| entry.logs.clone())
            .expect("logs");
        let snapshot = logs.lock().await.clone();
        assert!(
            log_window(&snapshot, None, None, Some("READY"), None)
                .iter()
                .any(|line| line.contains("READY")),
            "grep finds the READY line"
        );

        assert!(stop_process(name).await, "stop reports it stopped it");
        assert!(!is_running(name).await, "it is no longer running");
        assert!(PROCS.get(name).is_none(), "it is dropped from the registry");
    }
}
