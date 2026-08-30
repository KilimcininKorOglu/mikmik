//! The slash commands a connected client may offer, in the protocol's terms.
//!
//! The command layer is the same one the terminal runs, so an editor gets the
//! whole set rather than a second, smaller list that would drift from it.

use std::sync::Arc;

use agent_client_protocol_schema as acp;
use tokio::sync::mpsc;

use crate::runtime::AgentRuntime;
use crate::sessions::SessionState;

/// Every command a client may show, with the hint for what it takes.
///
/// Hidden commands are left out: they exist for compatibility or for tests,
/// and a client offering them would be advertising things nobody should type.
pub fn available_commands() -> Vec<acp::AvailableCommand> {
    mikmik_commands::all_commands()
        .iter()
        .filter(|command| !command.hidden())
        .map(|command| {
            acp::AvailableCommand::new(command.name(), command.description()).input(Some(
                acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                    input_hint(command.help(), command.name()),
                )),
            ))
        })
        .collect()
}

/// What to show in the input box before anything is typed.
///
/// Taken from the command's own usage line, which is where the argument form
/// is written. Without one there is nothing to promise, so the generic word is
/// used rather than a guess at what the command accepts.
fn input_hint(help: &str, name: &str) -> String {
    let Some(usage) = help.lines().next().and_then(|l| l.strip_prefix("Usage:")) else {
        return "arguments".to_string();
    };
    let usage = usage.trim();
    let arguments = usage
        .strip_prefix(&format!("/{name}"))
        .unwrap_or(usage)
        .trim();
    if arguments.is_empty() {
        return "arguments".to_string();
    }
    arguments.to_string()
}

/// The command a prompt names, with whatever was typed after it.
///
/// `None` when the text is not a command: a prompt that opens with a slash but
/// names nothing known is a question for the model, not a typo to reject.
pub fn split_command(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    let rest = text.strip_prefix('/')?;
    let (name, arguments) = match rest.split_once(char::is_whitespace) {
        Some((name, arguments)) => (name, arguments.trim()),
        None => (rest, ""),
    };
    mikmik_commands::find_command(name)?;
    Some((name.to_string(), arguments.to_string()))
}

/// What running a command came to.
#[derive(Default)]
pub struct Outcome {
    /// Text to show as the agent's answer.
    pub reply: Option<String>,
    /// Text to run as a turn, as if the user had typed it.
    pub prompt: Option<String>,
    /// What the client is told about the session afterwards.
    pub updates: Vec<acp::SessionUpdate>,
    /// Whether the command failed, which ends the turn as a refusal.
    pub failed: bool,
}

impl Outcome {
    fn said(text: impl Into<String>) -> Self {
        Self {
            reply: Some(text.into()),
            ..Default::default()
        }
    }

    fn failed(text: impl Into<String>) -> Self {
        Self {
            reply: Some(text.into()),
            failed: true,
            ..Default::default()
        }
    }
}

/// Run one slash command against a session.
///
/// Anything the command reports while it is still running (a browser URL, an
/// authentication that finished in the background) goes to `notes`, because
/// the answer alone would arrive too late to act on.
pub async fn run(
    runtime: &Arc<AgentRuntime>,
    session: &Arc<SessionState>,
    notes: &mpsc::UnboundedSender<String>,
    name: &str,
    arguments: &str,
) -> Outcome {
    let Some(command) = mikmik_commands::find_command(name) else {
        return Outcome::failed(format!("There is no /{name}."));
    };

    let overrides = session.settings.lock().clone();
    let mut config = runtime.config.clone();
    crate::session_config::apply_overrides(&mut config, &overrides);
    let before = config.clone();
    let route = config.resolve_route(config.effective_model());
    let context_window = runtime
        .model_registry
        .context_window_for(&route.account, &route.model);
    let mut ctx = mikmik_commands::CommandContext {
        context_window,
        // An ACP session records no per-turn usage, so there is no measured
        // figure to report. `/context` says so rather than showing the
        // session-wide total, which is not what the window holds.
        context_used_tokens: 0,
        config,
        cost_tracker: session.cost_tracker.clone(),
        messages: session.messages.lock().clone(),
        working_dir: session.cwd.lock().clone(),
        session_id: session.session_id.0.to_string(),
        session_title: session.title.lock().clone(),
        effort_level: overrides.effort.or(runtime.query_config.effort_level),
        remote_session_url: None,
        // A command that lists or authenticates MCP servers must talk about
        // the ones this session is actually running.
        mcp_manager: session
            .mcp
            .lock()
            .as_ref()
            .map(|mcp| mcp.manager.clone())
            .or_else(|| runtime.mcp_manager.clone()),
        mcp_auth_runner: Some(mcp_auth_runner(notes.clone())),
        active_agent: runtime.query_config.agent_definition.clone(),
        // Nobody is at this terminal: the client is an editor somewhere else.
        interactive: false,
    };

    let result = command.execute(arguments, &mut ctx).await;
    apply(runtime, session, notes, &before, result).await
}

/// Carry out what a command decided, and say what the client should know.
async fn apply(
    runtime: &Arc<AgentRuntime>,
    session: &Arc<SessionState>,
    notes: &mpsc::UnboundedSender<String>,
    before: &mikmik_core::config::Config,
    result: mikmik_commands::CommandResult,
) -> Outcome {
    use mikmik_commands::CommandResult as R;

    match result {
        R::Message(text) => Outcome::said(text),
        R::Error(text) => Outcome::failed(text),
        R::Silent => Outcome::default(),
        R::UserMessage(text) => Outcome {
            prompt: Some(text),
            ..Default::default()
        },

        R::ConfigChange(config) => {
            let said = adopt(
                &mut session.settings.lock(),
                &runtime.query_config.model,
                before,
                &config,
            );
            Outcome {
                reply: Some(said),
                updates: config_updates(runtime, session),
                ..Default::default()
            }
        }
        R::ConfigChangeMessage(config, message) => {
            adopt(
                &mut session.settings.lock(),
                &runtime.query_config.model,
                before,
                &config,
            );
            Outcome {
                reply: Some(message),
                updates: config_updates(runtime, session),
                ..Default::default()
            }
        }

        R::ClearConversation | R::NewSession => {
            session.messages.lock().clear();
            Outcome::said("Conversation cleared.")
        }
        R::RunCompaction { instruction } => {
            // A copy, so the session's lock never spans the summarisation call
            // and the client can go on reading the conversation while it runs.
            let current = session.messages.lock().clone();
            let before = current.len();
            let route = mikmik_api::resolve_effective_route(
                &runtime.config,
                runtime.model_registry.as_ref(),
            );

            // The compact model applies here too. "Always that one" has to
            // hold on every surface, or the setting means "usually".
            let run = mikmik_query::compact::compact_on_demand(
                &route,
                &runtime.config,
                Some(runtime.provider_registry.as_ref()),
                runtime.api_client.as_ref(),
                &current,
                instruction.as_deref(),
                session.session_id.0.as_ref(),
            )
            .await;

            match run.result {
                Ok(compacted) => {
                    let removed = before.saturating_sub(compacted.len());
                    *session.messages.lock() = compacted;
                    let mut said = format!(
                        "Compacted {removed} message{} into a summary.",
                        if removed == 1 { "" } else { "s" }
                    );
                    // The chosen compact model could not write it and the
                    // turn's own did, which the client has to be told.
                    if let Some(note) = run.note {
                        said.push('\n');
                        said.push_str(&note);
                    }
                    Outcome::said(said)
                }
                // The conversation was never replaced, so it is intact.
                Err(e) => Outcome::failed(format!("Could not compact: {e}")),
            }
        }

        R::SetMessages(messages) => {
            let removed = session.messages.lock().len().saturating_sub(messages.len());
            *session.messages.lock() = messages;
            Outcome::said(format!(
                "Rewound {removed} message{}.",
                if removed == 1 { "" } else { "s" }
            ))
        }
        R::RetryInterrupted => {
            // ACP keeps no per-turn interrupted flag, so retry the last turn:
            // drop it from the user prompt onward and resubmit that prompt.
            let prompt = {
                let mut msgs = session.messages.lock();
                match msgs
                    .iter()
                    .rposition(|m| m.role == mikmik_core::types::Role::User)
                {
                    Some(idx) => {
                        let text = msgs[idx].get_all_text();
                        msgs.truncate(idx);
                        text
                    }
                    None => String::new(),
                }
            };
            if prompt.trim().is_empty() {
                Outcome::said("Nothing to retry.")
            } else {
                Outcome {
                    prompt: Some(prompt),
                    ..Default::default()
                }
            }
        }
        R::ResumeSession(stored) => {
            let count = stored.messages.len();
            *session.messages.lock() = stored.messages;
            *session.title.lock() = stored.title.clone();
            Outcome {
                reply: Some(format!(
                    "Loaded \"{}\" with {count} message{}.",
                    stored.title.as_deref().unwrap_or(&stored.id),
                    if count == 1 { "" } else { "s" }
                )),
                updates: vec![info_update(session)],
                ..Default::default()
            }
        }
        R::RenameSession(title) => {
            *session.title.lock() = Some(title.clone());
            Outcome {
                reply: Some(format!("Session renamed to \"{title}\".")),
                updates: vec![info_update(session)],
                ..Default::default()
            }
        }
        R::MoveSession {
            destination,
            moved_changes,
        } => {
            *session.cwd.lock() = destination.clone();
            Outcome::said(format!(
                "Session moved to {}{}.",
                destination.display(),
                if moved_changes {
                    ", with the uncommitted changes"
                } else {
                    ""
                }
            ))
        }

        R::McpAuthFlow {
            server_name,
            auth_url,
            redirect_uri,
        } => Outcome::said(format!(
            "Authenticating with '{server_name}'. Open this in a browser:\n{auth_url}\n\
             The answer comes back to {redirect_uri}."
        )),

        R::StartOAuthFlow(login_with_claude_ai) => {
            log_in(
                runtime,
                notes,
                crate::runtime::LoginRequest {
                    provider: mikmik_core::ProviderId::ANTHROPIC.to_string(),
                    login_with_claude_ai,
                    label: None,
                },
            )
            .await
        }
        R::StartLoginForProvider {
            provider,
            login_with_claude_ai,
            label,
        } => {
            log_in(
                runtime,
                notes,
                crate::runtime::LoginRequest {
                    provider,
                    login_with_claude_ai,
                    label,
                },
            )
            .await
        }

        R::ReloadPlugins => {
            let cwd = session.cwd.lock().clone();
            let previous = mikmik_plugins::global_plugin_registry();
            let registry = mikmik_plugins::load_plugins(&cwd, &[]).await;
            let diff = previous
                .as_ref()
                .map(|old| registry.diff_against(old))
                .unwrap_or_default();
            mikmik_plugins::set_global_hooks(registry.build_hook_registry());
            Outcome::said(mikmik_plugins::format_reload_summary(&registry, &diff))
        }

        R::SyncAccountModels { accounts, force } => {
            sync_models(runtime, session, accounts, force).await
        }

        R::RefreshProviderState => {
            match mikmik_api::provider_state::clear_saved_provider_state().await {
                Ok(()) => Outcome::said(
                    "Saved provider state cleared. The agent reads it at startup, so restart it \
                 and run /connect to sign in again.",
                ),
                Err(e) => Outcome::failed(format!("Could not clear the saved provider state: {e}")),
            }
        }

        R::Exit => {
            Outcome::said("A session ends when the client closes it. Nothing was stopped here.")
        }

        // Only the three view-opening commands produce these, and each of them
        // answers in text when nobody is at a terminal, so reaching this means
        // a new command started returning one.
        R::OpenRewindOverlay | R::OpenHooksOverlay | R::OpenImportConfigOverlay => Outcome::failed(
            "That command answers with a view, which this client has no way to show.",
        ),
        // The editor would need a terminal this client does not own, so the
        // path is all that can usefully be handed back.
        R::OpenInEditor { path, .. } => Outcome::said(format!("Edit {} yourself.", path.display())),
    }
}

/// Take from a command's configuration the parts a session may hold, and say
/// which of them changed.
fn adopt(
    overrides: &mut crate::sessions::SessionSettings,
    current_model: &str,
    before: &mikmik_core::config::Config,
    after: &mikmik_core::config::Config,
) -> String {
    let mut changed: Vec<String> = Vec::new();

    if after.model != before.model {
        if let Some(model) = after.model.clone() {
            changed.push(format!("model {model}"));
            overrides.model = Some(model);
        }
    }
    if after.provider != before.provider {
        if let Some(provider) = after.provider.clone() {
            changed.push(format!("account {provider}"));
            overrides.provider = Some(provider);
        }
    }
    if after.permission_mode != before.permission_mode {
        changed.push(format!(
            "mode {}",
            crate::session_config::mode_id_for(&after.permission_mode)
        ));
        overrides.permission_mode = Some(after.permission_mode);
    }
    if after.effort != before.effort {
        if let Some(level) = after
            .effort
            .as_deref()
            .and_then(mikmik_core::effort::EffortLevel::from_str)
        {
            changed.push(format!("effort {}", level.as_str()));
            overrides.effort = Some(level);
        }
    }
    if changed.is_empty() {
        // A command that changed nothing a session can hold still ran; saying
        // "changed" would be false and saying nothing would look like a
        // failure.
        return format!("Nothing changed for this session. The model is still {current_model}.");
    }
    format!("Now using {}.", changed.join(", "))
}

/// The session's options as they stand, for a client to redraw.
fn config_updates(
    runtime: &Arc<AgentRuntime>,
    session: &Arc<SessionState>,
) -> Vec<acp::SessionUpdate> {
    let overrides = session.settings.lock().clone();
    let mut config = runtime.config.clone();
    crate::session_config::apply_overrides(&mut config, &overrides);
    let model = overrides
        .model
        .clone()
        .unwrap_or_else(|| runtime.query_config.model.clone());
    let effort = overrides.effort.or(runtime.query_config.effort_level);
    let options =
        crate::session_config::config_options(&config, &runtime.model_registry, &model, effort);
    let mode = crate::session_config::mode_id_for(&config.permission_mode);

    vec![
        acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(options)),
        acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
            acp::SessionModeId::new(mode),
        )),
    ]
}

/// The session's name and when it last moved.
fn info_update(session: &Arc<SessionState>) -> acp::SessionUpdate {
    acp::SessionUpdate::SessionInfoUpdate(
        acp::SessionInfoUpdate::new()
            .title(session.title.lock().clone())
            .updated_at(chrono::Utc::now().to_rfc3339()),
    )
}

/// Log in to an account, if this runtime was given a way to.
async fn log_in(
    runtime: &Arc<AgentRuntime>,
    notes: &mpsc::UnboundedSender<String>,
    request: crate::runtime::LoginRequest,
) -> Outcome {
    let Some(runner) = runtime.login_runner.clone() else {
        return Outcome::failed(
            "This agent was started without a way to log in. Run `mikmik auth login` in a \
             terminal instead.",
        );
    };
    let provider = request.provider.clone();
    match runner(request, notes.clone()).await {
        Ok(account) => Outcome::said(format!(
            "Signed in to {provider} as {account}. Restart the agent to use the new \
             credential, which is read at startup."
        )),
        Err(e) => Outcome::failed(format!("Signing in to {provider} failed: {e}")),
    }
}

/// Ask each named account what it serves and write the answer down.
async fn sync_models(
    runtime: &Arc<AgentRuntime>,
    session: &Arc<SessionState>,
    accounts: Vec<String>,
    force: bool,
) -> Outcome {
    let overrides = session.settings.lock().clone();
    let mut config = runtime.config.clone();
    crate::session_config::apply_overrides(&mut config, &overrides);

    let targets: Vec<String> = if accounts.is_empty() {
        let mut all: Vec<String> = config.provider_configs.keys().cloned().collect();
        all.sort();
        all
    } else {
        accounts
    };
    if targets.is_empty() {
        return Outcome::said("No accounts configured. Add one with /connect.");
    }
    if let Some(missing) = targets
        .iter()
        .find(|id| !config.provider_configs.contains_key(*id))
    {
        return Outcome::failed(format!(
            "No account named '{missing}'. Configured: {}.",
            config
                .provider_configs
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut lines: Vec<String> = Vec::new();
    for account in targets {
        let provider = runtime
            .provider_registry
            .get(&mikmik_core::ProviderId::new(&account))
            .cloned();
        let Some(provider) = provider else {
            lines.push(format!("{account}: no provider is configured for it."));
            continue;
        };
        match provider.discover_models().await {
            Ok(models) if !models.is_empty() => {
                match mikmik_api::model_sync::persist_account_models(&account, &models, force) {
                    Ok(outcome) => {
                        lines.push(mikmik_api::model_sync::describe_model_sync(
                            &account, &outcome,
                        ));
                    }
                    Err(e) => lines.push(format!("{account}: could not save models: {e}")),
                }
            }
            Ok(_) => lines.push(format!("{account}: reported no models.")),
            Err(e) => lines.push(format!("{account}: could not be asked: {e}")),
        }
    }
    Outcome::said(lines.join("\n"))
}

/// Run an MCP authentication in the background and say how it went.
fn mcp_auth_runner(
    notes: mpsc::UnboundedSender<String>,
) -> Arc<dyn Fn(mikmik_mcp::oauth::McpAuthSession) + Send + Sync> {
    Arc::new(move |session| {
        let notes = notes.clone();
        tokio::spawn(async move {
            let text = match mikmik_mcp::oauth::run_mcp_auth_session(session).await {
                Ok(result) => format!("Authenticated with '{}'.", result.server_name),
                Err(e) => format!("MCP authentication failed: {e}"),
            };
            let _ = notes.send(text);
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offered_commands_are_the_ones_the_terminal_runs() {
        let offered = available_commands();
        let expected = mikmik_commands::all_commands()
            .iter()
            .filter(|c| !c.hidden())
            .count();

        assert_eq!(offered.len(), expected);
        assert!(offered.iter().any(|c| c.name == "help"));
    }

    #[test]
    fn a_hidden_command_is_not_offered() {
        // Offering one would put a command in the client's list that nobody
        // is meant to type.
        let hidden: Vec<String> = mikmik_commands::all_commands()
            .iter()
            .filter(|c| c.hidden())
            .map(|c| c.name().to_string())
            .collect();

        let offered = available_commands();
        for name in &hidden {
            assert!(
                !offered.iter().any(|c| &c.name == name),
                "hidden command {name} was offered"
            );
        }
    }

    #[test]
    fn every_offered_command_says_what_it_does() {
        for command in available_commands() {
            assert!(
                !command.description.is_empty(),
                "{} has no description",
                command.name
            );
        }
    }

    #[test]
    fn a_prompt_naming_a_command_is_split_from_its_arguments() {
        assert_eq!(
            split_command("/help commands"),
            Some(("help".to_string(), "commands".to_string()))
        );
        assert_eq!(
            split_command("/help"),
            Some(("help".to_string(), String::new()))
        );
    }

    #[test]
    fn a_prompt_that_only_looks_like_a_command_goes_to_the_model() {
        // "/usr/bin is missing" is a question, not a command nobody defined.
        assert_eq!(split_command("/usr/bin is missing"), None);
        assert_eq!(split_command("what does /help do"), None);
        assert_eq!(split_command("help"), None);
    }

    #[test]
    fn a_command_that_changed_the_model_changes_it_for_this_session_only() {
        let before = mikmik_core::config::Config {
            model: Some("claude-opus-5".to_string()),
            ..Default::default()
        };
        let after = mikmik_core::config::Config {
            model: Some("gpt-5".to_string()),
            ..before.clone()
        };
        let mut overrides = crate::sessions::SessionSettings::default();

        let said = adopt(&mut overrides, "claude-opus-5", &before, &after);

        assert_eq!(overrides.model.as_deref(), Some("gpt-5"));
        assert!(said.contains("gpt-5"), "{said}");
    }

    #[test]
    fn a_command_that_changed_the_mode_changes_it_for_this_session_only() {
        let before = mikmik_core::config::Config::default();
        let after = mikmik_core::config::Config {
            permission_mode: mikmik_core::PermissionMode::AcceptEdits,
            ..Default::default()
        };
        let mut overrides = crate::sessions::SessionSettings::default();

        let said = adopt(&mut overrides, "m", &before, &after);

        assert_eq!(
            overrides.permission_mode,
            Some(mikmik_core::PermissionMode::AcceptEdits)
        );
        assert!(said.contains("acceptEdits"), "{said}");
    }

    #[test]
    fn a_command_that_changed_nothing_a_session_holds_says_so() {
        // Reporting a change would be false, and saying nothing at all reads
        // as a command that failed.
        let config = mikmik_core::config::Config::default();
        let mut overrides = crate::sessions::SessionSettings::default();

        let said = adopt(&mut overrides, "claude-opus-5", &config, &config);

        assert!(said.contains("claude-opus-5"), "{said}");
        assert_eq!(overrides.model, None);
        assert_eq!(overrides.permission_mode, None);
    }

    #[test]
    fn renaming_a_session_tells_the_client_its_new_name() {
        let session = SessionState::new(
            acp::SessionId::new("acp-1"),
            std::path::PathBuf::from("/tmp"),
        );
        *session.title.lock() = Some("the parser".to_string());

        let acp::SessionUpdate::SessionInfoUpdate(update) = info_update(&session) else {
            panic!("expected a session info update");
        };
        assert_eq!(
            update.title,
            acp::MaybeUndefined::Value("the parser".to_string())
        );
    }

    #[test]
    fn the_hint_comes_from_the_commands_own_usage_line() {
        assert_eq!(input_hint("Usage: /rewind [n]\nmore text", "rewind"), "[n]");
        assert_eq!(input_hint("Usage: /clear", "clear"), "arguments");
        assert_eq!(input_hint("Clears the conversation.", "clear"), "arguments");
    }
}
