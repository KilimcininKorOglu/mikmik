// SendMessageTool: send a message to another agent or broadcast to all.
//
// In the TypeScript version this uses a complex mailbox/swarm system with
// process-level sockets. The Rust port uses a simpler in-process DashMap
// inbox that works for sub-agents spawned via AgentTool.
//
// Messages are stored keyed by recipient name. Other agents can check
// their inbox by calling drain_inbox() or peek_inbox().

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// In-process inbox
// ---------------------------------------------------------------------------

/// A single message in the inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub from: String,
    pub to: String,
    pub content: String,
    pub timestamp: u64,
}

/// How much of a message the tool result echoes back to the sender.
const PREVIEW_CHARS: usize = 60;

/// Global inbox: recipient_id → queued messages.
static INBOX: Lazy<DashMap<String, Vec<AgentMessage>>> = Lazy::new(DashMap::new);

/// Remove and return all messages queued for `recipient`.
pub fn drain_inbox(recipient: &str) -> Vec<AgentMessage> {
    INBOX.remove(recipient).map(|(_, v)| v).unwrap_or_default()
}

/// Read (without removing) all messages queued for `recipient`.
pub fn peek_inbox(recipient: &str) -> Vec<AgentMessage> {
    INBOX.get(recipient).map(|v| v.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

pub struct SendMessageTool;

#[derive(Debug, Deserialize)]
struct SendMessageInput {
    /// Recipient name, or "*" for broadcast.
    to: String,
    /// Message body.
    message: String,
    /// Short preview text shown in the UI.
    #[serde(default)]
    summary: Option<String>,
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }

    fn description(&self) -> &str {
        "Send a message to another agent by name, or broadcast to all active agents with to=\"*\". \
         Recipients accumulate messages in their inbox and can retrieve them. \
         Use this for coordination between concurrent sub-agents."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient agent name or session ID. Use \"*\" to broadcast to all."
                },
                "message": {
                    "type": "string",
                    "description": "Message content"
                },
                "summary": {
                    "type": "string",
                    "description": "5–10 word preview for the UI (optional)"
                }
            },
            "required": ["to", "message"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: SendMessageInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        if params.message.is_empty() {
            return ToolResult::error("Message cannot be empty.".to_string());
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let msg = AgentMessage {
            from: ctx.session_id.clone(),
            to: params.to.clone(),
            content: params.message.clone(),
            timestamp: now,
        };

        // Both halves are model-supplied text, so both are bounded, and the
        // cut lands on a character boundary: slicing at a fixed byte offset
        // panics whenever that byte falls inside a multi-byte character.
        let preview = mikmik_core::truncate::truncate_text(
            params.summary.as_deref().unwrap_or(&params.message),
            PREVIEW_CHARS,
        );

        if params.to == "*" {
            // Broadcast: deliver to every existing inbox key
            let recipients: Vec<String> = INBOX.iter().map(|e| e.key().clone()).collect();

            if recipients.is_empty() {
                return ToolResult::success(
                    "Broadcast queued (no active recipient inboxes yet).".to_string(),
                );
            }

            for key in &recipients {
                INBOX.entry(key.clone()).or_default().push(msg.clone());
            }

            return ToolResult::success(format!(
                "Broadcast to {} agent(s): {}",
                recipients.len(),
                preview
            ));
        }

        // Directed message
        INBOX.entry(params.to.clone()).or_default().push(msg);

        ToolResult::success(format!("Message sent to '{}': {}", params.to, preview))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;
    use std::path::PathBuf;

    /// A message whose 60th byte lands inside a character. Slicing there is
    /// what the preview used to do, and it panicked.
    fn multibyte_message() -> String {
        format!("a{}", "€".repeat(25))
    }

    #[tokio::test]
    async fn preview_survives_a_multibyte_message() {
        let ctx = allow_all_context(PathBuf::from("/workspace"));
        let result = SendMessageTool
            .execute(
                json!({ "to": "preview-boundary-probe", "message": multibyte_message() }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        drain_inbox("preview-boundary-probe");
    }

    #[tokio::test]
    async fn preview_bounds_a_long_summary_too() {
        let ctx = allow_all_context(PathBuf::from("/workspace"));
        let result = SendMessageTool
            .execute(
                json!({
                    "to": "preview-summary-probe",
                    "message": "short",
                    "summary": "€".repeat(200),
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("(truncated)"),
            "an oversized summary should be cut: {}",
            result.content
        );
        drain_inbox("preview-summary-probe");
    }
}
