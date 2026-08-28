//! The Goal tool — the model's own view of the durable goal a session carries.
//!
//! `/goal` is the user's door to the same store; this is the model's. One tool
//! with an `op`, so the model can read the goal it is working under, mark it
//! complete after an audit, resume a paused one, drop one, or, only through the
//! guided flow, create one.
//!
//! Completion is detected by the query loop from the goal's *status*, not from
//! this tool's name, so `complete` only has to set the status and the loop
//! stops on the next turn.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use mikmik_core::{Goal, GoalStatus, GoalStore};
use serde::Deserialize;
use serde_json::{json, Value};

pub struct GoalTool;

#[derive(Debug, Deserialize)]
struct GoalInput {
    /// One of get, complete, resume, drop, create.
    op: String,
    /// create: the objective to work toward.
    #[serde(default)]
    objective: Option<String>,
    /// create: a soft token budget, in tokens.
    #[serde(default)]
    token_budget: Option<u64>,
    /// complete: what was accomplished and verified.
    #[serde(default)]
    audit_summary: Option<String>,
    /// complete: concrete evidence — test output, diffs, command results.
    #[serde(default)]
    evidence: Option<String>,
}

#[async_trait]
impl Tool for GoalTool {
    fn name(&self) -> &str {
        "Goal"
    }

    fn description(&self) -> &str {
        "Read and manage the durable goal this session is working under. Ops:\n\
         - get: the objective, its status, and the token budget and how much is left.\n\
         - complete: mark the goal complete AFTER a genuine audit; requires audit_summary and evidence. Calling it without a real audit is a goal contract violation.\n\
         - resume: continue a paused or budget-limited goal.\n\
         - drop: delete the current goal.\n\
         - create: set a new goal (objective, optional token_budget). Only when a goal was opened for you and none exists yet."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["get", "complete", "resume", "drop", "create"],
                    "description": "Which goal action to take."
                },
                "objective": {
                    "type": "string",
                    "description": "create: the objective to work toward."
                },
                "token_budget": {
                    "type": "number",
                    "description": "create: a soft token budget, in tokens."
                },
                "audit_summary": {
                    "type": "string",
                    "description": "complete: concise summary of what was accomplished and verified."
                },
                "evidence": {
                    "type": "string",
                    "description": "complete: concrete evidence — test output, diffs, command results."
                }
            },
            "required": ["op"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: GoalInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        if !mikmik_core::goals_enabled() {
            return ToolResult::error(
                "Goals are disabled. Unset MIKMIK_GOALS=0 to re-enable.".to_string(),
            );
        }

        let Some(store) = GoalStore::open_default() else {
            return ToolResult::error("Could not open the goal store.".to_string());
        };
        let session_id = &ctx.session_id;

        match params.op.as_str() {
            "get" => op_get(&store, session_id),
            "complete" => op_complete(&store, session_id, &params),
            "resume" => op_resume(&store, session_id),
            "drop" => op_drop(&store, session_id),
            "create" => op_create(&store, session_id, &params),
            other => ToolResult::error(format!(
                "unknown op {other:?}; use get, complete, resume, drop or create"
            )),
        }
    }
}

/// Report the current goal, its status, and what budget is left.
fn op_get(store: &GoalStore, session_id: &str) -> ToolResult {
    match store.get_goal(session_id) {
        None => ToolResult::success(
            "No goal is set for this session. Ask the user to open one, or use op \"create\" when a goal has been opened for you."
                .to_string(),
        ),
        Some(goal) => ToolResult::success(describe(&goal)),
    }
}

/// Mark the goal complete after an audit.
fn op_complete(store: &GoalStore, session_id: &str, params: &GoalInput) -> ToolResult {
    let audit = params.audit_summary.as_deref().unwrap_or("").trim();
    let evidence = params.evidence.as_deref().unwrap_or("").trim();
    if audit.is_empty() {
        return ToolResult::error(
            "audit_summary is required for complete: state concretely what was achieved."
                .to_string(),
        );
    }
    if evidence.is_empty() {
        return ToolResult::error(
            "evidence is required for complete: give test output, diffs or command results."
                .to_string(),
        );
    }
    if store.get_active_goal(session_id).is_none() {
        return ToolResult::error(
            "There is no active goal to complete. Use op \"get\" to see the current one."
                .to_string(),
        );
    }
    match store.set_status(session_id, GoalStatus::Complete) {
        Ok(()) => ToolResult::success(format!(
            "Goal marked complete.\n\nAudit summary: {audit}\n\nEvidence: {evidence}"
        )),
        Err(error) => ToolResult::error(format!("Failed to mark the goal complete: {error}")),
    }
}

/// Resume a paused or budget-limited goal.
fn op_resume(store: &GoalStore, session_id: &str) -> ToolResult {
    match store.get_goal(session_id) {
        None => ToolResult::error("There is no goal to resume.".to_string()),
        Some(goal) => match goal.status {
            GoalStatus::Active => ToolResult::success("The goal is already active.".to_string()),
            GoalStatus::Complete => ToolResult::error(
                "The goal is complete; create a new one rather than resuming it.".to_string(),
            ),
            GoalStatus::Paused | GoalStatus::BudgetLimited => {
                match store.set_status(session_id, GoalStatus::Active) {
                    Ok(()) => ToolResult::success(
                        "Goal resumed; it will continue on the next turn.".to_string(),
                    ),
                    Err(error) => ToolResult::error(format!("Failed to resume the goal: {error}")),
                }
            }
        },
    }
}

/// Drop the current goal.
fn op_drop(store: &GoalStore, session_id: &str) -> ToolResult {
    match store.clear_goal(session_id) {
        Ok(()) => ToolResult::success("Goal dropped.".to_string()),
        Err(error) => ToolResult::error(format!("Failed to drop the goal: {error}")),
    }
}

/// Create a goal, but only when none exists yet.
///
/// The gate is the same one omp uses: the model does not set itself a budget
/// out of nowhere. A goal is opened for it through the guided flow, and only
/// then, with no record yet, may it create one from the requirements it
/// gathered.
fn op_create(store: &GoalStore, session_id: &str, params: &GoalInput) -> ToolResult {
    if store.get_goal(session_id).is_some() {
        return ToolResult::error(
            "A goal already exists for this session. Use op \"drop\" first, or \"resume\" to continue it."
                .to_string(),
        );
    }
    let objective = params.objective.as_deref().unwrap_or("").trim();
    if objective.is_empty() {
        return ToolResult::error(
            "objective is required for create: state the single verifiable outcome.".to_string(),
        );
    }
    match store.set_goal(session_id, objective, params.token_budget) {
        Ok(goal) => ToolResult::success(format!("Goal created.\n\n{}", describe(&goal))),
        Err(error) => ToolResult::error(format!("Failed to create the goal: {error}")),
    }
}

/// One human-readable block for a goal.
fn describe(goal: &Goal) -> String {
    let budget = match goal.budget_display() {
        Some(budget) => {
            let remaining = goal
                .token_budget
                .map(|b| b.saturating_sub(goal.tokens_used));
            match remaining {
                Some(remaining) => format!("\nBudget: {budget} ({remaining} tokens left)"),
                None => format!("\nBudget: {budget}"),
            }
        }
        None => String::new(),
    };
    format!(
        "Status: {}\nTurns: {}\nElapsed: {}{}\nObjective:\n  {}",
        goal.status.as_str(),
        goal.turns_used,
        goal.elapsed_display(),
        budget,
        goal.objective,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> GoalStore {
        // An in-memory store, so each test is isolated and touches no file.
        GoalStore::open(std::path::Path::new(":memory:")).expect("in-memory goal store")
    }

    #[test]
    fn get_reports_no_goal_when_none_is_set() {
        let result = op_get(&store(), "s1");
        assert!(!result.is_error);
        assert!(result.content.contains("No goal"), "{}", result.content);
    }

    #[test]
    fn create_then_get_reads_back_the_objective() {
        let store = store();
        let created = op_create(
            &store,
            "s1",
            &GoalInput {
                op: "create".into(),
                objective: Some("Migrate to Fastify".into()),
                token_budget: Some(250_000),
                audit_summary: None,
                evidence: None,
            },
        );
        assert!(!created.is_error, "{}", created.content);

        let got = op_get(&store, "s1");
        assert!(
            got.content.contains("Migrate to Fastify"),
            "{}",
            got.content
        );
        assert!(got.content.contains("tokens left"), "{}", got.content);
    }

    #[test]
    fn create_is_refused_when_a_goal_already_exists() {
        // The gate that keeps the model from setting itself a second budget.
        let store = store();
        store.set_goal("s1", "First", None).expect("seed a goal");

        let result = op_create(
            &store,
            "s1",
            &GoalInput {
                op: "create".into(),
                objective: Some("Second".into()),
                token_budget: None,
                audit_summary: None,
                evidence: None,
            },
        );

        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("already exists"),
            "{}",
            result.content
        );
    }

    #[test]
    fn complete_needs_an_audit_and_evidence() {
        let store = store();
        store.set_goal("s1", "Do the thing", None).expect("seed");

        let no_audit = op_complete(
            &store,
            "s1",
            &GoalInput {
                op: "complete".into(),
                objective: None,
                token_budget: None,
                audit_summary: Some("  ".into()),
                evidence: Some("tests pass".into()),
            },
        );
        assert!(no_audit.is_error, "{}", no_audit.content);
        assert!(
            no_audit.content.contains("audit_summary"),
            "{}",
            no_audit.content
        );
    }

    #[test]
    fn complete_sets_the_status_the_loop_reads() {
        // The query loop stops on `Complete` status, so the op has to leave the
        // store in exactly that state, not merely report success.
        let store = store();
        store.set_goal("s1", "Do the thing", None).expect("seed");

        let done = op_complete(
            &store,
            "s1",
            &GoalInput {
                op: "complete".into(),
                objective: None,
                token_budget: None,
                audit_summary: Some("did it".into()),
                evidence: Some("tests pass".into()),
            },
        );
        assert!(!done.is_error, "{}", done.content);
        assert_eq!(
            store.get_goal("s1").map(|goal| goal.status),
            Some(GoalStatus::Complete)
        );
    }

    #[test]
    fn resume_reactivates_a_paused_goal() {
        let store = store();
        store.set_goal("s1", "Do the thing", None).expect("seed");
        store
            .set_status("s1", GoalStatus::Paused)
            .expect("pause it");

        let resumed = op_resume(&store, "s1");
        assert!(!resumed.is_error, "{}", resumed.content);
        assert_eq!(
            store.get_goal("s1").map(|goal| goal.status),
            Some(GoalStatus::Active)
        );
    }

    #[test]
    fn drop_removes_the_goal() {
        let store = store();
        store.set_goal("s1", "Do the thing", None).expect("seed");

        let dropped = op_drop(&store, "s1");
        assert!(!dropped.is_error, "{}", dropped.content);
        assert!(store.get_goal("s1").is_none());
    }
}
