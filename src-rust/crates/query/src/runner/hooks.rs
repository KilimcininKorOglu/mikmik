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

/// What the caller must do after the `PostModelTurn` hooks ran.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PostModelTurn {
    /// Nothing to do; the turn continues as normal.
    Continue,
    /// A hook exited above 1. Its messages are already in the conversation and
    /// the loop must return.
    Veto,
}

/// Run the `PostModelTurn` hooks for one turn and apply what they said.
///
/// Both dispatch arms call this. It used to live inline in the Anthropic arm
/// alone, so a user's `PostModelTurn` hook never fired on any other account.
pub(crate) fn apply_post_model_turn(
    assistant_msg: &mikmik_core::types::Message,
    tool_ctx: &mikmik_tools::ToolContext,
    messages: &mut Vec<mikmik_core::types::Message>,
    event_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::QueryEvent>>,
) -> PostModelTurn {
    let result = fire_post_sampling_hooks(assistant_msg, &tool_ctx.config);
    if result.blocking_errors.is_empty() {
        return PostModelTurn::Continue;
    }
    let veto = result.prevent_continuation;
    for message in result.blocking_errors {
        if !veto {
            tracing::debug!("PostModelTurn hook injecting error message");
        }
        messages.push(message);
    }
    if veto {
        if let Some(tx) = event_tx {
            let _ = tx.send(crate::QueryEvent::Status(
                "PostModelTurn hook vetoed continuation.".to_string(),
            ));
        }
        return PostModelTurn::Veto;
    }
    PostModelTurn::Continue
}

/// Run the `Stop` hooks, blocking ones first and background ones after.
///
/// Every path that ends a turn calls this, on either dispatch arm.
pub(crate) async fn fire_stop_hooks(
    assistant_msg: &mikmik_core::types::Message,
    tool_ctx: &mikmik_tools::ToolContext,
) {
    let stop_ctx = mikmik_core::hooks::HookContext {
        event: "Stop".to_string(),
        tool_name: None,
        tool_input: None,
        tool_output: Some(assistant_msg.get_all_text()),
        is_error: None,
        session_id: Some(tool_ctx.session_id.clone()),
    };
    mikmik_core::hooks::run_hooks(
        &tool_ctx.config.hooks,
        mikmik_core::config::HookEvent::Stop,
        &stop_ctx,
        &tool_ctx.working_dir,
    )
    .await;

    // Spawns its own blocking tasks and returns an empty Vec at once.
    let _background = stop_hooks_with_full_behavior(
        assistant_msg,
        &tool_ctx.config,
        tool_ctx.working_dir.clone(),
    );
}

/// Everything that runs once a turn has genuinely ended.
///
/// The `Stop` hooks, session-memory extraction, and the AutoDream
/// consolidation check. Both dispatch arms call this; it used to live inline in
/// the Anthropic arm alone, so on any other account no `Stop` hook fired and no
/// memory was ever written.
pub(crate) async fn fire_end_of_turn(
    assistant_msg: &mikmik_core::types::Message,
    tool_ctx: &mikmik_tools::ToolContext,
    config: &crate::QueryConfig,
    messages: &[mikmik_core::types::Message],
    route: &mikmik_core::config::Route,
) {
    fire_stop_hooks(assistant_msg, tool_ctx).await;

    if !config.auto_memory_enabled {
        return;
    }

    if crate::session_memory::SessionMemoryExtractor::should_extract(messages) {
        // Through the account that just served the turn, not a fresh Anthropic
        // client built from `ANTHROPIC_API_KEY`. The provider is resolved
        // inside the task because the extraction is detached and the loop's own
        // handles borrow from the caller's frame.
        let route = route.clone();
        let config = tool_ctx.config.clone();
        let messages = messages.to_vec();
        let working_dir = tool_ctx.working_dir.clone();

        tokio::spawn(async move {
            let Some(provider) = mikmik_api::provider_by_id(&config, &route.account).await else {
                tracing::debug!(
                    account = %route.account,
                    "Session memory extraction skipped: no usable provider"
                );
                return;
            };
            let backend = crate::compact::ProviderBackend(provider);
            let extractor = crate::session_memory::SessionMemoryExtractor::new(route.model);
            match extractor.extract(&messages, &working_dir, &backend).await {
                Ok(memories) if !memories.is_empty() => {
                    let project_root =
                        mikmik_core::session_storage::transcript_root_for(&working_dir);
                    let target = crate::session_memory::session_notes_path(
                        &mikmik_core::memdir::auto_memory_path(&project_root),
                    );
                    if let Err(e) =
                        crate::session_memory::SessionMemoryExtractor::persist(&memories, &target)
                            .await
                    {
                        tracing::warn!(error = %e, "Failed to persist session memories");
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "Session memory extraction failed (non-fatal)");
                }
            }
        });
    }

    // AutoDream consolidation. `maybe_trigger` checks the gates and takes the
    // lock; the subagent runs in a detached task so the spawn does not call
    // `run_query_loop` from inside its own future, which would make that
    // future `!Send`.
    let project_root = mikmik_core::session_storage::transcript_root_for(&tool_ctx.working_dir);
    let memory_dir = mikmik_core::memdir::auto_memory_path(&project_root);
    let conversations_dir = mikmik_core::session_storage::transcript_dir(&project_root);
    let dreamer = crate::auto_dream::AutoDream::new(memory_dir, conversations_dir);
    if let Ok(Some(task)) = dreamer.maybe_trigger().await {
        let agent_input = serde_json::json!({
            "description": "memory consolidation",
            "prompt": task.prompt,
            "max_turns": 20,
            "system_prompt": "You are performing automatic memory consolidation. Complete the task and return a brief summary.",
            "run_in_background": true,
            "isolation": null
        });
        let ctx = tool_ctx.clone();
        tokio::spawn(async move {
            let agent = crate::agent_tool::AgentTool;
            let _result = mikmik_tools::Tool::execute(&agent, agent_input, &ctx).await;
            crate::auto_dream::AutoDream::finish_consolidation(&task).await;
        });
    }
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
    let filenames = mikmik_core::agentsmd::MemoryFilenames::from_config(&tool_ctx.config);
    let rules = mikmik_core::rules::rules_for(
        &project_root,
        filenames,
        tool_ctx.config.effective_rules_builtin(),
        &tool_ctx.config.rules_disabled,
    );
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

/// How many times one turn may be interrupted by a prose rule.
///
/// A rule speaks once per session by default and cannot loop, but `repeat:
/// always` can, and a model that keeps writing the same forbidden word would
/// then never finish a turn.
const MAX_PROSE_INTERRUPTS: u8 = 3;

/// How much new text has to arrive before the rules are checked again.
///
/// A delta is often one token. Rescanning the whole answer on each of them is
/// quadratic, and a rule that matches four characters later than it could is
/// not worth that.
const PROSE_CHECK_STEP: usize = 48;

/// Watches what the model writes, so a rule can stop it mid-answer.
///
/// Built once per turn. A session whose rules all watch tools carries an idle
/// watch that costs one boolean per delta, which is every ordinary session.
pub(crate) struct ProseWatch {
    rules: std::sync::Arc<mikmik_core::rules::RuleSet>,
    session_id: String,
    project_root: std::path::PathBuf,
    text: String,
    thinking: String,
    checked_len: usize,
    /// Names for the transcript, and the rendered blocks for the model.
    fired: Vec<String>,
    rendered: Vec<String>,
    interrupts_left: u8,
}

impl ProseWatch {
    /// Build the watch for one turn.
    pub(crate) fn new(tool_ctx: &mikmik_tools::ToolContext) -> Self {
        let project_root = mikmik_core::session_storage::transcript_root_for(&tool_ctx.working_dir);
        let rules = if tool_ctx.config.effective_rules_enabled() {
            mikmik_core::rules::rules_for(
                &project_root,
                mikmik_core::agentsmd::MemoryFilenames::from_config(&tool_ctx.config),
                tool_ctx.config.effective_rules_builtin(),
                &tool_ctx.config.rules_disabled,
            )
        } else {
            std::sync::Arc::new(mikmik_core::rules::RuleSet::default())
        };
        Self {
            rules,
            session_id: tool_ctx.session_id.clone(),
            project_root,
            text: String::new(),
            thinking: String::new(),
            checked_len: 0,
            fired: Vec::new(),
            rendered: Vec::new(),
            interrupts_left: MAX_PROSE_INTERRUPTS,
        }
    }

    /// Forget the previous turn's text.
    ///
    /// A rule reads one answer, not the transcript. Without this, a turn that
    /// ended cleanly would still be scanned again under the next one.
    pub(crate) fn start_turn(&mut self) {
        self.text.clear();
        self.thinking.clear();
        self.checked_len = 0;
    }

    /// Whether anything is being watched at all.
    pub(crate) fn is_idle(&self) -> bool {
        self.interrupts_left == 0 || !self.rules.watches_prose()
    }

    /// Take one delta. Returns `true` when the turn must stop here.
    pub(crate) fn push(&mut self, delta: &str, stream: mikmik_core::rules::ProseStream) -> bool {
        if self.is_idle() {
            return false;
        }
        let written = match stream {
            mikmik_core::rules::ProseStream::Text => &mut self.text,
            mikmik_core::rules::ProseStream::Thinking => &mut self.thinking,
        };
        written.push_str(delta);
        let total = self.text.len() + self.thinking.len();
        if total < self.checked_len + PROSE_CHECK_STEP {
            return false;
        }
        self.checked_len = total;
        self.check(stream)
    }

    fn check(&mut self, stream: mikmik_core::rules::ProseStream) -> bool {
        let written = match stream {
            mikmik_core::rules::ProseStream::Text => &self.text,
            mikmik_core::rules::ProseStream::Thinking => &self.thinking,
        };
        let turn = 0u64; // Prose rules are claimed per session, not per turn.
        let mut hit = false;
        for rule in self.rules.match_prose(written, stream) {
            if !mikmik_core::rules::claim(&self.session_id, rule, turn) {
                continue;
            }
            tracing::info!(rule = %rule.name, "conditional rule matched what was written");
            self.fired.push(rule.name.clone());
            self.rendered.push(mikmik_core::rules::render_rule(rule));
            hit = true;
        }
        if hit {
            self.interrupts_left = self.interrupts_left.saturating_sub(1);
        }
        hit
    }

    /// What the model is told before it writes the turn again.
    ///
    /// `None` when nothing matched, which is the ordinary case.
    pub(crate) async fn take_message(&mut self) -> Option<mikmik_core::types::Message> {
        if self.rendered.is_empty() {
            return None;
        }
        let names = std::mem::take(&mut self.fired);
        if let Ok(path) =
            mikmik_core::session_storage::transcript_path(&self.project_root, &self.session_id)
        {
            if let Err(e) = mikmik_core::session_storage::append_rules_fired(&path, &names).await {
                tracing::debug!("could not record which rules spoke: {e}");
            }
        }
        let body = std::mem::take(&mut self.rendered).join("\n\n");
        Some(mikmik_core::types::Message::user(body))
    }
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
