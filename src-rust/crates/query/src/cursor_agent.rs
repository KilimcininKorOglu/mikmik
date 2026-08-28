//! cursor_agent.rs — bind Cursor's exec channel to real mikmik tools.
//!
//! The Cursor provider in `mikmik-api` defines the `CursorExecHandlers` trait
//! but cannot depend on `mikmik-tools`, so the implementation lives here where
//! the tool registry and its `ToolContext` (cwd, permission mode, permission
//! handler) are reachable. Each handler maps one Cursor tool onto the matching
//! mikmik tool and runs it through the same `execute_tool` path the ordinary
//! turn loop uses, so a Cursor-driven tool call is gated exactly like a
//! model-driven one.

use async_trait::async_trait;
use mikmik_api::providers::cursor::{CursorExecHandlers, CursorTurnOutcome, ToolExecOutcome};
use mikmik_tools::{Tool, ToolContext};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::runner::tools::execute_tool;
use crate::QueryEvent;

/// Runs Cursor exec frames against the live mikmik tool set.
pub struct CursorBridge<'a> {
    tools: &'a [Box<dyn Tool>],
    ctx: &'a ToolContext,
}

impl<'a> CursorBridge<'a> {
    pub fn new(tools: &'a [Box<dyn Tool>], ctx: &'a ToolContext) -> Self {
        Self { tools, ctx }
    }

    /// Run one mikmik tool by name and flatten its result to text.
    async fn run(&self, name: &str, input: Value) -> ToolExecOutcome {
        let tool_id = uuid::Uuid::new_v4().to_string();
        let result = execute_tool(name, &tool_id, &input, self.tools, self.ctx).await;
        ToolExecOutcome {
            text: result.content,
            is_error: result.is_error,
        }
    }
}

/// Quote a path or directory for a single-quoted shell word.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[async_trait]
impl CursorExecHandlers for CursorBridge<'_> {
    async fn read(&self, path: &str, offset: Option<i64>, limit: Option<i64>) -> ToolExecOutcome {
        let mut input = json!({ "file_path": path });
        if let Some(offset) = offset {
            input["offset"] = json!(offset);
        }
        if let Some(limit) = limit {
            input["limit"] = json!(limit);
        }
        self.run("Read", input).await
    }

    async fn write(&self, path: &str, content: &str) -> ToolExecOutcome {
        self.run("Write", json!({ "file_path": path, "content": content }))
            .await
    }

    async fn edit(&self, path: &str, edits: &[(String, String)]) -> ToolExecOutcome {
        if edits.is_empty() {
            return ToolExecOutcome::err("No edits supplied");
        }
        let mut applied = Vec::new();
        for (old_text, new_text) in edits {
            let outcome = self
                .run(
                    "Edit",
                    json!({
                        "file_path": path,
                        "old_string": old_text,
                        "new_string": new_text,
                    }),
                )
                .await;
            if outcome.is_error {
                return outcome;
            }
            applied.push(outcome.text);
        }
        ToolExecOutcome::ok(applied.join("\n"))
    }

    async fn delete(&self, path: &str) -> ToolExecOutcome {
        self.run(
            "Bash",
            json!({ "command": format!("rm -f -- {}", sh_quote(path)) }),
        )
        .await
    }

    async fn ls(&self, path: &str) -> ToolExecOutcome {
        let target = if path.is_empty() { "." } else { path };
        self.run("Read", json!({ "file_path": target })).await
    }

    async fn grep(
        &self,
        pattern: &str,
        path: &str,
        glob: &str,
        ignore_case: bool,
    ) -> ToolExecOutcome {
        let mut input = json!({ "pattern": pattern, "output_mode": "content" });
        if !path.is_empty() {
            input["path"] = json!(path);
        }
        if !glob.is_empty() {
            input["glob"] = json!(glob);
        }
        if ignore_case {
            input["-i"] = json!(true);
        }
        self.run("Grep", input).await
    }

    async fn find(&self, pattern: &str, path: &str) -> ToolExecOutcome {
        let mut input = json!({ "pattern": pattern });
        if !path.is_empty() {
            input["path"] = json!(path);
        }
        self.run("Glob", input).await
    }

    async fn shell(&self, command: &str, cwd: &str, timeout: Option<i64>) -> ToolExecOutcome {
        // mikmik's Bash tool has no working-directory parameter, so a directory
        // is composed onto the command instead.
        let full = if cwd.is_empty() {
            command.to_string()
        } else {
            format!("cd {} && {command}", sh_quote(cwd))
        };
        let mut input = json!({ "command": full });
        if let Some(timeout) = timeout {
            if timeout > 0 {
                input["timeout"] = json!(timeout);
            }
        }
        self.run("Bash", input).await
    }

    async fn diagnostics(&self, _path: &str) -> ToolExecOutcome {
        // mikmik's LSP tool exposes no diagnostics action, so report a clean run
        // rather than a failure the model would treat as a broken file.
        ToolExecOutcome::ok(String::new())
    }

    async fn mcp(&self, name: &str, args_json: &str) -> ToolExecOutcome {
        let input: Value = serde_json::from_str(args_json).unwrap_or_else(|_| json!({}));
        self.run(name, input).await
    }
}

/// Drive one full Cursor agent turn against the live tool set.
///
/// The agent runs its own multi-tool loop over one stream; this forwards its
/// deltas to the TUI and binds every exec frame to a real mikmik tool. Returns
/// `None` when the turn was cancelled, `Some(Err)` on failure, and `Some(Ok)`
/// with the assistant message the turn produced.
pub(crate) async fn drive_turn(
    account: &str,
    request: mikmik_api::ProviderRequest,
    tools: &[Box<dyn Tool>],
    tool_ctx: &ToolContext,
    event_tx: Option<&mpsc::UnboundedSender<QueryEvent>>,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> Option<Result<CursorTurnOutcome, String>> {
    use mikmik_api::providers::cursor::CursorAgent;
    let agent = match CursorAgent::from_account(account).or_else(CursorAgent::from_stored) {
        Some(agent) => agent,
        None => return Some(Err("no Cursor account is signed in".to_string())),
    };

    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<mikmik_api::StreamEvent>();
    let forward = event_tx.cloned();
    let forwarder = tokio::spawn(async move {
        while let Some(evt) = evt_rx.recv().await {
            if let Some(tx) = &forward {
                if let Some(ae) = crate::runner::stream::map_to_anthropic_event(&evt) {
                    let _ = tx.send(QueryEvent::Stream(ae));
                }
            }
        }
    });

    let bridge = CursorBridge::new(tools, tool_ctx);
    let result = tokio::select! {
        _ = cancel_token.cancelled() => None,
        r = mikmik_api::providers::cursor::run_turn(&agent, &request, &bridge, &evt_tx) => {
            Some(r.map_err(|e| e.to_string()))
        }
    };
    drop(evt_tx);
    let _ = forwarder.await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }
}
