// PowerShell tool: execute PowerShell commands (Windows-native).
//
// On Windows, PowerShell provides richer scripting than cmd.exe.
// On non-Windows platforms, attempts to use `pwsh` (PowerShell Core).
//
// Security model
// ──────────────
// Before any execution the command is passed through `classify_ps_command`.
// The resulting `PsRiskLevel` drives the permission gate:
//
//   Critical → always blocked (hard error, never executed)
//   High     → requires explicit user approval (once / session / deny)
//   Medium   → requires approval only when ctx.require_confirmation is set
//   Low      → executes directly

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use mikmik_core::ps_classifier::{classify_ps_command, PsRiskLevel};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::debug;

pub struct PowerShellTool;

#[derive(Debug, Deserialize)]
struct PowerShellInput {
    command: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_timeout")]
    timeout: u64,
    /// When true, Medium-risk commands also prompt for approval.
    #[serde(default)]
    require_confirmation: bool,
}

fn default_timeout() -> u64 {
    120_000
}

// ---------------------------------------------------------------------------
// Risk-label helpers (used in messages shown to the user)
// ---------------------------------------------------------------------------

fn risk_label(level: PsRiskLevel) -> &'static str {
    match level {
        PsRiskLevel::Critical => "Critical",
        PsRiskLevel::High => "High",
        PsRiskLevel::Medium => "Medium",
        PsRiskLevel::Low => "Low",
    }
}

fn risk_explanation(level: PsRiskLevel, command: &str) -> String {
    match level {
        PsRiskLevel::Critical => format!(
            "PowerShell command classified as CRITICAL risk — execution blocked.\n\
             Reason: the command contains destructive or remote-code-execution patterns.\n\
             Command: {}",
            command
        ),
        PsRiskLevel::High => {
            "[High risk] This may modify system-wide security policy, the registry (HKLM), user accounts, or firewall rules.".to_string()
        }
        PsRiskLevel::Medium => {
            "[Medium risk] This may delete files, control services, or make network requests.".to_string()
        }
        PsRiskLevel::Low => String::new(), // never shown
    }
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for PowerShellTool {
    // Gates itself: calls `ctx.check_permission*` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_POWERSHELL
    }

    fn description(&self) -> &str {
        "Execute a PowerShell command. Use for Windows-native operations, .NET APIs, \
         registry access, and Windows-specific system administration."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The PowerShell command or script to execute"
                },
                "description": {
                    "type": "string",
                    "description": "Description of what this command does"
                },
                "timeout": {
                    "type": "number",
                    "description": "Timeout in ms (default 120000, max 600000)"
                },
                "require_confirmation": {
                    "type": "boolean",
                    "description": "When true, Medium-risk commands also prompt for approval"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: PowerShellInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // ── Step 1: classify the command ─────────────────────────────────────
        let risk = classify_ps_command(&params.command);

        // ── Step 2: apply the risk gate ──────────────────────────────────────
        match risk {
            PsRiskLevel::Critical => {
                // Hard block — never executed regardless of permission mode.
                return ToolResult::error(risk_explanation(PsRiskLevel::Critical, &params.command));
            }

            PsRiskLevel::High => {
                // Require explicit user permission (same once/session/deny
                // pattern as BashTool: delegate to ctx.check_permission which
                // in interactive mode shows the TUI dialog).
                let desc = format!(
                    "[{} risk] {}",
                    risk_label(risk),
                    params.description.as_deref().unwrap_or(&params.command)
                );
                let details = risk_explanation(PsRiskLevel::High, &params.command);
                if let Err(e) = ctx.check_permission_with_details_and_path(
                    self.name(),
                    &desc,
                    &details,
                    std::path::PathBuf::from(&params.command),
                    false,
                ) {
                    return ToolResult::error(e.to_string());
                }
            }

            PsRiskLevel::Medium => {
                // Only gate if the caller set require_confirmation, or if the
                // context permission mode is Default (non-bypass, non-accept).
                let needs_gate = params.require_confirmation
                    || matches!(
                        ctx.permission_mode,
                        mikmik_core::config::PermissionMode::Default
                            | mikmik_core::config::PermissionMode::Plan
                    );

                if needs_gate {
                    let desc = format!(
                        "[{} risk] {}",
                        risk_label(risk),
                        params.description.as_deref().unwrap_or(&params.command)
                    );
                    let details = risk_explanation(PsRiskLevel::Medium, &params.command);
                    if let Err(e) = ctx.check_permission_with_details_and_path(
                        self.name(),
                        &desc,
                        &details,
                        std::path::PathBuf::from(&params.command),
                        false,
                    ) {
                        return ToolResult::error(e.to_string());
                    }
                }
            }

            PsRiskLevel::Low => {
                // Standard (non-risk-gated) permission check — honours bypass
                // and plan-mode rules, but does not show a dialog.
                let reason = params
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("This will execute a PowerShell command.")
                    .to_string();

                if let Err(e) = ctx.check_permission_for_path(
                    self.name(),
                    &reason,
                    std::path::PathBuf::from(&params.command),
                    false,
                ) {
                    return ToolResult::error(e.to_string());
                }
            }
        }

        // ── Step 3: execute ──────────────────────────────────────────────────
        //
        // In the session's own interpreter, which stays open, so a variable, a
        // `cd` and an imported module outlive the command that made them.
        debug!(
            command = %params.command,
            risk    = ?risk,
            "Executing PowerShell command"
        );

        let timeout_ms = params.timeout.min(600_000);
        let timeout_dur = Duration::from_millis(timeout_ms);

        let session = match crate::session_powershell(&ctx.session_id, &ctx.working_dir) {
            Ok(session) => session,
            Err(error) => return ToolResult::error(format!("Failed to spawn PowerShell: {error}")),
        };

        let ran = {
            let mut session = session.lock().await;
            session.run(&params.command, timeout_dur).await
        };

        let ran = match ran {
            Ok(ran) => ran,
            Err(error) => {
                // The interpreter was killed, so the session is finished with
                // it; the next call starts a new one.
                crate::drop_session_powershell(&ctx.session_id);
                return ToolResult::error(format!("PowerShell command failed: {error}"));
            }
        };

        let mut output = ran.output.trim_end().to_string();
        if !ran.errors.trim().is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("STDERR:\n");
            output.push_str(ran.errors.trim_end());
        }
        if output.is_empty() {
            output = "(no output)".to_string();
        }

        // Truncate very long output (same limit as BashTool)
        const MAX_OUTPUT_LEN: usize = 100_000;
        if output.len() > MAX_OUTPUT_LEN {
            let half = MAX_OUTPUT_LEN / 2;
            let start = &output[..half];
            let end = &output[output.len() - half..];
            output = format!(
                "{}\n\n... ({} characters truncated) ...\n\n{}",
                start,
                output.len() - MAX_OUTPUT_LEN,
                end
            );
        }

        if ran.exit_code == 0 {
            ToolResult::success(output)
        } else {
            ToolResult::error(format!(
                "PowerShell exited with code {}\n{}",
                ran.exit_code, output
            ))
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    /// A sleep duration no other run can be using, so a process left behind by
    /// an earlier run is never read as this one's.
    fn unique_marker() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        // Fractional seconds keep the number one `sleep` accepts.
        format!("999337.{}", nanos % 1_000_000_000)
    }

    fn pgrep_matches(marker: &str) -> bool {
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .map(|out| !out.stdout.is_empty())
            .unwrap_or(false)
    }

    fn bypassing_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            permission_mode: mikmik_core::config::PermissionMode::BypassPermissions,
            permission_handler: std::sync::Arc::new(
                mikmik_core::permissions::AutoPermissionHandler {
                    mode: mikmik_core::config::PermissionMode::BypassPermissions,
                },
            ),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            // A session of its own per test. The interpreter belongs to the
            // runtime that started it, and each `#[tokio::test]` builds its
            // own, so a shared one would be reached after its runtime had
            // gone. One process running MikMik has one runtime, so this is a
            // property of the tests rather than of the tool.
            session_id: format!("powershell-test-{}", uuid::Uuid::new_v4()),
            file_history: std::sync::Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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

    /// The timeout path used to kill the interpreter and leave the process the
    /// script actually started running. This is not a Windows-only hole: the
    /// `pwsh` path had no drop guard either.
    #[tokio::test]
    async fn a_timed_out_script_takes_its_children_with_it() {
        if which::which("pwsh").is_err() {
            eprintln!("skipped: pwsh is not installed on this machine");
            return;
        }
        let marker = unique_marker();

        let result = PowerShellTool
            .execute(
                json!({
                    "command": format!("Start-Process -NoNewWindow sleep -ArgumentList '{marker}'; Start-Sleep -Seconds 60"),
                    "timeout": 1_500u64,
                }),
                &bypassing_ctx(),
            )
            .await;

        assert!(
            result.is_error,
            "expected a timeout, got: {}",
            result.content
        );
        assert!(result.content.contains("timed out"), "{}", result.content);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pgrep_matches(&marker) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !pgrep_matches(&marker),
            "the script's child survived the timeout"
        );
    }
    /// The guard must not fire when the command succeeded: a script that
    /// deliberately leaves something running would otherwise have it killed the
    /// moment the tool returned.
    #[tokio::test]
    async fn a_script_that_finished_keeps_what_it_left_running() {
        if which::which("pwsh").is_err() {
            eprintln!("skipped: pwsh is not installed on this machine");
            return;
        }
        let marker = unique_marker();
        let err_file = std::env::temp_dir().join(format!("mikmik-ps-{marker}.err"));

        let result = PowerShellTool
            .execute(
                json!({
                    // The child's output goes elsewhere: it would otherwise
                    // hold the interpreter's pipes open and the tool would read
                    // until the timeout instead of returning. The two streams
                    // must differ, which is why one goes to a file.
                    "command": format!(
                        "Start-Process -NoNewWindow sleep -ArgumentList '{marker}' \
                         -RedirectStandardOutput /dev/null -RedirectStandardError {}; \
                         Start-Sleep -Milliseconds 300",
                        err_file.display()
                    ),
                    "timeout": 30_000u64,
                }),
                &bypassing_ctx(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        tokio::time::sleep(Duration::from_millis(500)).await;
        let survived = pgrep_matches(&marker);
        // Clean up before asserting, so a failure does not leave the process.
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(&marker)
            .output();
        let _ = std::fs::remove_file(&err_file);
        assert!(survived, "a completed command had its leftovers killed");
    }

    /// The whole point of holding the interpreter open: what one call leaves
    /// behind, the next one sees.
    #[tokio::test]
    async fn state_outlives_the_call_that_made_it() {
        if which::which("pwsh").is_err() {
            eprintln!("skipped: pwsh is not installed on this machine");
            return;
        }
        let ctx = bypassing_ctx();

        let set = PowerShellTool
            .execute(json!({"command": "$mikmik_probe = 'kept'"}), &ctx)
            .await;
        assert!(!set.is_error, "{}", set.content);

        let read = PowerShellTool
            .execute(json!({"command": "$mikmik_probe"}), &ctx)
            .await;
        assert!(!read.is_error, "{}", read.content);
        assert_eq!(read.content.trim(), "kept");

        crate::clear_session_shell_state(&ctx.session_id);
    }

    /// A session that ended takes its interpreter with it, so the next one
    /// starts from nothing rather than inheriting a stranger's variables.
    #[tokio::test]
    async fn ending_the_session_ends_the_interpreter() {
        if which::which("pwsh").is_err() {
            eprintln!("skipped: pwsh is not installed on this machine");
            return;
        }
        let ctx = bypassing_ctx();

        PowerShellTool
            .execute(json!({"command": "$mikmik_probe = 'kept'"}), &ctx)
            .await;
        crate::clear_session_shell_state(&ctx.session_id);

        let read = PowerShellTool
            .execute(json!({"command": "\"[$mikmik_probe]\""}), &ctx)
            .await;
        assert_eq!(read.content.trim(), "[]");

        crate::clear_session_shell_state(&ctx.session_id);
    }

    /// A command that ran too long leaves the interpreter killed, so the next
    /// call has to start a new one rather than talk to a corpse.
    #[tokio::test]
    async fn the_session_recovers_from_a_timeout() {
        if which::which("pwsh").is_err() {
            eprintln!("skipped: pwsh is not installed on this machine");
            return;
        }
        let ctx = bypassing_ctx();

        let timed_out = PowerShellTool
            .execute(
                json!({"command": "Start-Sleep -Seconds 60", "timeout": 1_000u64}),
                &ctx,
            )
            .await;
        assert!(timed_out.is_error);
        assert!(
            timed_out.content.contains("timed out"),
            "{}",
            timed_out.content
        );

        let after = PowerShellTool
            .execute(json!({"command": "Write-Output 'a new one'"}), &ctx)
            .await;
        assert!(!after.is_error, "{}", after.content);
        assert_eq!(after.content.trim(), "a new one");

        crate::clear_session_shell_state(&ctx.session_id);
    }
}
