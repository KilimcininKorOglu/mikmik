//! A persistent JavaScript session that drives the desktop.
//!
//! The `computer` tool takes one action per call and forgets everything
//! between them, so a task that reads the screen, decides, and acts costs
//! three turns. This tool spends one: the code runs in a session that stays
//! alive, so a variable set in one call is still there in the next.
//!
//! The session is a `node` process talking to this side over a loopback
//! socket. A socket rather than the process's own stdin, because the host has
//! to answer host calls *while* the code is still running, and one pipe
//! cannot carry the code in and the answers back without the script blocking
//! on the descriptor the host is writing to.

mod ax;
mod host_ops;
mod protocol;

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use protocol::{FromRunner, ToRunner};

/// The runner the session's `node` process executes.
const RUNNER_JS: &str = include_str!("runner.js");

/// The tool's name, which is also its permission-rule key.
pub const TOOL_NAME: &str = "computer_script";

/// How long one call may run when it names no limit of its own.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// The longest limit a call may ask for.
const MAX_TIMEOUT_SECS: u64 = 600;

/// How long the runner has to connect back before the session is abandoned.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

struct ScriptSession {
    /// Held so `shutdown_session` can kill the process tree. Never read from.
    child: Child,
    /// The socket the runner connected on.
    stream: BufReader<TcpStream>,
    /// The id of the next call, so a late answer from a timed-out call is
    /// recognised as stale rather than read as this call's result.
    next_call: u64,
    /// The accessibility elements this session is holding. Shared with the
    /// blocking thread that answers an `ax_*` op, so a handle one call held is
    /// still valid in the next.
    ax_handles: Arc<ax::HandleStore>,
}

/// One session per `session_id`.
static SESSIONS: Lazy<Arc<DashMap<String, Arc<Mutex<ScriptSession>>>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// Stop the scripting session `session_id` started, if it started one.
///
/// A script is exactly the place where someone leaves a loop running, so the
/// whole process tree goes, not just `node`.
pub async fn shutdown_session(session_id: &str) {
    let Some((_, session)) = SESSIONS.remove(session_id) else {
        return;
    };
    let mut session = session.lock().await;
    // Let go of every held element: one outlives the window it names, and
    // keeping it pins a platform object for nothing once the session ends.
    session.ax_handles.clear();
    if let Some(pid) = session.child.id() {
        mikmik_core::process_tree::kill_tree(pid);
    }
    let _ = session.child.kill().await;
}

/// A token the runner has to present, so nothing else on the machine can talk
/// to a listener that drives the desktop.
fn connect_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|error| format!("no random source: {error}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn get_or_spawn(session_id: &str) -> Result<Arc<Mutex<ScriptSession>>, String> {
    if let Some(existing) = SESSIONS.get(session_id) {
        return Ok(existing.clone());
    }

    // Loopback only, and on a port the OS picks: nothing off this machine can
    // reach a listener that presses keys on it.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("could not open the bridge: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("could not read the bridge port: {error}"))?
        .port();
    let token = connect_token()?;

    let runner_path = write_runner()?;
    let mut builder = tokio::process::Command::new("node");
    builder
        .arg(&runner_path)
        .arg(port.to_string())
        .arg(&token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    mikmik_core::process_tree::spawn_in_own_group(&mut builder);
    let child = builder
        .spawn()
        .map_err(|error| format!("could not start node: {error}"))?;

    let stream = accept_runner(listener, &token).await?;
    let session = Arc::new(Mutex::new(ScriptSession {
        child,
        stream: BufReader::new(stream),
        next_call: 1,
        ax_handles: Arc::new(ax::HandleStore::new()),
    }));
    SESSIONS.insert(session_id.to_string(), session.clone());
    debug!(port, "computer_script session started");
    Ok(session)
}

/// Put the runner on disk beside the other per-user state.
///
/// Written every start rather than once: the file is small, and a copy left
/// by an older build would otherwise run instead of this one's.
fn write_runner() -> Result<std::path::PathBuf, String> {
    let dir = mikmik_core::mikmik_home().join("run");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not prepare {}: {error}", dir.display()))?;
    let path = dir.join("computer_script_runner.js");
    std::fs::write(&path, RUNNER_JS)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(path)
}

/// Take the runner's connection, and refuse anything that cannot name the
/// token this session was started with.
async fn accept_runner(listener: TcpListener, token: &str) -> Result<TcpStream, String> {
    let accepted = timeout(CONNECT_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "node did not connect back".to_string())?
        .map_err(|error| format!("bridge accept failed: {error}"))?;
    let (stream, _) = accepted;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    timeout(CONNECT_TIMEOUT, reader.read_line(&mut line))
        .await
        .map_err(|_| "node connected but said nothing".to_string())?
        .map_err(|error| format!("bridge read failed: {error}"))?;

    match serde_json::from_str::<FromRunner>(line.trim()) {
        Ok(FromRunner::Hello { token: given }) if given == token => Ok(reader.into_inner()),
        _ => Err("something other than the runner connected to the bridge".to_string()),
    }
}

/// Run one call and answer every host call it makes along the way.
async fn run_call(
    session: &Arc<Mutex<ScriptSession>>,
    code: &str,
    read_only: bool,
    limit: Duration,
) -> Result<CallOutcome, String> {
    let mut guard = session.lock().await;
    let call_id = guard.next_call;
    guard.next_call += 1;

    write_line(
        &mut guard,
        &ToRunner::Run {
            id: call_id,
            code: code.to_string(),
            read_only,
        },
    )
    .await?;

    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let mut line = String::new();
        let read = tokio::time::timeout_at(deadline, guard.stream.read_line(&mut line)).await;
        let Ok(read) = read else {
            return Err(format!(
                "the script did not finish within {}s",
                limit.as_secs()
            ));
        };
        match read {
            Err(error) => return Err(format!("bridge read failed: {error}")),
            Ok(0) => return Err("the scripting session ended".to_string()),
            Ok(_) => {}
        }

        let Ok(message) = serde_json::from_str::<FromRunner>(line.trim()) else {
            continue;
        };
        match message {
            FromRunner::Hello { .. } => continue,
            FromRunner::Host { id, op, args } => {
                let ax_handles = guard.ax_handles.clone();
                let answer =
                    answer_host_call(id, &op, &args, read_only, deadline, limit, ax_handles)
                        .await?;
                write_line(&mut guard, &answer).await?;
            }
            FromRunner::Done {
                id,
                ok,
                output,
                value,
                error,
            } => {
                // A `done` from an earlier call reached us late; the call it
                // belongs to has already reported its own timeout.
                if id != call_id {
                    continue;
                }
                return Ok(CallOutcome {
                    ok,
                    output,
                    value,
                    error,
                });
            }
        }
    }
}

/// Decide what to answer one host call with.
///
/// Two policies live here rather than in the loop. `read_only` is enforced a
/// second time, after the runner has already enforced it, so the flag still
/// holds if the runner is replaced by something that does not check it. And the
/// op is bound by the call's own deadline: an unbounded op spends the whole
/// budget inside itself and the loop then reports the *script* as the thing
/// that overran, which is the wrong thing to tell anyone. On macOS this is not
/// hypothetical — a screen or accessibility call from a binary with no grant
/// blocks for tens of seconds, and the op's name is the only useful thing to
/// say about it.
///
/// An `Err` ends the call. A refused or failed op is an `Ok` the script can
/// catch, because the session is still healthy.
async fn answer_host_call(
    id: u64,
    op: &str,
    args: &Value,
    read_only: bool,
    deadline: tokio::time::Instant,
    limit: Duration,
    ax_handles: Arc<ax::HandleStore>,
) -> Result<ToRunner, String> {
    let writes = if ax::owns(op) {
        ax::writes(op)
    } else {
        host_ops::writes(op)
    };
    if read_only && writes {
        return Ok(ToRunner::failed(
            id,
            format!("read_only is set, so {op} is refused"),
        ));
    }

    // The two op families answer on different threads: an `ax_*` op reads the
    // session's held elements, so it runs on a blocking thread with the store,
    // while the desktop ops go through `host_ops`. Both are bound by the call's
    // deadline for the same reason: an op that never returns must not spend the
    // whole budget and let the loop blame the script for overrunning.
    let overran = || {
        format!(
            "`{op}` did not answer within the call's {}s; on macOS that is what an ungranted \
             screen-recording or accessibility permission looks like",
            limit.as_secs()
        )
    };

    // A budget already spent ends the call here, naming the op, instead of
    // letting a fast op finish past the deadline. `timeout_at` polls the inner
    // future before it consults the clock, so an op that answers instantly
    // (macOS `clipboard_read`) would otherwise slip past an exhausted deadline.
    if tokio::time::Instant::now() >= deadline {
        return Err(overran());
    }

    if ax::owns(op) {
        let (op_owned, args_owned) = (op.to_string(), args.clone());
        let task = tokio::task::spawn_blocking(move || {
            ax::run_blocking(&op_owned, &args_owned, &ax_handles)
        });
        return match tokio::time::timeout_at(deadline, task).await {
            Err(_) => Err(overran()),
            Ok(Err(join)) => Err(format!("the accessibility call did not finish: {join}")),
            Ok(Ok(Ok(value))) => Ok(ToRunner::ok(id, value)),
            Ok(Ok(Err(error))) => Ok(ToRunner::failed(id, error.to_string())),
        };
    }

    match tokio::time::timeout_at(deadline, host_ops::run(op, args)).await {
        Err(_) => Err(overran()),
        Ok(Ok(value)) => Ok(ToRunner::ok(id, value)),
        Ok(Err(error)) => Ok(ToRunner::failed(id, error)),
    }
}

struct CallOutcome {
    ok: bool,
    output: String,
    value: Value,
    error: Option<String>,
}

async fn write_line(session: &mut ScriptSession, message: &ToRunner) -> Result<(), String> {
    let mut line = serde_json::to_string(message)
        .map_err(|error| format!("could not encode a bridge message: {error}"))?;
    line.push('\n');
    session
        .stream
        .get_mut()
        .write_all(line.as_bytes())
        .await
        .map_err(|error| format!("bridge write failed: {error}"))
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

pub struct ComputerScriptTool;

#[derive(Debug, Deserialize)]
struct ScriptInput {
    code: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    timeout: Option<u64>,
}

#[async_trait]
impl Tool for ComputerScriptTool {
    // Prompts for itself in `execute`, like the other desktop tool.
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        concat!(
            "Run JavaScript against the real desktop in a session that stays alive between \
             calls, so a variable set in one call is still there in the next. Use it when a \
             task needs to look at the screen and act on what it sees within one turn. \
             Top-level await is available. Assign without `let` or `const` to keep a value \
             for the next call. Available: ",
            "await screenshot() -> {width,height,mime_type,base64}; ",
            "await displays(); await windows(); await cursor(); ",
            "await move(x,y); await click(x,y,button?); await doubleClick(x,y); ",
            "await drag(x1,y1,x2,y2); await type(text); await key('ctrl+c'); ",
            "await scroll('down',3); await clipboard() / clipboard(text); await wait(ms); ",
            "print(...) to report a value. ",
            "The accessibility tree, which names every control the platform draws: ",
            "await ax.focused(); await ax.tree(pid?,depth?); await ax.find({role,title,value,pid?,limit?}); ",
            "await ax.get(handle,attr); await ax.set(handle,attr,value); await ax.press(handle). ",
            "A find or tree returns nodes with an opaque handle you pass back to get, set and press. ",
            "This tool reads no DOM; use the browser for a page."
        )
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Dangerous
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "JavaScript to run, with top-level await available"
                },
                "read_only": {
                    "type": "boolean",
                    "description": "Refuse anything that moves the pointer, presses a key or writes the clipboard. Reading the screen, the displays, the windows and the clipboard still works."
                },
                "timeout": {
                    "type": "number",
                    "description": "Seconds to allow (default 120, maximum 600)"
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: ScriptInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        let description = if params.read_only {
            "computer_script: read the desktop".to_string()
        } else {
            "computer_script: drive the desktop".to_string()
        };
        if let Err(error) = ctx.check_permission(self.name(), &description, false) {
            return ToolResult::error(error.to_string());
        }

        let limit = Duration::from_secs(
            params
                .timeout
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        );

        let session = match get_or_spawn(&ctx.session_id).await {
            Ok(session) => session,
            Err(error) => return ToolResult::error(error),
        };

        match run_call(&session, &params.code, params.read_only, limit).await {
            Ok(outcome) => report(outcome),
            Err(error) => {
                // A broken bridge leaves a process nothing will talk to again.
                shutdown_session(&ctx.session_id).await;
                ToolResult::error(error)
            }
        }
    }
}

/// Turn a finished call into what the model reads.
fn report(outcome: CallOutcome) -> ToolResult {
    let mut parts: Vec<String> = Vec::new();
    if !outcome.output.is_empty() {
        parts.push(outcome.output.clone());
    }
    if !outcome.value.is_null() {
        parts.push(format!("=> {}", render_value(&outcome.value)));
    }

    if outcome.ok {
        if parts.is_empty() {
            parts.push("(no output)".to_string());
        }
        ToolResult::success(parts.join("\n"))
    } else {
        parts.push(outcome.error.unwrap_or_else(|| "the script failed".into()));
        ToolResult::error(parts.join("\n"))
    }
}

/// A returned value, with a screenshot's payload left out.
///
/// A base64 image is hundreds of kilobytes and the model cannot read it as
/// text; the shape around it is what says the capture worked.
fn render_value(value: &Value) -> String {
    let trimmed = strip_base64(value.clone());
    serde_json::to_string(&trimmed).unwrap_or_else(|_| "<unprintable>".to_string())
}

fn strip_base64(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let trimmed: serde_json::Map<String, Value> = map
                .into_iter()
                .map(|(key, inner)| {
                    if key == "base64" {
                        let length = inner.as_str().map(str::len).unwrap_or(0);
                        (key, json!(format!("<{length} base64 characters>")))
                    } else {
                        (key, strip_base64(inner))
                    }
                })
                .collect();
            Value::Object(trimmed)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(strip_base64).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_promises_exactly_what_the_runner_defines() {
        // The runner defines the surface; the description quotes it. A name in
        // one and not the other sends the model at a function that is not
        // there, or hides one that is.
        // Matched on the call form rather than the bare name. A bare `contains`
        // is satisfied by any longer word that happens to hold the name:
        // "wait" is inside "await", which the description says anyway, so the
        // loose check passed even with `wait(ms)` deleted.
        let description = ComputerScriptTool.description();
        for name in [
            "screenshot",
            "displays",
            "windows",
            "cursor",
            "move",
            "click",
            "doubleClick",
            "drag",
            "type",
            "key",
            "scroll",
            "clipboard",
            "wait",
        ] {
            assert!(
                description.contains(&format!("{name}(")),
                "the description omits {name}()"
            );
            assert!(
                RUNNER_JS.contains(&format!("{name}: ")),
                "the runner does not define {name}"
            );
        }
        // `print` is not one of the api entries; the runner puts it on
        // `globalThis` beside them.
        assert!(
            description.contains("print("),
            "the description omits print()"
        );
        assert!(
            RUNNER_JS.contains("globalThis.print"),
            "the runner does not define print"
        );

        // The accessibility surface is a nested object, so it is checked as
        // `ax.<name>(` in the description and `<name>: ` in the runner's `ax`
        // literal. Same failure the flat check guards: a name in one and not
        // the other.
        for name in ["focused", "tree", "find", "get", "set", "press"] {
            assert!(
                description.contains(&format!("ax.{name}(")),
                "the description omits ax.{name}()"
            );
            assert!(
                RUNNER_JS.contains(&format!("{name}: ")),
                "the runner's ax object does not define {name}"
            );
        }
    }

    #[test]
    fn a_screenshot_is_reported_without_its_payload() {
        let value = json!({"width": 100, "base64": "AAAABBBB"});

        let rendered = render_value(&value);

        assert!(rendered.contains("8 base64 characters"), "{rendered}");
        assert!(!rendered.contains("AAAABBBB"), "{rendered}");
    }

    #[test]
    fn a_failed_call_reports_what_it_printed_before_it_failed() {
        let result = report(CallOutcome {
            ok: false,
            output: "halfway".to_string(),
            value: Value::Null,
            error: Some("boom".to_string()),
        });

        assert!(result.is_error);
        assert!(result.content.contains("halfway"), "{}", result.content);
        assert!(result.content.contains("boom"), "{}", result.content);
    }

    #[test]
    fn a_call_that_returns_nothing_still_says_so() {
        let result = report(CallOutcome {
            ok: true,
            output: String::new(),
            value: Value::Null,
            error: None,
        });

        assert!(!result.is_error);
        assert!(result.content.contains("no output"), "{}", result.content);
    }

    /// A deadline that has already passed, so no op can beat it.
    fn spent_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() - Duration::from_secs(1)
    }

    #[tokio::test]
    async fn an_op_that_outlives_the_call_names_itself_and_not_the_script() {
        // The message the user reads has to name the op. Before the bound was
        // there, a platform call that blocked for a minute was reported as "the
        // script did not finish", which sends anyone reading it at the wrong
        // half of the system.
        let error = answer_host_call(
            1,
            "clipboard_read",
            &json!({}),
            false,
            spent_deadline(),
            Duration::from_secs(30),
            Arc::new(ax::HandleStore::new()),
        )
        .await
        .expect_err("a spent deadline ends the call");

        assert!(error.contains("clipboard_read"), "{error}");
        assert!(!error.contains("the script"), "{error}");
    }

    #[tokio::test]
    async fn read_only_refuses_a_writing_op_without_reaching_the_desktop() {
        // The refusal has to come back before the deadline is consulted:
        // nothing should touch the pointer, and a spent deadline proves the op
        // was never started.
        let answer = answer_host_call(
            7,
            "click",
            &json!({"x": 1, "y": 1}),
            true,
            spent_deadline(),
            Duration::from_secs(30),
            Arc::new(ax::HandleStore::new()),
        )
        .await
        .expect("a refusal is an answer, not a broken session");

        let encoded = serde_json::to_string(&answer).expect("the answer encodes");
        assert!(encoded.contains("read_only"), "{encoded}");
        assert!(encoded.contains("\"ok\":false"), "{encoded}");
    }

    #[tokio::test]
    async fn read_only_refuses_a_writing_ax_op() {
        // `ax_set` writes, so `read_only` has to close it in the same place it
        // closes `click`. The gate reads `ax::writes` for an `ax_*` op, and a
        // spent deadline proves the backend was never reached.
        let answer = answer_host_call(
            3,
            "ax_set",
            &json!({"handle": "ax-1", "attribute": "AXValue", "value": "x"}),
            true,
            spent_deadline(),
            Duration::from_secs(30),
            Arc::new(ax::HandleStore::new()),
        )
        .await
        .expect("a refusal is an answer, not a broken session");

        let encoded = serde_json::to_string(&answer).expect("the answer encodes");
        assert!(encoded.contains("read_only"), "{encoded}");
        assert!(encoded.contains("\"ok\":false"), "{encoded}");
    }

    #[tokio::test]
    async fn an_unknown_ax_handle_fails_the_op_without_breaking_the_session() {
        // The script sends any handle string it likes. One the store does not
        // hold is a failed op the script can catch, not an `Err` that tears the
        // session down. This also exercises the blocking-thread path with the
        // real store, no platform grant required.
        let answer = answer_host_call(
            9,
            "ax_get",
            &json!({"handle": "ax-404", "attribute": "AXValue"}),
            false,
            tokio::time::Instant::now() + Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(ax::HandleStore::new()),
        )
        .await
        .expect("an unknown handle is a caught failure, not a broken session");

        let encoded = serde_json::to_string(&answer).expect("the answer encodes");
        assert!(encoded.contains("ax-404"), "{encoded}");
        assert!(encoded.contains("\"ok\":false"), "{encoded}");
    }

    #[test]
    fn the_token_is_long_and_different_every_time() {
        let first = connect_token().expect("a token");
        let second = connect_token().expect("a token");

        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
    }
}
