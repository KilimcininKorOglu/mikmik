// Advise tool: how a watching advisor puts a note in front of the primary.
//
// Registered only on a watcher's own context, where `advisor_note_tx` is set.
// A primary agent never sees it, so no agent can advise itself.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use mikmik_core::advisor::{AdvisorNote, AdvisorSeverity};
use serde::Deserialize;
use serde_json::{json, Value};

pub struct AdviseTool;

#[derive(Debug, Deserialize)]
struct AdviseInput {
    note: String,
    #[serde(default)]
    severity: Option<String>,
}

#[async_trait]
impl Tool for AdviseTool {
    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_ADVISE
    }

    fn description(&self) -> &str {
        "Surface one piece of advice to the agent you are watching. Terse, \
         specific, actionable. At most one call per update, and never the same \
         note twice. Severity says how strongly to weigh it: omit it or say \
         `nit` for cleanup and low-risk edge cases, which the agent reads at \
         the next step boundary; `concern` for a material risk or a likely \
         wrong direction, which stops the turn it arrives during; `blocker` \
         when continuing would clearly waste the work, which stops the turn and \
         wakes a finished one. Say nothing when the agent is on track: silence \
         is how you say there are no concerns."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "One concrete piece of advice for the agent you are watching."
                },
                "severity": {
                    "type": "string",
                    "enum": ["nit", "concern", "blocker"],
                    "description": "How strongly to weigh this. Omit for a plain nit."
                }
            },
            "required": ["note"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: AdviseInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let note = params.note.trim();
        if note.is_empty() {
            return ToolResult::error("A note with no text says nothing. Say what is wrong.");
        }

        let Some(tx) = ctx.advisor_note_tx.as_ref() else {
            return ToolResult::error(
                "This session has nobody to advise. `Advise` belongs to a watching advisor.",
            );
        };

        let severity = params
            .severity
            .as_deref()
            .map(AdvisorSeverity::parse)
            .unwrap_or(AdvisorSeverity::Nit);

        let sent = tx.send(AdvisorNote {
            advisor: ctx.advisor_name.clone(),
            severity,
            note: note.to_string(),
        });
        if sent.is_err() {
            return ToolResult::error("The session this advice was for has ended.");
        }

        // Always "recorded", whatever the guard downstream decides. Telling the
        // model its note was dropped teaches it to rephrase the same useless
        // note until one gets through, which is the behaviour the guard exists
        // to stop.
        ToolResult::success("Recorded.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;

    #[tokio::test]
    async fn a_note_reaches_the_channel_with_its_severity_and_its_author() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = allow_all_context(std::env::temp_dir());
        ctx.advisor_note_tx = Some(tx);
        ctx.advisor_name = Some("Architecture".to_string());

        let result = AdviseTool
            .execute(
                json!({ "note": "  The lock is held across an await.  ", "severity": "concern" }),
                &ctx,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        let note = rx.try_recv().expect("the note was sent");
        assert_eq!(note.note, "The lock is held across an await.");
        assert_eq!(note.severity, AdvisorSeverity::Concern);
        assert_eq!(note.advisor.as_deref(), Some("Architecture"));
    }

    #[tokio::test]
    async fn an_unreadable_severity_reads_as_a_nit() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = allow_all_context(std::env::temp_dir());
        ctx.advisor_note_tx = Some(tx);

        let result = AdviseTool
            .execute(
                json!({ "note": "Consider a helper.", "severity": "URGENT" }),
                &ctx,
            )
            .await;
        assert!(!result.is_error);
        assert_eq!(
            rx.try_recv().expect("sent").severity,
            AdvisorSeverity::Nit,
            "an unknown severity must not interrupt a turn"
        );
    }

    #[tokio::test]
    async fn a_primary_agent_has_nobody_to_advise() {
        let ctx = allow_all_context(std::env::temp_dir());
        let result = AdviseTool
            .execute(json!({ "note": "Something." }), &ctx)
            .await;
        assert!(result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn an_empty_note_is_refused() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = allow_all_context(std::env::temp_dir());
        ctx.advisor_note_tx = Some(tx);

        let result = AdviseTool.execute(json!({ "note": "   " }), &ctx).await;
        assert!(result.is_error, "{}", result.content);
    }
}
