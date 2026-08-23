// Post-sampling / stop hooks fired around each model turn.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

/// Result returned by `fire_post_sampling_hooks`.
#[derive(Debug, Default)]
pub struct PostSamplingHookResult {
    /// Error messages produced by hooks with non-zero exit codes.
    /// These are injected into the conversation as user messages before the
    /// next model turn so the model can react to them.
    pub blocking_errors: Vec<mikmik_core::types::Message>,
    /// When `true` the query loop must not continue and should surface the
    /// error messages to the caller.  Set when any hook exits with code > 1.
    pub prevent_continuation: bool,
}

/// Execute all `PostModelTurn` hooks defined in `config.hooks`.
///
/// Each hook is run synchronously (blocking via `std::process::Command`).
/// On a non-zero exit code, the hook's stderr (falling back to stdout) is
/// wrapped in a user `Message` and appended to `blocking_errors`.
/// If the exit code is **strictly greater than 1** `prevent_continuation` is
/// set so the query loop can return early.
pub fn fire_post_sampling_hooks(
    _turn_result: &mikmik_core::types::Message,
    config: &mikmik_core::config::Config,
) -> PostSamplingHookResult {
    use mikmik_core::config::HookEvent;
    use mikmik_core::types::Message;

    let mut result = PostSamplingHookResult::default();

    let entries = match config.hooks.get(&HookEvent::PostModelTurn) {
        Some(e) => e,
        None => return result,
    };

    for entry in entries {
        let sh = if cfg!(windows) { "cmd" } else { "sh" };
        let flag = if cfg!(windows) { "/C" } else { "-c" };

        let output = match std::process::Command::new(sh)
            .args([flag, &entry.command])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(command = %entry.command, error = %e, "PostModelTurn hook spawn failed");
                continue;
            }
        };

        if output.status.success() {
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let body = if !stderr.trim().is_empty() {
            stderr
        } else {
            stdout
        };

        tracing::warn!(
            command = %entry.command,
            exit_code = ?output.status.code(),
            "PostModelTurn hook returned non-zero exit"
        );

        result.blocking_errors.push(Message::user(format!(
            "[Hook '{}' error]:\n{}",
            entry.command,
            body.trim()
        )));

        // Exit code > 1 → hard veto of continuation.
        if output.status.code().unwrap_or(1) > 1 {
            result.prevent_continuation = true;
        }
    }

    result
}

/// Spawn all `Stop` hooks in fire-and-forget background tasks.
///
/// Stop hooks are non-blocking by design: the caller does not wait for them.
/// Returns an empty `Vec` immediately; results (if any) are lost.
pub fn stop_hooks_with_full_behavior(
    turn_result: &mikmik_core::types::Message,
    config: &mikmik_core::config::Config,
    working_dir: std::path::PathBuf,
) -> Vec<mikmik_core::types::Message> {
    use mikmik_core::config::HookEvent;

    let entries = match config.hooks.get(&HookEvent::Stop) {
        Some(e) if !e.is_empty() => e.clone(),
        _ => return Vec::new(),
    };

    let output_text = turn_result.get_all_text();

    for entry in entries {
        let cmd = entry.command.clone();
        let dir = working_dir.clone();
        let text = output_text.clone();

        tokio::task::spawn_blocking(move || {
            let sh = if cfg!(windows) { "cmd" } else { "sh" };
            let flag = if cfg!(windows) { "/C" } else { "-c" };

            let _ = std::process::Command::new(sh)
                .args([flag, &cmd])
                .current_dir(&dir)
                .env("CLAUDE_HOOK_OUTPUT", &text)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        });
    }

    Vec::new()
}

/// Run every `PreToolUse` hook, from settings and from plugins, for one tool.
///
/// Returns the error result the caller must substitute when a hook blocked the
/// call, and `None` when the tool may run. Both dispatch arms of the query loop
/// call this, so a hook fires no matter which provider served the turn.
pub(crate) async fn run_pre_tool_hooks(
    tool_ctx: &mikmik_tools::ToolContext,
    name: &str,
    input: &serde_json::Value,
) -> Option<mikmik_tools::ToolResult> {
    let hook_ctx = mikmik_core::hooks::HookContext {
        event: "PreToolUse".to_string(),
        tool_name: Some(name.to_string()),
        tool_input: Some(input.clone()),
        tool_output: None,
        is_error: None,
        session_id: Some(tool_ctx.session_id.clone()),
    };
    let outcome = mikmik_core::hooks::run_hooks(
        &tool_ctx.config.hooks,
        mikmik_core::config::HookEvent::PreToolUse,
        &hook_ctx,
        &tool_ctx.working_dir,
    )
    .await;

    if let mikmik_core::hooks::HookOutcome::Blocked(reason) = outcome {
        tracing::warn!(tool = %name, reason = %reason, "PreToolUse hook blocked execution");
        return Some(mikmik_tools::ToolResult::error(format!(
            "Blocked by hook: {}",
            reason
        )));
    }

    if let mikmik_plugins::HookOutcome::Deny(reason) =
        mikmik_plugins::run_global_pre_tool_hook(name, input).await
    {
        tracing::warn!(tool = %name, reason = %reason, "Plugin PreToolUse hook blocked execution");
        return Some(mikmik_tools::ToolResult::error(format!(
            "Blocked by plugin hook: {}",
            reason
        )));
    }

    None
}

/// What the project's conditional rules say about one tool call.
#[derive(Debug)]
pub(crate) enum RuleOutcome {
    /// No rule matched, or none of the matching ones may speak again.
    Silent,
    /// Run the call, then put this text on top of its result.
    Remind(String),
    /// Refuse the call and answer with this instead.
    Block(mikmik_tools::ToolResult),
}

/// Check one tool call against the project's conditional rules.
///
/// Runs where the arguments are complete JSON and the tool has not started, so
/// a rule can refuse the call rather than comment on what it already did.
pub(crate) async fn check_rules(
    tool_ctx: &mikmik_tools::ToolContext,
    name: &str,
    input: &serde_json::Value,
) -> RuleOutcome {
    if !tool_ctx.config.effective_rules_enabled() {
        return RuleOutcome::Silent;
    }

    let project_root = mikmik_core::session_storage::transcript_root_for(&tool_ctx.working_dir);
    let filenames = mikmik_core::claudemd::MemoryFilenames::from_config(&tool_ctx.config);
    let rules =
        mikmik_core::rules::rules_for(&project_root, filenames, &tool_ctx.config.rules_disabled);
    if rules.is_empty() {
        return RuleOutcome::Silent;
    }

    let turn = tool_ctx
        .current_turn
        .load(std::sync::atomic::Ordering::Relaxed) as u64;
    let matched = rules.match_tool(name, input);

    // A blocking rule wins over a reminding one: the call it refuses cannot
    // also carry a note on the result it never produces.
    let mut reminders: Vec<String> = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut spoke: Vec<String> = Vec::new();
    for rule in matched {
        if !mikmik_core::rules::claim(&tool_ctx.session_id, rule, turn) {
            continue;
        }
        tracing::info!(tool = %name, rule = %rule.name, "conditional rule matched");
        spoke.push(rule.name.clone());
        let rendered = mikmik_core::rules::render_rule(rule);
        match rule.action {
            mikmik_core::rules::RuleAction::Block => blocks.push(rendered),
            mikmik_core::rules::RuleAction::Remind => reminders.push(rendered),
        }
    }

    // On disk, so a resumed session does not say the same thing again about
    // work that is already done. A transcript that cannot be written is worth
    // a log line and nothing more: the rule still reached the model.
    if !spoke.is_empty() {
        match mikmik_core::session_storage::transcript_path(&project_root, &tool_ctx.session_id) {
            Ok(path) => {
                if let Err(e) =
                    mikmik_core::session_storage::append_rules_fired(&path, &spoke).await
                {
                    tracing::debug!("could not record which rules spoke: {e}");
                }
            }
            Err(e) => tracing::debug!("could not record which rules spoke: {e}"),
        }
    }

    if !blocks.is_empty() {
        // A reminder that matched the same call belongs in the refusal too,
        // because the call is not going to produce a result to carry it.
        blocks.extend(reminders);
        return RuleOutcome::Block(mikmik_tools::ToolResult::error(blocks.join("\n\n")));
    }
    if reminders.is_empty() {
        return RuleOutcome::Silent;
    }
    RuleOutcome::Remind(reminders.join("\n\n"))
}

/// Run every `PostToolUse` hook, from settings and from plugins, for one tool.
pub(crate) async fn run_post_tool_hooks(
    tool_ctx: &mikmik_tools::ToolContext,
    name: &str,
    input: &serde_json::Value,
    result: &mikmik_tools::ToolResult,
) {
    let hook_ctx = mikmik_core::hooks::HookContext {
        event: "PostToolUse".to_string(),
        tool_name: Some(name.to_string()),
        tool_input: Some(input.clone()),
        tool_output: Some(result.content.clone()),
        is_error: Some(result.is_error),
        session_id: Some(tool_ctx.session_id.clone()),
    };
    mikmik_core::hooks::run_hooks(
        &tool_ctx.config.hooks,
        mikmik_core::config::HookEvent::PostToolUse,
        &hook_ctx,
        &tool_ctx.working_dir,
    )
    .await;

    mikmik_plugins::run_global_post_tool_hook(name, input, &result.content, result.is_error).await;
}
