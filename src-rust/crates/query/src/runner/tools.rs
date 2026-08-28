// Tool execution helpers: argument parsing, permission gating, and the
// single-tool / batch execution paths. Extracted from lib.rs (issue #232).
// Behavior-preserving move.

use crate::*;

/// Parse the accumulated JSON arguments of a streamed tool call.
///
/// Providers stream a tool call's arguments as a sequence of partial-JSON
/// deltas which we concatenate into a single buffer. A well-behaved
/// no-argument call yields an empty (or whitespace-only) buffer, which we
/// map to an empty object. Any *non-empty* buffer that fails to parse is
/// returned as an error rather than being silently replaced with `{}` — a
/// truncated stream must never cause a tool (e.g. Edit/Write) to run with
/// empty arguments (issue #215).
pub(crate) fn parse_tool_args(json_str: &str) -> Result<Value, serde_json::Error> {
    let trimmed = json_str.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(trimmed)
}

/// Whether a `PermissionLevel` must be gated by the central backstop.
///
/// Only `None` and `ReadOnly` are exempt; every other level (`Write`,
/// `Execute`, `Dangerous`, `Forbidden`) represents a side-effecting action that
/// the backstop must confirm before it runs.
pub(crate) fn permission_level_is_gated(level: PermissionLevel) -> bool {
    !matches!(level, PermissionLevel::None | PermissionLevel::ReadOnly)
}

/// Synthesize a human-readable permission description for a tool that does not
/// gate itself, surfacing the tool name and a truncated preview of its input so
/// the user can see what is about to run.
pub(crate) fn synthesize_permission_description(name: &str, input: &Value) -> String {
    let rendered = serde_json::to_string(input).unwrap_or_default();
    let preview: String = rendered.chars().take(200).collect();
    if preview.is_empty() || preview == "{}" || preview == "null" {
        format!("Run tool '{}'", name)
    } else {
        format!("Run tool '{}' with input: {}", name, preview)
    }
}

/// Execute a single tool invocation.
pub(crate) async fn execute_tool(
    name: &str,
    tool_id: &str,
    input: &Value,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
) -> ToolResult {
    let tool = tools.iter().find(|t| t.name() == name);

    match tool {
        Some(tool) => {
            debug!(tool = name, "Executing tool");
            // A copy of the turn's context that knows which call it belongs to.
            // Concurrent calls each get their own, so nothing is shared that
            // one call could read as another's.
            let ctx = &ToolContext {
                current_call: Some(std::sync::Arc::new(mikmik_tools::ActiveToolCall {
                    id: tool_id.to_string(),
                    input: input.clone(),
                    permission_wait_ms: std::sync::Arc::default(),
                })),
                // A hosting client is told which call anything it shows the
                // user belongs to.
                editor: ctx
                    .editor
                    .as_ref()
                    .and_then(|editor| editor.for_tool_call(tool_id))
                    .or_else(|| ctx.editor.clone()),
                ..ctx.clone()
            };
            // Central permission backstop (issue #210): if a tool does not gate
            // itself (`self_gates() == false`) and requires a gated permission
            // level, prompt here BEFORE executing. On denial, return a blocked
            // result WITHOUT running the tool. Tools that already prompt
            // internally opt out via `self_gates() == true` (no double-prompt),
            // and read-only / no-permission tools are skipped. This makes a tool
            // that forgets to gate itself secure by default.
            if !tool.self_gates() && permission_level_is_gated(tool.permission_level()) {
                let description = synthesize_permission_description(name, input);
                if let Err(e) = ctx.check_permission(name, &description, false) {
                    warn!(tool = name, "Tool blocked by central permission backstop");
                    return ToolResult::error(e.to_string());
                }
            }
            // Timed here rather than around the whole function, so the number
            // is the tool's own work. The central backstop above prompts before
            // this point, so its wait is already outside the timer; clear the
            // per-call counter so only a wait the tool raises from *inside*
            // execute (a self-gating tool) is counted, then subtracted below.
            if let Some(call) = &ctx.current_call {
                call.permission_wait_ms
                    .store(0, std::sync::atomic::Ordering::Relaxed);
            }
            let started = std::time::Instant::now();
            let mut result = tool.execute(input.clone(), ctx).await;
            let elapsed = started.elapsed().as_millis() as u64;
            // A self-gating tool that prompted inside execute recorded how long
            // the user took; subtract it so the duration is the tool's own work.
            let waited = ctx
                .current_call
                .as_ref()
                .map(|call| {
                    call.permission_wait_ms
                        .load(std::sync::atomic::Ordering::Relaxed)
                })
                .unwrap_or(0);
            result.duration_ms = Some(elapsed.saturating_sub(waited));
            result
        }
        None => {
            warn!(tool = name, "Unknown tool requested");
            ToolResult::error(format!("Unknown tool: {}", name))
        }
    }
}

/// Run a batch of tool-execution futures concurrently, abandoning them promptly
/// if `cancel_token` fires (issue #218).
///
/// Returns exactly one `ToolResult` per input future, in order, plus a bool that
/// is `true` iff the batch was cancelled before every tool finished. On the
/// happy path (no cancellation) this is `join_all` and the results are the real
/// tool outputs. On cancellation the in-flight futures are dropped (abandoned)
/// and every position is filled with a synthetic cancelled `ToolResult` so the
/// caller can still answer every `tool_use` and keep the message history valid.
pub(crate) async fn run_tool_batch<F>(
    exec_futures: Vec<F>,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> (Vec<ToolResult>, bool)
where
    F: std::future::Future<Output = ToolResult>,
{
    let count = exec_futures.len();
    tokio::select! {
        results = futures::future::join_all(exec_futures) => (results, false),
        _ = cancel_token.cancelled() => {
            let cancelled = (0..count)
                .map(|_| ToolResult::error(TOOL_CANCELLED_MSG))
                .collect();
            (cancelled, true)
        }
    }
}

/// Load persisted todos for `session_id` and return a nudge string if any are
/// incomplete (status != "completed"). Returns empty string otherwise.
pub(crate) fn build_todo_nudge(session_id: &str) -> String {
    let todos = mikmik_tools::todo_write::load_todos(session_id);
    let incomplete_count = todos
        .iter()
        .filter(|t| t["status"].as_str() != Some("completed"))
        .count();
    if incomplete_count == 0 {
        String::new()
    } else {
        format!(
            "You have {} incomplete task{} in your TodoWrite list. \
             Make sure to complete all tasks before ending your response.",
            incomplete_count,
            if incomplete_count == 1 { "" } else { "s" }
        )
    }
}

#[cfg(test)]
mod duration_tests {
    use super::*;
    use async_trait::async_trait;
    use mikmik_core::permissions::{PermissionDecision, PermissionHandler, PermissionRequest};
    use mikmik_tools::{PendingPermissionStore, PermissionLevel, Tool, ToolResult};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    /// A handler that always asks, so the request reaches the pending queue and
    /// the call blocks until a decision is sent.
    struct AlwaysAsk;
    impl PermissionHandler for AlwaysAsk {
        fn check_permission(&self, _request: &PermissionRequest) -> PermissionDecision {
            PermissionDecision::Ask {
                reason: "ask".to_string(),
            }
        }
        fn request_permission(&self, _request: &PermissionRequest) -> PermissionDecision {
            PermissionDecision::Ask {
                reason: "ask".to_string(),
            }
        }
    }

    /// A self-gating tool whose own work is trivial: all its wall time is the
    /// permission prompt it raises from inside `execute`.
    struct SelfGatingSleeper;
    #[async_trait]
    impl Tool for SelfGatingSleeper {
        fn name(&self) -> &str {
            "self_gating_sleeper"
        }
        fn description(&self) -> &str {
            "prompts from inside execute"
        }
        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::Execute
        }
        fn self_gates(&self) -> bool {
            true
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({ "type": "object" })
        }
        async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
            match ctx.check_permission("self_gating_sleeper", "do it", false) {
                Ok(()) => ToolResult::success("done"),
                Err(error) => ToolResult::error(error.to_string()),
            }
        }
    }

    /// The reported duration must be the tool's own work, not how long the user
    /// took to approve a prompt the tool raised from inside `execute`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_self_gating_tools_duration_excludes_the_permission_wait() {
        let store = Arc::new(parking_lot::Mutex::new(PendingPermissionStore::default()));
        let mut ctx = crate::agent_tool::tests::parent_context();
        ctx.pending_permissions = Some(store.clone());
        ctx.non_interactive = false;
        ctx.permission_handler = Arc::new(AlwaysAsk);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(SelfGatingSleeper)];

        // Approve only after a long pause, so a duration that swallowed the wait
        // would be clearly above this pause and one that excludes it clearly
        // below.
        let store_bg = store.clone();
        let approver = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            loop {
                let taken = {
                    let mut guard = store_bg.lock();
                    guard.queue.pop_front()
                };
                if let Some(mut request) = taken {
                    if let Some(tx) = request.decision_tx.take() {
                        let _ = tx.send(PermissionDecision::Allow);
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let result = execute_tool("self_gating_sleeper", "call-1", &json!({}), &tools, &ctx).await;
        let _ = approver.await;

        assert!(!result.is_error, "{}", result.content);
        let duration = result.duration_ms.expect("a duration was stamped");
        assert!(
            duration < 300,
            "duration {duration}ms still carried the ~400ms permission wait"
        );
    }
}
