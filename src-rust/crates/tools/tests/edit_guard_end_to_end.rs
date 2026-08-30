//! The edit guard from a real `settings.json` to a real refusal.
//!
//! The unit tests set `config.edit_guard` directly. That leaves one link
//! untested, and it is the one a user actually touches: the file on disk, read
//! through `Settings`, copied into `Config`, and reaching the tool. A key that
//! parses but never lands there is a setting that looks configured and does
//! nothing.

use std::sync::Arc;

use mikmik_tools::{Tool, ToolContext};
use serde_json::json;

/// Allows everything, so a refusal in these tests can only come from the guard.
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

/// A session whose config came from the given `settings.json` text.
fn session(settings_json: &str, cwd: std::path::PathBuf) -> ToolContext {
    let settings: mikmik_core::config::Settings =
        serde_json::from_str(settings_json).expect("the settings file must parse");
    ToolContext {
        working_dir: cwd,
        permission_handler: Arc::new(AllowAll),
        cost_tracker: mikmik_core::cost::CostTracker::new(),
        session_id: "edit-guard-test".to_string(),
        file_history: Arc::new(parking_lot::Mutex::new(
            mikmik_core::file_history::FileHistory::new(),
        )),
        file_snapshots: Arc::new(parking_lot::Mutex::new(
            mikmik_core::file_snapshot::FileSnapshotStore::new(),
        )),
        current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        non_interactive: true,
        mcp_manager: None,
        config: settings.config,
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

const STRICT: &str = r#"{ "version": 1, "config": { "editGuard": "strict" } }"#;
const DEFAULT: &str = r#"{ "version": 1 }"#;

async fn read(ctx: &ToolContext, path: &std::path::Path, args: serde_json::Value) {
    let mut input = json!({ "file_path": path.to_string_lossy() });
    if let (Some(target), Some(extra)) = (input.as_object_mut(), args.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    let result = mikmik_tools::FileReadTool.execute(input, ctx).await;
    assert!(!result.is_error, "read failed: {}", result.content);
}

async fn edit(
    ctx: &ToolContext,
    path: &std::path::Path,
    old: &str,
    new: &str,
) -> mikmik_tools::ToolResult {
    mikmik_tools::FileEditTool
        .execute(
            json!({
                "file_path": path.to_string_lossy(),
                "old_string": old,
                "new_string": new,
            }),
            ctx,
        )
        .await
}

#[tokio::test]
async fn a_settings_file_asking_for_strict_refuses_a_blind_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write");

    let ctx = session(STRICT, dir.path().to_path_buf());
    read(&ctx, &path, json!({ "offset": 1, "limit": 2 })).await;

    let refused = edit(&ctx, &path, "four", "FOUR").await;
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.contains("never displayed"),
        "{}",
        refused.content
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read back"),
        "one\ntwo\nthree\nfour\nfive\n",
        "the file was written despite the refusal"
    );

    let allowed = edit(&ctx, &path, "two", "TWO").await;
    assert!(!allowed.is_error, "{}", allowed.content);
}

#[tokio::test]
async fn a_settings_file_asking_for_strict_refuses_an_edit_to_a_changed_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one\ntwo\n").expect("write");

    let ctx = session(STRICT, dir.path().to_path_buf());
    read(&ctx, &path, json!({})).await;

    // Something outside the session moves the file.
    std::fs::write(&path, "one\ntwo\nthree\n").expect("rewrite");

    let refused = edit(&ctx, &path, "one", "ONE").await;
    assert!(refused.is_error, "{}", refused.content);
    assert!(
        refused.content.contains("changed after"),
        "{}",
        refused.content
    );
}

/// The upgrade case: a settings file that never mentions the key must behave
/// exactly as it did before the guard existed.
#[tokio::test]
async fn a_settings_file_that_says_nothing_checks_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one\ntwo\n").expect("write");

    let ctx = session(DEFAULT, dir.path().to_path_buf());
    assert_eq!(
        ctx.config.effective_edit_guard(),
        mikmik_core::file_snapshot::EditGuard::Off
    );
    read(&ctx, &path, json!({ "offset": 1, "limit": 1 })).await;
    std::fs::write(&path, "one\ntwo\nthree\n").expect("rewrite");

    let allowed = edit(&ctx, &path, "two", "TWO").await;
    assert!(!allowed.is_error, "{}", allowed.content);
    assert!(std::fs::read_to_string(&path)
        .expect("read back")
        .contains("TWO"));
}
