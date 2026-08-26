// monitor_tool.rs — Monitor background tasks
//
// Provides a "monitor" tool that lets the agent inspect background tasks
// started via BashTool with run_in_background=true.  Supports listing all
// tasks, checking the status or output of a specific task, and cancelling a
// running task.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use mikmik_core::tasks::{global_registry, TaskStatus};
use serde::Deserialize;
use serde_json::{json, Value};

pub struct MonitorTool;

#[derive(Deserialize)]
struct MonitorInput {
    #[serde(default)]
    action: MonitorAction,
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum MonitorAction {
    #[default]
    List,
    Status,
    Output,
    Cancel,
}

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &str {
        "monitor"
    }

    fn description(&self) -> &str {
        "Monitor background tasks started with run_in_background=true. \
        List all tasks, check status, retrieve output, or cancel a running task."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "status", "output", "cancel"],
                    "description": "Action to perform. 'list' shows all tasks, 'status'/'output' inspect a specific task, 'cancel' terminates a running task.",
                    "default": "list"
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID to inspect or cancel. Required for status, output, cancel actions."
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let parsed: MonitorInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        match parsed.action {
            MonitorAction::List => {
                let tasks = global_registry().list();
                if tasks.is_empty() {
                    return ToolResult::success("No background tasks.");
                }
                let mut lines = vec!["Background tasks:".to_string()];
                for t in &tasks {
                    let status = match &t.status {
                        TaskStatus::Running => "running".to_string(),
                        TaskStatus::Completed => "completed".to_string(),
                        TaskStatus::Failed(msg) => format!("failed: {}", msg),
                        TaskStatus::Cancelled => "cancelled".to_string(),
                    };
                    lines.push(format!("  {} [{}] {}", t.id, status, t.name));
                }
                ToolResult::success(lines.join("\n"))
            }

            MonitorAction::Status => {
                let id = match parsed.task_id {
                    Some(id) => id,
                    None => return ToolResult::error("task_id required for status action"),
                };
                match global_registry().get(&id) {
                    None => ToolResult::error(format!("Task {} not found", id)),
                    Some(t) => {
                        let status = match &t.status {
                            TaskStatus::Running => "running".to_string(),
                            TaskStatus::Completed => "completed (exit 0)".to_string(),
                            TaskStatus::Failed(msg) => format!("failed: {}", msg),
                            TaskStatus::Cancelled => "cancelled".to_string(),
                        };
                        ToolResult::success(format!(
                            "Task: {}\nStatus: {}\nCommand: {}\nOutput lines: {}",
                            t.id,
                            status,
                            t.name,
                            t.output.len()
                        ))
                    }
                }
            }

            MonitorAction::Output => {
                let id = match parsed.task_id {
                    Some(id) => id,
                    None => return ToolResult::error("task_id required for output action"),
                };
                match global_registry().get(&id) {
                    None => ToolResult::error(format!("Task {} not found", id)),
                    Some(t) => {
                        let output = t.output.join("\n");
                        if output.is_empty() {
                            ToolResult::success("(no output yet)")
                        } else {
                            ToolResult::success(output)
                        }
                    }
                }
            }

            MonitorAction::Cancel => {
                let id = match parsed.task_id {
                    Some(id) => id,
                    None => return ToolResult::error("task_id required for cancel action"),
                };
                match global_registry().get(&id) {
                    None => ToolResult::error(format!("Task {} not found", id)),
                    Some(t) => {
                        if let TaskStatus::Running = t.status {
                            // Stop the whole tree, not the recorded pid alone:
                            // a background command runs under a shell, so the
                            // pid names the wrapper and killing it left the
                            // work the user asked to stop still running.
                            if let Some(pid) = t.pid {
                                mikmik_core::process_tree::terminate_tree(pid);
                            }
                            // Signal the task's cancellation token (if any) and
                            // mark it Cancelled. For an in-process background
                            // agent this is what actually stops the running loop
                            // (issue #219); the pid-kill above covers external
                            // OS child processes.
                            global_registry().cancel(&id);
                            ToolResult::success(format!("Task {} cancelled.", id))
                        } else {
                            ToolResult::error(format!(
                                "Task {} is not running (status: {})",
                                id, t.status
                            ))
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_tool_name() {
        assert_eq!(MonitorTool.name(), "monitor");
    }

    #[test]
    fn monitor_schema_has_action_and_task_id() {
        let schema = MonitorTool.input_schema();
        let props = &schema["properties"];
        assert!(
            props["action"].is_object(),
            "schema should have 'action' property"
        );
        assert!(
            props["task_id"].is_object(),
            "schema should have 'task_id' property"
        );
    }

    #[test]
    fn monitor_schema_is_object() {
        let schema = MonitorTool.input_schema();
        assert!(schema.is_object());
        assert!(schema.get("properties").is_some());
    }

    #[tokio::test]
    async fn monitor_list_empty() {
        // The global registry is shared across tests, so we just verify the
        // tool runs without panicking and returns a success result.
        let tool = MonitorTool;
        let input = json!({"action": "list"});
        let ctx = make_test_ctx();
        let result = tool.execute(input, &ctx).await;
        // Either "No background tasks." or a list — both are successes.
        assert!(
            !result.is_error,
            "list action should not return an error: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn monitor_status_missing_task_id() {
        let tool = MonitorTool;
        let input = json!({"action": "status"});
        let ctx = make_test_ctx();
        let result = tool.execute(input, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("task_id required"));
    }

    #[tokio::test]
    async fn monitor_output_unknown_task() {
        let tool = MonitorTool;
        let input = json!({"action": "output", "task_id": "nonexistent-uuid-1234"});
        let ctx = make_test_ctx();
        let result = tool.execute(input, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn monitor_cancel_unknown_task() {
        let tool = MonitorTool;
        let input = json!({"action": "cancel", "task_id": "nonexistent-uuid-5678"});
        let ctx = make_test_ctx();
        let result = tool.execute(input, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    fn make_test_ctx() -> ToolContext {
        use mikmik_core::config::Config;
        use mikmik_core::permissions::AutoPermissionHandler;
        use std::path::PathBuf;
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let handler = Arc::new(AutoPermissionHandler {
            mode: mikmik_core::config::PermissionMode::Default,
        });
        ToolContext {
            working_dir: PathBuf::from("."),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: handler,
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "test-monitor".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: Arc::new(AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: Config::default(),
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
    /// Cancelling a task used to reach the recorded pid alone, which for a
    /// background command is the shell wrapper, so the work the user asked to
    /// stop kept running.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn cancelling_a_task_stops_what_its_shell_started() {
        let marker = unique_marker();
        // Driven through the tool, so the test exercises the path a task
        // actually takes into the registry.
        let started = crate::pty_bash::PtyBashTool
            .execute(
                json!({
                    // `/bin/sleep` spelled out: the carried `sleep` runs in
                    // this process and starts nothing for a cancel to reach,
                    // and what this test is about is the child process.
                    "command": format!("/bin/sleep {marker} & wait"),
                    "run_in_background": true,
                }),
                &bypassing_ctx(),
            )
            .await;
        assert!(!started.is_error, "{}", started.content);

        let task_id = started
            .content
            .lines()
            .find_map(|line| line.strip_prefix("Task ID: "))
            .expect("the tool reports the task id")
            .to_string();

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(pgrep_matches(&marker), "the child never started");

        let result = MonitorTool
            .execute(
                json!({"action": "cancel", "task_id": task_id}),
                &make_test_ctx(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pgrep_matches(&marker) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !pgrep_matches(&marker),
            "the shell's child survived the cancel"
        );
    }

    /// The bash tool asks before running; this test is about what a cancel
    /// kills, not about the prompt.
    #[cfg(not(windows))]
    fn bypassing_ctx() -> ToolContext {
        let mut ctx = make_test_ctx();
        ctx.permission_handler =
            std::sync::Arc::new(mikmik_core::permissions::AutoPermissionHandler {
                mode: mikmik_core::config::PermissionMode::BypassPermissions,
            });
        ctx
    }

    /// A sleep duration no other run can be using.
    ///
    /// A fixed marker made the test fail whenever an earlier run had left a
    /// process behind: `pgrep` found that one and read it as this run's.
    #[cfg(not(windows))]
    fn unique_marker() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        // Fractional seconds keep the number one `sleep` accepts.
        format!("999339.{}", nanos % 1_000_000_000)
    }

    #[cfg(not(windows))]
    fn pgrep_matches(marker: &str) -> bool {
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .map(|out| !out.stdout.is_empty())
            .unwrap_or(false)
    }
}
