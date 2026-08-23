// REPLTool: Executes code in a persistent interpreter session.
//
// Tool name: "REPL" (matches TypeScript REPL_TOOL_NAME constant)
//
// The same interpreter process stays alive across multiple tool calls within
// a session. Supports: python3, node, bash (default).
//
// Input: { language?: "python"|"javascript"|"bash", code: string }
// Output: stdout/stderr from the interpreter
//
// Implementation uses per-(session, language) child processes kept alive in a
// global registry.  Code is injected over stdin; a known sentinel string is
// printed after each block so we know when output is complete.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use dashmap::DashMap;
use mikmik_core::bash_classifier::{classify_bash_command, BashRiskLevel};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;

// ---------------------------------------------------------------------------
// Session registry
// ---------------------------------------------------------------------------

struct ReplSession {
    // The handle is held so the interpreter can be killed at session end; it
    // is never read from after the spawn. Dropping it would not stop the
    // process either way, since `kill_on_drop` is deliberately not set.
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Key: (session_id, language)
///
/// Interpreters are kept alive across tool calls on purpose, which is what
/// makes a REPL a REPL. `shutdown_session` is what ends them; without it they
/// outlived the session that started them.
static REPL_SESSIONS: Lazy<Arc<DashMap<(String, String), Arc<Mutex<ReplSession>>>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

/// Stop every interpreter started for `session_id`.
///
/// Called when a session ends. Each interpreter is killed along with anything
/// it started, because a REPL is exactly the place where a user starts a
/// server or a watcher and leaves it running.
pub async fn shutdown_session(session_id: &str) {
    let keys: Vec<(String, String)> = REPL_SESSIONS
        .iter()
        .map(|entry| entry.key().clone())
        .filter(|(id, _)| id == session_id)
        .collect();

    for key in keys {
        let Some((_, session)) = REPL_SESSIONS.remove(&key) else {
            continue;
        };
        let mut session = session.lock().await;
        if let Some(pid) = session.child.id() {
            mikmik_core::process_tree::kill_tree(pid);
        }
        let _ = session.child.kill().await;
    }
}

// ---------------------------------------------------------------------------
// Sentinel values
// The interpreter prints this after executing user code so we know output is done.
// ---------------------------------------------------------------------------

const SENTINEL: &str = "__REPL_DONE_7f3a9b__";

/// Return the command + args to spawn for a given language.
fn interpreter_for(language: &str) -> Option<(&'static str, Vec<&'static str>)> {
    match language {
        "python" | "python3" => Some(("python3", vec!["-u", "-i"])),
        "javascript" | "node" => Some(("node", vec![])),
        "bash" | "" => Some(("bash", vec!["--norc", "--noprofile"])),
        _ => None,
    }
}

/// Build the code block + sentinel emission for the given language.
fn wrap_code(language: &str, code: &str) -> String {
    match language {
        "python" | "python3" => {
            // Wrap in exec() so multi-line blocks work inside `-i` mode.
            // After execution, print the sentinel unconditionally.
            format!(
                "import sys as _sys\ntry:\n    exec({:?})\nexcept Exception as _e:\n    print(repr(_e), file=_sys.stderr)\nprint({:?})\n",
                code, SENTINEL
            )
        }
        "javascript" | "node" => {
            // Node REPL (.load) can't do this inline; use eval via --input-type
            // but since we spawned a bare `node` process we use process.stdout.write.
            format!(
                "try {{ {} }} catch(e) {{ process.stderr.write(String(e) + '\\n') }}\nprocess.stdout.write({:?} + '\\n')\n",
                code, SENTINEL
            )
        }
        _ => {
            // bash: run code, echo sentinel at end
            format!("{}\necho {:?}\n", code, SENTINEL)
        }
    }
}

async fn get_or_spawn_session(
    session_id: &str,
    language: &str,
) -> Result<Arc<Mutex<ReplSession>>, String> {
    let key = (session_id.to_string(), language.to_string());

    // Fast path: session already exists
    if let Some(entry) = REPL_SESSIONS.get(&key) {
        return Ok(entry.clone());
    }

    // Spawn a new interpreter
    let (cmd, args) =
        interpreter_for(language).ok_or_else(|| format!("Unsupported language: {}", language))?;

    let mut builder = tokio::process::Command::new(cmd);
    builder
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own group, so `shutdown_session` can reach what the interpreter
    // started and not only the interpreter.
    mikmik_core::process_tree::spawn_in_own_group(&mut builder);
    let mut child = builder
        .spawn()
        .map_err(|e| format!("Failed to spawn '{}': {}", cmd, e))?;

    let stdin = child.stdin.take().ok_or("No stdin")?;
    let stdout = child.stdout.take().ok_or("No stdout")?;

    let session = Arc::new(Mutex::new(ReplSession {
        child,
        stdin,
        stdout: BufReader::new(stdout),
    }));

    REPL_SESSIONS.insert(key, session.clone());
    Ok(session)
}

/// Execute code in a session, returning collected output up to the sentinel.
async fn run_in_session(
    session: &Arc<Mutex<ReplSession>>,
    language: &str,
    code: &str,
) -> Result<String, String> {
    let wrapped = wrap_code(language, code);

    let mut guard = session.lock().await;
    guard
        .stdin
        .write_all(wrapped.as_bytes())
        .await
        .map_err(|e| format!("Write to interpreter stdin failed: {}", e))?;
    guard
        .stdin
        .flush()
        .await
        .map_err(|e| format!("Flush interpreter stdin failed: {}", e))?;

    // Read lines until we see the sentinel, with a timeout
    let mut output_lines: Vec<String> = Vec::new();
    let read_timeout = Duration::from_secs(30);

    loop {
        let mut line = String::new();
        let line_fut = guard.stdout.read_line(&mut line);
        match timeout(read_timeout, line_fut).await {
            Err(_) => {
                return Err(format!(
                    "Interpreter timed out after {}s waiting for output.",
                    read_timeout.as_secs()
                ))
            }
            Ok(Err(e)) => return Err(format!("Read error: {}", e)),
            Ok(Ok(0)) => {
                // EOF — interpreter exited
                return Err("Interpreter exited unexpectedly.".to_string());
            }
            Ok(Ok(_)) => {
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                if trimmed == SENTINEL {
                    break;
                }
                // Strip the Python `>>>` / `...` prompts that -i mode emits
                let clean = trimmed
                    .trim_start_matches(">>> ")
                    .trim_start_matches("... ");
                output_lines.push(clean.to_string());
            }
        }
    }

    Ok(output_lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

pub struct ReplTool;

#[derive(Debug, Deserialize)]
struct ReplInput {
    code: String,
    #[serde(default)]
    language: Option<String>,
}

#[async_trait]
impl Tool for ReplTool {
    // Gates itself: calls `ctx.check_permission` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "REPL"
    }

    fn description(&self) -> &str {
        "Execute code in a persistent interpreter session. The same interpreter process \
         stays alive across multiple tool calls so variables, imports, and state persist \
         between invocations. Supports bash (default), python, and javascript (node)."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The code to execute in the interpreter session"
                },
                "language": {
                    "type": "string",
                    "enum": ["bash", "python", "javascript"],
                    "description": "Interpreter language. Defaults to bash."
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: ReplInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let language = params.language.as_deref().unwrap_or("bash").to_lowercase();

        // ── Security gate (issue #209) ───────────────────────────────────────
        // REPL executes arbitrary, model-supplied code in a live interpreter, so
        // it must pass the same permission gate as the Bash tool BEFORE any
        // interpreter is spawned or any code is run.  `execute_tool` does not gate
        // on our behalf, so we gate here.  `is_read_only = false` ensures the
        // action is treated as arbitrary execution (never auto-approved in
        // Default/Plan/AcceptEdits modes).
        let preview: String = params
            .code
            .chars()
            .take(80)
            .collect::<String>()
            .replace('\n', " ");
        let reason = format!("REPL ({}): {}", language, preview);
        if let Err(e) = ctx.check_permission(self.name(), &reason, false) {
            return ToolResult::error(e.to_string());
        }

        // For shell languages, additionally block Critical-risk commands
        // unconditionally — exactly like the PTY Bash tool does.
        if matches!(language.as_str(), "bash" | "sh" | "")
            && classify_bash_command(&params.code) == BashRiskLevel::Critical
        {
            return ToolResult::error(format!(
                "Command blocked: classified as Critical risk by the bash security classifier.\n\
                 Refusing to execute REPL code: {}",
                preview
            ));
        }

        debug!(
            session = %ctx.session_id,
            language = %language,
            "ReplTool execute"
        );

        let session = match get_or_spawn_session(&ctx.session_id, &language).await {
            Ok(s) => s,
            Err(e) => return ToolResult::error(format!("Failed to start REPL session: {}", e)),
        };

        match run_in_session(&session, &language, &params.code).await {
            Ok(output) => ToolResult::success(output),
            Err(e) => {
                // Remove the dead session so next call spawns a fresh one
                let key = (ctx.session_id.clone(), language.clone());
                REPL_SESSIONS.remove(&key);
                ToolResult::error(format!("REPL error: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// Handler that always asks; combined with `non_interactive = true` this
    /// resolves to a permission denial (mirrors `AskPermissionHandler` in lib.rs).
    struct DenyHandler;
    impl mikmik_core::permissions::PermissionHandler for DenyHandler {
        fn check_permission(
            &self,
            _request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            mikmik_core::permissions::PermissionDecision::Ask {
                reason: "denied in test".to_string(),
            }
        }
        fn request_permission(
            &self,
            request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            self.check_permission(request)
        }
    }

    /// Handler that allows everything — used to exercise the Critical-block path.
    struct AllowHandler;
    impl mikmik_core::permissions::PermissionHandler for AllowHandler {
        fn check_permission(
            &self,
            _request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            mikmik_core::permissions::PermissionDecision::Allow
        }
        fn request_permission(
            &self,
            _request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            mikmik_core::permissions::PermissionDecision::Allow
        }
    }

    fn ctx_with(
        handler: Arc<dyn mikmik_core::permissions::PermissionHandler>,
        session_id: &str,
    ) -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: handler,
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: session_id.to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: Arc::new(AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: mikmik_core::config::Config::default(),
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            plan_approval_tx: None,
            tool_output_tx: None,
            plan_mode_tx: None,
            advisor_note_tx: None,
            advisor_name: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            current_call: None,
            editor: None,
            inbox: Default::default(),
        }
    }

    #[test]
    fn repl_requires_execute_permission_level() {
        assert_eq!(ReplTool.permission_level(), PermissionLevel::Execute);
    }

    #[tokio::test]
    async fn repl_denied_permission_blocks_execution() {
        let ctx = ctx_with(Arc::new(DenyHandler), "repl-deny-test");
        let result = ReplTool
            .execute(
                json!({ "language": "python", "code": "print('should not run')" }),
                &ctx,
            )
            .await;

        assert!(result.is_error, "denied REPL must return an error result");
        // No interpreter should have been spawned for this session/language.
        assert!(
            REPL_SESSIONS
                .get(&("repl-deny-test".to_string(), "python".to_string()))
                .is_none(),
            "no REPL session must be spawned when permission is denied"
        );
    }

    #[tokio::test]
    async fn repl_blocks_critical_bash_even_when_allowed() {
        let ctx = ctx_with(Arc::new(AllowHandler), "repl-critical-test");
        let result = ReplTool
            .execute(json!({ "language": "bash", "code": "rm -rf /" }), &ctx)
            .await;

        assert!(result.is_error, "Critical bash must be blocked");
        assert!(
            result.content.contains("Critical"),
            "block message should mention the Critical classification, got: {}",
            result.content
        );
        assert!(
            REPL_SESSIONS
                .get(&("repl-critical-test".to_string(), "bash".to_string()))
                .is_none(),
            "no bash REPL session must be spawned for a Critical command"
        );
    }
    /// A sleep duration no other run can be using, so a process left behind by
    /// an earlier run is never read as this one's.
    #[cfg(unix)]
    fn unique_marker() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        // Fractional seconds keep the number one `sleep` accepts.
        format!("999332.{}", nanos % 1_000_000_000)
    }

    #[cfg(unix)]
    fn pgrep_matches(marker: &str) -> bool {
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .map(|out| !out.stdout.is_empty())
            .unwrap_or(false)
    }

    /// Interpreters outlived the session that started them: nothing ever ended
    /// them, so they sat in the registry until the process exited.
    #[cfg(unix)]
    #[tokio::test]
    async fn ending_a_session_stops_its_interpreters_and_their_children() {
        let marker = unique_marker();
        let session_id = format!("repl-shutdown-{marker}");
        let ctx = ctx_with(Arc::new(AllowHandler), &session_id);

        let result = ReplTool
            .execute(
                json!({
                    "language": "bash",
                    "code": format!("sleep {marker} >/dev/null 2>&1 &"),
                }),
                &ctx,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(pgrep_matches(&marker), "the child never started");

        shutdown_session(&session_id).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pgrep_matches(&marker) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !pgrep_matches(&marker),
            "the interpreter's child survived the session"
        );
        assert!(
            REPL_SESSIONS
                .iter()
                .all(|entry| entry.key().0 != session_id),
            "the session's entries must be gone from the registry"
        );
    }
}
