//! The scripting bridge, from a tool call to a real `node` process.
//!
//! The unit tests cover the message shapes. They leave the part that actually
//! has to work untested: a node process that connects back, runs the code,
//! calls home while it runs, and keeps its variables for the next call. Every
//! one of those is a place the bridge can be wired up wrong and still compile.
//!
//! Skipped where `node` is absent, which is also where the roster withholds
//! the tool.

#![cfg(feature = "computer-use")]

use std::sync::Arc;

use mikmik_tools::{Tool, ToolContext};
use serde_json::json;

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

fn session(session_id: &str) -> ToolContext {
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
        inbox: Default::default(),
    }
}

fn node_is_available() -> bool {
    which::which("node").is_ok()
}

async fn run(ctx: &ToolContext, code: &str) -> mikmik_tools::ToolResult {
    mikmik_tools::ComputerScriptTool
        .execute(json!({ "code": code, "timeout": 30 }), ctx)
        .await
}

#[tokio::test]
async fn the_bridge_runs_code_and_brings_back_what_it_printed() {
    if !node_is_available() {
        return;
    }
    let ctx = session("script-basic");

    let result = run(&ctx, "print('hello from the session')").await;
    mikmik_tools::computer_script::shutdown_session(&ctx.session_id).await;

    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("hello from the session"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn a_returned_value_comes_back_with_the_output() {
    if !node_is_available() {
        return;
    }
    let ctx = session("script-value");

    let result = run(&ctx, "return 6 * 7").await;
    mikmik_tools::computer_script::shutdown_session(&ctx.session_id).await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("42"), "{}", result.content);
}

#[tokio::test]
async fn state_survives_between_calls() {
    // The whole reason this tool exists beside `computer`: one call reads the
    // screen, the next acts on what it found, without paying a turn between.
    if !node_is_available() {
        return;
    }
    let ctx = session("script-state");

    let first = run(&ctx, "remembered = 17").await;
    let second = run(&ctx, "print(remembered + 1)").await;
    mikmik_tools::computer_script::shutdown_session(&ctx.session_id).await;

    assert!(!first.is_error, "{}", first.content);
    assert!(!second.is_error, "{}", second.content);
    assert!(second.content.contains("18"), "{}", second.content);
}

#[tokio::test]
async fn a_thrown_error_comes_back_as_an_error() {
    if !node_is_available() {
        return;
    }
    let ctx = session("script-throw");

    let result = run(&ctx, "throw new Error('deliberate')").await;
    mikmik_tools::computer_script::shutdown_session(&ctx.session_id).await;

    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("deliberate"), "{}", result.content);
}

#[tokio::test]
async fn a_host_call_reaches_the_host_and_answers_back() {
    // `clipboard()` proves the round trip: the code calls home while it is
    // still running, and the answer arrives as a value. It is the one reading
    // op that needs no macOS permission grant. `displays()` and `screenshot()`
    // go through ScreenCaptureKit, which blocks for tens of seconds when the
    // calling binary has no screen-recording grant, and a test binary is
    // rebuilt under a new hash often enough that it never has one.
    if !node_is_available() {
        return;
    }
    let ctx = session("script-host");

    let result = run(&ctx, "print(typeof (await clipboard()) === 'string')").await;
    mikmik_tools::computer_script::shutdown_session(&ctx.session_id).await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("true"), "{}", result.content);
}

#[tokio::test]
async fn read_only_refuses_a_write_and_allows_a_read() {
    if !node_is_available() {
        return;
    }
    let ctx = session("script-readonly");

    let refused = mikmik_tools::ComputerScriptTool
        .execute(
            json!({ "code": "await click(10, 10)", "read_only": true, "timeout": 30 }),
            &ctx,
        )
        .await;
    let allowed = mikmik_tools::ComputerScriptTool
        .execute(
            json!({ "code": "print(typeof (await clipboard()) === 'string')", "read_only": true, "timeout": 30 }),
            &ctx,
        )
        .await;
    mikmik_tools::computer_script::shutdown_session(&ctx.session_id).await;

    assert!(refused.is_error, "{}", refused.content);
    assert!(refused.content.contains("read_only"), "{}", refused.content);
    assert!(!allowed.is_error, "{}", allowed.content);
}

#[tokio::test]
async fn a_call_that_never_finishes_gives_the_turn_back() {
    if !node_is_available() {
        return;
    }
    let ctx = session("script-timeout");

    let started = std::time::Instant::now();
    let result = mikmik_tools::ComputerScriptTool
        .execute(
            json!({ "code": "await new Promise(() => {})", "timeout": 1 }),
            &ctx,
        )
        .await;
    let waited = started.elapsed();
    mikmik_tools::computer_script::shutdown_session(&ctx.session_id).await;

    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("1s"), "{}", result.content);
    // The elapsed time is asserted as well as the message. The message quotes
    // the limit that was asked for, so it reads the same whatever deadline the
    // loop actually used: a deadline fifteen times too long still printed "1s".
    // The margin covers starting `node`, which happens inside this window.
    assert!(
        waited < std::time::Duration::from_secs(10),
        "the turn came back after {waited:?}, not near the 1s that was asked for"
    );
}

#[tokio::test]
async fn an_ax_call_routes_through_the_bridge_to_the_backend() {
    // The whole `ax` path in one call, without touching the flaky part of the
    // platform. `ax.get` on a handle the store never held travels the same
    // route a real read would (runner `ax.get` -> host `ax_get` -> the ax arm
    // of the dispatcher -> the backend's `get`), and the backend answers
    // `UnknownHandle` from the store *before* it makes any platform call. So
    // this pins the wiring deterministically: a routed op comes back as a
    // caught "no element" error, fast, while an unrouted one would come back
    // "unknown host call". A real tree read is verified by hand on macOS, where
    // it returns a live application's role and title.
    if !node_is_available() {
        return;
    }
    let ctx = session("script-ax");

    let result = run(
        &ctx,
        "try { await ax.get('ax-none', 'AXValue'); print('no throw'); } \
         catch (e) { print('caught ' + e.message); }",
    )
    .await;
    mikmik_tools::computer_script::shutdown_session(&ctx.session_id).await;

    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("caught ") && result.content.contains("ax-none"),
        "the ax op did not reach the backend's handle check: {}",
        result.content
    );
    assert!(
        !result.content.contains("unknown host call"),
        "the ax op was not routed: {}",
        result.content
    );
}
