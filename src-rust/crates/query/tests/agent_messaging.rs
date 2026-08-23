//! What `AgentTool::execute` does before it ever reaches the network.
//!
//! The unit tests cover addressing, delivery and the guard's lifetime against
//! the functions themselves. What they cannot reach is the wiring inside
//! `execute`: that it claims a name at all, that the name it reports is the
//! one it claimed, and that a message sent to that name lands. None of it
//! needs a model, because `execute` returns a background agent's id without
//! any request going out.

use mikmik_tools::{AgentAddress, PermissionLevel, Tool, ToolContext, ToolResult};
use std::sync::Arc;

struct AllowAll;

impl mikmik_core::permissions::PermissionHandler for AllowAll {
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

/// A top-level session, addressed the way `run_query_loop` addresses one.
fn session_context(session_id: &str) -> ToolContext {
    ToolContext {
        working_dir: std::env::temp_dir(),
        permission_mode: mikmik_core::config::PermissionMode::Default,
        permission_handler: Arc::new(AllowAll),
        cost_tracker: mikmik_core::cost::CostTracker::new(),
        session_id: session_id.to_string(),
        file_history: Arc::new(parking_lot::Mutex::new(
            mikmik_core::file_history::FileHistory::new(),
        )),
        file_snapshots: Arc::new(parking_lot::Mutex::new(
            mikmik_core::file_snapshot::FileSnapshotStore::new(),
        )),
        current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
        inbox: AgentAddress {
            own: session_id.to_string(),
            parent: None,
            name: Some(mikmik_tools::MAIN_NAME.to_string()),
            parent_blocked: false,
        },
    }
}

async fn send(ctx: &ToolContext, to: &str, message: &str) -> ToolResult {
    mikmik_tools::SendMessageTool
        .execute(serde_json::json!({ "to": to, "message": message }), ctx)
        .await
}

/// The name the call reports is the one another agent can actually use. A
/// reported name that addressed nothing would be the same fault the tool had
/// before: a success message with nothing behind it.
#[tokio::test]
async fn a_background_agent_is_reachable_by_the_name_it_reports() {
    let session = "sess-e2e-background";
    let ctx = session_context(session);

    let started = mikmik_query::AgentTool
        .execute(
            serde_json::json!({
                "description": "watch the build",
                "name": "scout",
                "prompt": "wait",
                "run_in_background": true,
            }),
            &ctx,
        )
        .await;

    assert!(!started.is_error, "{}", started.content);

    let reported: serde_json::Value =
        serde_json::from_str(&started.content).expect("the result is JSON");
    let name = reported["agent_name"]
        .as_str()
        .expect("the call reports the name it assigned");
    assert_eq!(name, "scout");

    let sent = send(&ctx, name, "the build is green").await;
    assert!(
        !sent.is_error,
        "the reported name reached nothing: {}",
        sent.content
    );
}

/// Without a name of its own the agent would answer to the description, which
/// is free text and often a whole sentence.
#[tokio::test]
async fn an_unnamed_agent_still_gets_an_address() {
    let session = "sess-e2e-unnamed";
    let ctx = session_context(session);

    let started = mikmik_query::AgentTool
        .execute(
            serde_json::json!({
                "description": "Review the auth module",
                "prompt": "wait",
                "run_in_background": true,
            }),
            &ctx,
        )
        .await;

    assert!(!started.is_error, "{}", started.content);

    let reported: serde_json::Value =
        serde_json::from_str(&started.content).expect("the result is JSON");
    let name = reported["agent_name"]
        .as_str()
        .expect("a name was assigned");

    assert_eq!(name, "review-the-auth-module");
    assert!(!send(&ctx, name, "start with the token path").await.is_error);
}

/// `SendMessage` needs no permission, so nothing about this path prompts.
#[test]
fn messaging_is_not_a_gated_capability() {
    assert_eq!(
        mikmik_tools::SendMessageTool.permission_level(),
        PermissionLevel::None
    );
}
