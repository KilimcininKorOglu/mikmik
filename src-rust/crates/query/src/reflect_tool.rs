//! Reflect tool: consolidate recent sessions into durable memory on demand.
//!
//! `AutoDream` already runs a "Dream: Memory Consolidation" pass, but only on a
//! schedule (24h since the last run, 5 new sessions, no held lock). `Reflect`
//! lets the model ask for that pass now: it bypasses the time and session gates
//! but still takes the lock, so it cannot collide with a scheduled dream. The
//! run itself goes through the same `run_consolidation` path the scheduler
//! uses, so the two cannot drift.

use async_trait::async_trait;
use mikmik_tools::{PermissionLevel, Tool, ToolContext, ToolResult};
use serde_json::{json, Value};

pub struct ReflectTool;

#[async_trait]
impl Tool for ReflectTool {
    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_REFLECT
    }

    fn description(&self) -> &str {
        "Consolidate recent sessions into durable memory now. Runs a reflective \
         pass over the memory files: it merges new signal into existing notes, \
         removes contradicted facts, and prunes the index. Use it after a \
         session that taught the project something worth keeping. It writes \
         through the memory tools, so nothing is lost if you stop early."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "focus": {
                    "type": "string",
                    "description": "Optional. A topic to concentrate the consolidation on, e.g. \"the release process\"."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let focus = input
            .get("focus")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|focus| !focus.is_empty())
            .map(str::to_string);

        let project_root = mikmik_core::session_storage::transcript_root_for(&ctx.working_dir);
        let memory_dir = mikmik_core::memdir::auto_memory_path(&project_root);
        let conversations_dir = mikmik_core::session_storage::transcript_dir(&project_root);
        let dreamer = crate::auto_dream::AutoDream::new(memory_dir, conversations_dir);

        match dreamer.force_consolidation().await {
            Ok(Some(mut task)) => {
                if let Some(focus) = focus {
                    task.prompt
                        .push_str(&format!("\n\n## Focus for this run\n{focus}"));
                }
                let summary = crate::consolidation::run_consolidation(task, ctx, false).await;
                ToolResult::success(
                    summary.unwrap_or_else(|| "Memory consolidation complete.".to_string()),
                )
            }
            Ok(None) => ToolResult::success(
                "A memory consolidation is already running, so this one was skipped.".to_string(),
            ),
            Err(error) => {
                ToolResult::error(format!("Could not start memory consolidation: {error}"))
            }
        }
    }
}
