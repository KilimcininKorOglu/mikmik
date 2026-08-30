//! Test-only helpers for exercising a tool's `execute` against a permissive
//! `ToolContext` rooted at a caller-supplied (usually temp) directory.

use crate::ToolContext;
use std::path::PathBuf;
use std::sync::Arc;

/// `MIKMIK_HOME` is process-global, so the tests that redirect it run one at a
/// time and put it back afterwards.
pub(crate) static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Points `MIKMIK_HOME` at a directory for as long as it is held.
///
/// Take [`HOME_LOCK`] first: a test that writes into the config root and does
/// not redirect it fails CI's "the tests left the config root alone" step.
pub(crate) struct HomeGuard {
    saved: Option<std::ffi::OsString>,
}

impl HomeGuard {
    pub(crate) fn pointing_at(dir: &std::path::Path) -> Self {
        let saved = std::env::var_os("MIKMIK_HOME");
        std::env::set_var("MIKMIK_HOME", dir);
        Self { saved }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.saved {
            Some(value) => std::env::set_var("MIKMIK_HOME", value),
            None => std::env::remove_var("MIKMIK_HOME"),
        }
    }
}

/// Permission handler that approves everything, so `execute` runs unattended.
pub(crate) struct AllowAllHandler;

impl mikmik_core::permissions::PermissionHandler for AllowAllHandler {
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

/// Build a permissive, non-interactive `ToolContext` rooted at `working_dir`.
pub(crate) fn allow_all_context(working_dir: PathBuf) -> ToolContext {
    ToolContext {
        working_dir,
        permission_handler: Arc::new(AllowAllHandler),
        cost_tracker: mikmik_core::cost::CostTracker::new(),
        session_id: "eol-test".to_string(),
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
