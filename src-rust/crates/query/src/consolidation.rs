//! Running a memory consolidation: the shared spawn path for the scheduled
//! `AutoDream` and the on-demand `Reflect` tool. Keeping it in one place stops
//! the two callers from drifting on the agent input or the lock release.

use crate::auto_dream::{AutoDream, ConsolidationTask};
use mikmik_tools::{Tool, ToolContext};

/// Given to the consolidation sub-agent as its system prompt.
const CONSOLIDATION_SYSTEM_PROMPT: &str = "You are performing automatic memory consolidation. \
     Complete the task and return a brief summary.";

/// Run one consolidation. `background` spawns and returns `None` at once;
/// otherwise it waits and returns the sub-agent's summary. Either way the lock
/// is released through `finish_consolidation` when the run ends.
pub async fn run_consolidation(
    task: ConsolidationTask,
    ctx: &ToolContext,
    background: bool,
) -> Option<String> {
    let agent_input = serde_json::json!({
        "description": "memory consolidation",
        "prompt": task.prompt.clone(),
        "max_turns": 20,
        "system_prompt": CONSOLIDATION_SYSTEM_PROMPT,
        "run_in_background": background,
        "isolation": null,
        // `AgentTool` reads this through `resolve_subagent_model`, which falls
        // back to the session's own route when the value is absent.
        "model": ctx.config.memory_model,
    });

    if background {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let agent = crate::agent_tool::AgentTool;
            let _ = Tool::execute(&agent, agent_input, &ctx).await;
            AutoDream::finish_consolidation(&task).await;
        });
        return None;
    }

    let agent = crate::agent_tool::AgentTool;
    let result = Tool::execute(&agent, agent_input, ctx).await;
    AutoDream::finish_consolidation(&task).await;
    Some(result.content)
}
