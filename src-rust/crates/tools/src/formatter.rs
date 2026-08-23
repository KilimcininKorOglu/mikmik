// formatter.rs — run a configured file formatter after writes/edits.

use crate::ToolContext;

/// Try to format a file using any configured formatter.
/// Returns silently if no formatter is configured or the formatter fails.
pub async fn try_format_file(path: &str, ctx: &ToolContext) {
    let formatters = &ctx.config.formatter;
    if formatters.is_empty() {
        return;
    }

    // Determine the file's extension (with leading dot, e.g. ".ts").
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e))
        .unwrap_or_default();

    for fmt in formatters.values() {
        if fmt.disabled || fmt.command.is_empty() {
            continue;
        }
        if !fmt.extensions.iter().any(|e| e == &ext) {
            continue;
        }

        let mut cmd = tokio::process::Command::new(&fmt.command[0]);
        let mut file_injected = false;
        for arg in &fmt.command[1..] {
            if arg == "$FILE" || arg == "{file}" {
                cmd.arg(path);
                file_injected = true;
            } else {
                cmd.arg(arg);
            }
        }
        // Append the file path if no explicit placeholder was present.
        if !file_injected {
            cmd.arg(path);
        }

        // The formatter is only the wrapper for whatever it starts, and a
        // timeout that drops the future leaves all of it running. Guard the
        // tree so a cancelled turn or an expired limit takes the whole thing.
        mikmik_core::process_tree::spawn_in_own_group(&mut cmd);
        let Ok(child) = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            // Silently ignored, as every other formatter failure is: a file
            // that did not get formatted is not worth interrupting a turn for.
            // `break`, not `continue`: the first matching formatter is the only
            // one that runs, whether or not it started.
            break;
        };
        let mut tree_guard = mikmik_core::process_tree::ProcessTreeKillGuard::new(child.id());
        match tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
            .await
        {
            Ok(_) => tree_guard.disarm(),
            Err(_) => tree_guard.kill_now(),
        }

        // Only apply the first matching formatter.
        break;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mikmik_core::config::FormatterConfig;

    /// A sleep duration no other run can be using, so a process left behind by
    /// an earlier run is never read as this one's.
    fn unique_marker() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        // Fractional seconds keep the number one `sleep` accepts.
        format!("999333.{}", nanos % 1_000_000_000)
    }

    fn pgrep_matches(marker: &str) -> bool {
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
            .map(|out| !out.stdout.is_empty())
            .unwrap_or(false)
    }

    fn ctx_with_formatter(command: Vec<String>) -> ToolContext {
        let mut config = mikmik_core::config::Config::default();
        config.formatter.insert(
            "test".to_string(),
            FormatterConfig {
                command,
                extensions: vec![".txt".to_string()],
                disabled: false,
            },
        );
        ToolContext {
            working_dir: std::env::temp_dir(),
            permission_mode: mikmik_core::config::PermissionMode::BypassPermissions,
            permission_handler: std::sync::Arc::new(
                mikmik_core::permissions::AutoPermissionHandler {
                    mode: mikmik_core::config::PermissionMode::BypassPermissions,
                },
            ),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "formatter-test".to_string(),
            file_history: std::sync::Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config,
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

    /// A cancelled turn used to leave the formatter and everything it started
    /// running: there was a time limit but nothing killed anything on drop.
    #[tokio::test]
    async fn a_cancelled_format_takes_the_formatter_with_it() {
        let marker = unique_marker();
        let ctx = ctx_with_formatter(vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("sleep {marker} & wait"),
        ]);

        {
            let running = try_format_file("/tmp/whatever.txt", &ctx);
            tokio::pin!(running);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), &mut running).await;
            assert!(pgrep_matches(&marker), "the formatter never started");
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pgrep_matches(&marker) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !pgrep_matches(&marker),
            "the formatter's child survived the cancel"
        );
    }
}
