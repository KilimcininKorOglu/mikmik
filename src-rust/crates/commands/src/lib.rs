// mikmik-commands: Slash command system for MikMik.
//
// This crate implements the /command framework that allows users to type
// commands like /help, /compact, /clear, /model, /config, /cost, etc.
// Each command is a struct implementing the `SlashCommand` trait.

use async_trait::async_trait;
use mikmik_core::config::{Config, HookEntry, HookEvent, Settings, Theme};
use mikmik_core::cost::CostTracker;
use mikmik_core::types::{ContentBlock, Message};
use std::collections::BTreeMap;
#[allow(unused_imports)]
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Core trait
// ---------------------------------------------------------------------------

/// Context available to every slash command.
pub struct CommandContext {
    pub config: Config,
    pub cost_tracker: Arc<CostTracker>,
    pub messages: Vec<Message>,
    pub working_dir: std::path::PathBuf,
    pub session_id: String,
    pub session_title: Option<String>,
    /// The effort level in force, and whether anyone chose it.
    ///
    /// `None` means nothing was chosen, which is not the same as choosing the
    /// default: an unset effort sends no reasoning configuration at all.
    pub effort_level: Option<mikmik_core::effort::EffortLevel>,
    /// Remote session URL set when a bridge connection is active.
    pub remote_session_url: Option<String>,
    // Note: config already contains hooks, mcp_servers, etc.
    /// Live MCP manager — present when servers are connected.
    pub mcp_manager: Option<Arc<mikmik_mcp::McpManager>>,
    /// Optional callback for starting an MCP OAuth flow in the background.
    pub mcp_auth_runner: Option<Arc<dyn Fn(mikmik_mcp::oauth::McpAuthSession) + Send + Sync>>,
    /// Whether whoever ran the command can see a view on this terminal.
    ///
    /// False for an editor over ACP, a remote client, and the headless path.
    /// A command that would open a picker answers those callers in text
    /// instead, because a view they cannot see helps nobody and takes the
    /// place of the answer they could have read.
    pub interactive: bool,
    /// The agent definition in force, when the session is running under one.
    ///
    /// An agent's own settings win over the session's for the fields it
    /// declares, so a command that changes one of those fields has to be able
    /// to say that its change will not take effect yet.
    pub active_agent: Option<mikmik_core::AgentDefinition>,
    /// The active model's context window, in tokens.
    ///
    /// Resolved through `ModelRegistry::context_window_for`, because the window
    /// is per model and a command that assumes one reports the wrong share of
    /// it for every model that carries another.
    pub context_window: u64,
    /// Tokens the API counted for the last request, and 0 before the first one.
    ///
    /// This is the last turn's `usage.total_input()`, the same figure the
    /// footer draws. It is not `cost_tracker.total_tokens()`, which accumulates
    /// over the session and passes the window without the context ever being
    /// full.
    pub context_used_tokens: u64,
}

/// Result of running a slash command.
#[derive(Debug)]
pub enum CommandResult {
    /// Display a message to the user (does NOT go to the model).
    Message(String),
    /// Inject a message into the conversation as though the user typed it.
    UserMessage(String),
    /// Modify the configuration.
    ConfigChange(Config),
    /// Modify the configuration and show a specific status message.
    ConfigChangeMessage(Config, String),
    /// Trigger a background MCP OAuth flow and request runtime reconnect on success.
    McpAuthFlow {
        /// The configured MCP server name.
        server_name: String,
        /// The browser URL shown to the user while the background flow runs.
        auth_url: String,
        /// The local callback URL waiting for the OAuth redirect.
        redirect_uri: String,
    },
    /// Reload the plugins from disk and re-apply what they contribute.
    /// Carried out by the session loop, which owns every surface a plugin
    /// touches.
    ReloadPlugins,
    /// Clear the conversation.
    ClearConversation,
    /// Replace the conversation with a specific message list (used by /rewind).
    SetMessages(Vec<Message>),
    /// Load a previously saved session into the live REPL.
    ResumeSession(mikmik_core::history::ConversationSession),
    /// Update the current session title.
    RenameSession(String),
    /// Trigger the OAuth login flow (handled by the REPL in main.rs).
    /// The bool indicates whether to use Claude.ai auth (true) or Console auth (false).
    StartOAuthFlow(bool),
    /// Trigger the OAuth login flow for a specific provider with optional
    /// human-friendly label for the new account profile.
    ///
    /// `provider` is one of `mikmik_core::ProviderId::ANTHROPIC` or
    /// `PROVIDER_CODEX`. `login_with_claude_ai` is only meaningful for
    /// Anthropic.
    StartLoginForProvider {
        provider: String,
        login_with_claude_ai: bool,
        label: Option<String>,
    },
    /// Exit the REPL.
    Exit,
    /// No visible output.
    Silent,
    /// An error.
    Error(String),
    /// Open the rewind/message-selector overlay in the TUI.
    /// The TUI will call SetMessages when the user confirms.
    OpenRewindOverlay,
    /// Summarise the conversation now, replacing its head with the summary.
    ///
    /// Carried out by the session loop, which owns the API client and the
    /// transcript. `SetMessages` would not do: it is `/rewind`'s outcome and
    /// reports itself as one, and the compaction has to happen before the
    /// message list is known.
    RunCompaction {
        /// What the user asked the summary to preserve, when they said.
        instruction: Option<String>,
    },
    /// Open the hooks configuration browser overlay in the TUI.
    /// Falls back to a text listing in non-TUI contexts.
    OpenHooksOverlay,
    /// Open the import-config overlay in the TUI.
    OpenImportConfigOverlay,
    /// Clear saved provider auth, model selection, and model caches, then
    /// rebuild the live runtime state.
    RefreshProviderState,
    /// Start a fresh session (opencode's `/new`): reset to a blank home,
    /// preserving the current model/provider/effort selection and working
    /// directory. Lazy — the new session is only persisted on the first message.
    NewSession,
    /// Re-home the current session to another worktree/directory of the same
    /// project (opencode's `/move`). The git working-tree changes have already
    /// been relocated by the command; the CLI just repoints the live session.
    MoveSession {
        /// Absolute destination directory the session now lives in.
        destination: std::path::PathBuf,
        /// Whether uncommitted changes were carried across (for the status line).
        moved_changes: bool,
    },
    /// Open a file in the user's editor.
    ///
    /// Carried out by the session loop, which is the only place that can hand
    /// the terminal over: an editor started while the TUI holds raw mode and
    /// the alternate screen draws over the frame and gets redrawn on the next
    /// one.
    OpenInEditor {
        /// The file to open. Created empty first if it does not exist.
        path: std::path::PathBuf,
        /// What to say once the editor has exited.
        message: String,
    },
    /// Ask the named accounts what models they serve and rewrite their lists.
    ///
    /// Discovery is async and needs the live provider registry, so the command
    /// only names the accounts and the event loop performs the calls.
    SyncAccountModels {
        /// Accounts to ask. Empty means every configured account.
        accounts: Vec<String>,
        /// Whether the endpoint's limits may replace ones the user wrote by
        /// hand into `modelOverrides`.
        force: bool,
    },
}

/// Every slash command implements this trait.
#[async_trait]
pub trait SlashCommand: Send + Sync {
    /// The primary name (without the leading `/`).
    fn name(&self) -> &str;
    /// Alias names (e.g. `["h"]` for `/help`).
    fn aliases(&self) -> Vec<&str> {
        vec![]
    }
    /// One-line description for /help.
    fn description(&self) -> &str;
    /// Detailed help text (shown by `/help <command>`).
    fn help(&self) -> &str {
        self.description()
    }
    /// Whether this command is visible in /help output.
    fn hidden(&self) -> bool {
        false
    }
    /// Execute the command with the given arguments string.
    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult;
}

/// The cheapest model the active account serves, and that account.
fn resolve_fast_model_route(config: &Config) -> mikmik_core::config::Route {
    mikmik_api::resolve_small_model_route(config, &mikmik_api::ModelRegistry::new())
}

use mikmik_core::message_utils::text_from_blocks as text_from_content_blocks;

// ---------------------------------------------------------------------------
// Feature command modules (extracted per issue #232 to shrink this file).
// Each module owns a cohesive group of SlashCommand impls plus its private
// helpers. Command structs are re-exported so the public surface is unchanged.
// ---------------------------------------------------------------------------
mod goal;
pub use goal::*;
mod todos;
pub use todos::*;
mod poke;
pub use poke::*;
mod turns;
pub use turns::*;
mod yolo;
pub use yolo::*;
mod config_cmd;
pub use config_cmd::*;
mod plugin;
pub use plugin::*;
mod doctor;
pub use doctor::*;
mod accounts;
pub use accounts::*;
mod review;
pub use review::*;
mod mcp;
pub use mcp::*;
mod export;
pub use export::*;
mod share;
pub use share::*;
mod copy;
pub use copy::*;
mod chrome;
pub use chrome::*;
mod teleport;
pub use teleport::*;
mod managed_agents;
pub use managed_agents::*;
mod appearance;
pub use appearance::*;
mod memories;
pub use memories::*;
mod memory;
pub use memory::*;
mod permissions;
pub use permissions::*;
mod session;
pub use session::*;
mod remote;
pub use remote::*;
mod workspace;
pub use workspace::*;
mod history;
pub use history::*;
mod sandbox;
pub use sandbox::*;
mod ultrareview;
pub use ultrareview::*;
mod thinkback;
pub use thinkback::*;
mod search;
pub use search::*;
mod session_tools;
pub use session_tools::*;
mod display;
pub use display::*;
mod maintenance;
pub use maintenance::*;
mod setup;
pub use setup::*;
mod diagnostics;
pub use diagnostics::*;
mod providers;
pub use providers::*;
mod usage;
pub use usage::*;
mod extras;
pub use extras::*;
mod ui_settings;
use ui_settings::*;
mod new_move;
pub use new_move::*;
mod buddy;
pub use buddy::*;

// ---------------------------------------------------------------------------
// Built-in commands
// ---------------------------------------------------------------------------

pub struct HelpCommand;
pub struct ClearCommand;
pub struct CompactCommand;
pub struct CostCommand;
pub struct ExitCommand;
pub struct ModelCommand;
pub struct VersionCommand;
pub struct ResumeCommand;
pub struct StatusCommand;
pub struct DiffCommand;
pub struct InitCommand;
pub struct HooksCommand;
pub struct ImportConfigCommand;
pub struct ThinkingCommand;
// New commands
// Batch-1 new commands
// New commands: teleport, btw, sandbox-toggle
pub struct NamedCommandAdapter {
    pub slash_name: &'static str,
    pub target_name: &'static str,
    pub slash_aliases: &'static [&'static str],
    pub slash_description: &'static str,
    pub slash_help: &'static str,
}

#[derive(serde::Serialize)]
struct KeybindingTemplateFile {
    #[serde(rename = "$schema")]
    schema: &'static str,
    #[serde(rename = "$docs")]
    docs: &'static str,
    bindings: Vec<KeybindingTemplateBlock>,
}

#[derive(serde::Serialize)]
struct KeybindingTemplateBlock {
    context: String,
    bindings: BTreeMap<String, Option<String>>,
}

fn save_settings_mutation<F>(mutate: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut Settings),
{
    let mut settings = Settings::load_sync()?;
    mutate(&mut settings);
    settings.save_sync()
}

fn open_with_system(target: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let ps_cmd = format!("Start-Process '{}'", target.replace('\'', "''"));
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps_cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }
}

fn format_keystroke(keystroke: &mikmik_core::keybindings::ParsedKeystroke) -> String {
    let mut parts = Vec::new();
    if keystroke.ctrl {
        parts.push("ctrl".to_string());
    }
    if keystroke.alt {
        parts.push("alt".to_string());
    }
    if keystroke.shift {
        parts.push("shift".to_string());
    }
    if keystroke.meta {
        parts.push("meta".to_string());
    }
    parts.push(match keystroke.key.as_str() {
        "space" => "space".to_string(),
        other => other.to_string(),
    });
    parts.join("+")
}

fn format_chord(chord: &[mikmik_core::keybindings::ParsedKeystroke]) -> String {
    chord
        .iter()
        .map(format_keystroke)
        .collect::<Vec<_>>()
        .join(" ")
}

fn generate_keybindings_template() -> anyhow::Result<String> {
    let mut grouped: BTreeMap<String, BTreeMap<String, Option<String>>> = BTreeMap::new();
    for binding in mikmik_core::keybindings::default_bindings() {
        let chord = format_chord(&binding.chord);
        if mikmik_core::keybindings::NON_REBINDABLE.contains(&chord.as_str()) {
            continue;
        }
        grouped
            .entry(format!("{:?}", binding.context))
            .or_default()
            .insert(chord, binding.action.clone());
    }

    let template = KeybindingTemplateFile {
        schema: "https://www.schemastore.org/claude-code-keybindings.json",
        docs: "https://code.claude.com/docs/en/keybindings",
        bindings: grouped
            .into_iter()
            .map(|(context, bindings)| KeybindingTemplateBlock { context, bindings })
            .collect(),
    };

    Ok(format!("{}\n", serde_json::to_string_pretty(&template)?))
}

fn parse_theme(name: &str) -> Option<Theme> {
    match name.trim().to_lowercase().as_str() {
        "default" | "system" => Some(Theme::Default),
        "dark" => Some(Theme::Dark),
        "light" => Some(Theme::Light),
        custom if !custom.is_empty() => Some(Theme::Custom(custom.to_string())),
        _ => None,
    }
}

fn current_output_style_name(config: &Config) -> &str {
    config.output_style.as_deref().unwrap_or("default")
}

fn available_output_style_names() -> Vec<String> {
    mikmik_core::output_styles::all_styles_with_runtime(&Settings::config_dir())
        .into_iter()
        .map(|style| style.name)
        .collect()
}

fn split_command_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escape = false;

    for ch in args.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }

        match ch {
            '\\' => escape = true,
            '\'' | '"' if quote == Some(ch) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            ch if ch.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        out.push(current);
    }

    out
}

fn execute_named_command_from_slash(
    target_name: &str,
    args: &str,
    ctx: &CommandContext,
) -> CommandResult {
    let Some(cmd) = named_commands::find_named_command(target_name) else {
        return CommandResult::Error(format!(
            "Named command '{}' is not available in this build.",
            target_name
        ));
    };

    let parsed_args = split_command_args(args);
    let parsed_refs = parsed_args.iter().map(String::as_str).collect::<Vec<_>>();
    cmd.execute_named(&parsed_refs, ctx)
}

// ---- /help ---------------------------------------------------------------

/// Category labels for help grouping.
fn command_category(name: &str) -> &'static str {
    match name {
        "clear" | "new" | "compact" | "rewind" | "summary" | "export" | "rename" | "branch"
        | "fork" => "Conversation",
        "model" | "config" | "theme" | "color" | "vim" | "fast" | "effort" | "voice"
        | "statusline" | "output-style" | "keybindings" | "privacy-settings"
        | "rate-limit-options" | "sandbox-toggle" | "timeline" => "Settings",
        "cost" | "stats" | "usage" | "extra-usage" | "context" => "Usage & Cost",
        "status" | "doctor" | "terminal-setup" | "version" | "update" | "upgrade"
        | "release-notes" => "System",
        "login" | "logout" | "refresh" | "permissions" => "Auth & Permissions",
        "memory" | "files" | "diff" | "init" | "commit" | "review" | "security-review"
        | "import-config" => "Project",
        "mcp" | "hooks" | "ide" | "chrome" => "Integrations",
        "session" | "resume" | "remote-control" | "remote-env" | "teleport" | "move" => {
            "Sessions & Remote"
        }
        "help" | "exit" => "General",
        "think-back" | "thinkback-play" | "thinking" | "plan" | "tasks" => "AI & Thinking",
        "copy" | "skills" | "agents" | "plugin" | "reload-plugins" | "stickers" | "passes"
        | "desktop" | "mobile" | "btw" => "Tools & Extras",
        _ => "Other",
    }
}

#[async_trait]
impl SlashCommand for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["h", "?"]
    }
    fn description(&self) -> &str {
        "Show available commands and usage information"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        if !args.is_empty() {
            // Show help for a specific command
            if let Some(cmd) = find_command(args) {
                let aliases = cmd.aliases();
                let alias_line = if aliases.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nAliases: {}",
                        aliases
                            .iter()
                            .map(|a| format!("/{}", a))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                return CommandResult::Message(format!(
                    "/{name}{aliases}\n{desc}\n\n{help}",
                    name = cmd.name(),
                    aliases = alias_line,
                    desc = cmd.description(),
                    help = cmd.help(),
                ));
            }
            return CommandResult::Error(format!("Unknown command: /{}", args));
        }

        // Grouped output
        let commands = all_commands();
        let visible: Vec<_> = commands.iter().filter(|c| !c.hidden()).collect();

        // Collect categories in stable order
        let category_order = [
            "Conversation",
            "Settings",
            "Usage & Cost",
            "System",
            "Auth & Permissions",
            "Project",
            "Integrations",
            "Sessions & Remote",
            "AI & Thinking",
            "Tools & Extras",
            "General",
            "Other",
        ];

        let mut by_cat: std::collections::HashMap<&str, Vec<String>> =
            std::collections::HashMap::new();

        for cmd in &visible {
            let cat = command_category(cmd.name());
            let aliases = cmd.aliases();
            let alias_str = if aliases.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    aliases
                        .iter()
                        .map(|a| format!("/{}", a))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            by_cat.entry(cat).or_default().push(format!(
                "  /{:<20} {}",
                format!("{}{}", cmd.name(), alias_str),
                cmd.description()
            ));
        }

        let mut output = String::from("MikMik — Slash Commands\n");
        output.push_str("════════════════════════════\n");

        for cat in &category_order {
            if let Some(entries) = by_cat.get(cat) {
                output.push_str(&format!("\n{}\n", cat));
                for entry in entries {
                    output.push_str(&format!("{}\n", entry));
                }
            }
        }

        output.push_str("\nType /help <command> for detailed help on a specific command.");
        CommandResult::Message(output)
    }
}

// ---- /clear --------------------------------------------------------------

#[async_trait]
impl SlashCommand for ClearCommand {
    fn name(&self) -> &str {
        "clear"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["c", "reset"]
    }
    fn description(&self) -> &str {
        "Clear the conversation history"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::ClearConversation
    }
}

// ---- /compact ------------------------------------------------------------

#[async_trait]
impl SlashCommand for CompactCommand {
    fn name(&self) -> &str {
        "compact"
    }
    fn description(&self) -> &str {
        "Compact the conversation to reduce token usage"
    }

    fn help(&self) -> &str {
        "Usage: /compact [instruction]\n\n\
         Replaces the older part of the conversation with a summary, keeping \
         the most recent turns verbatim. The cut never splits a tool call from \
         its result.\n\n\
         An instruction is passed to the summariser, e.g. \
         `/compact keep every file path and command`."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let trimmed = args.trim();
        CommandResult::RunCompaction {
            instruction: (!trimmed.is_empty()).then(|| trimmed.to_string()),
        }
    }
}

// ---- /cost ---------------------------------------------------------------

/// The rate card for one model, named so the reader knows whose rates these
/// are: the model that spent the tokens need not be the session model.
fn rates_line(model: &str, pricing: mikmik_core::cost::ModelPricing) -> String {
    format!(
        "  Rates ($/MTok) for {}: input ${:.2} | output ${:.2} | cache-write ${:.3} | cache-read ${:.3}",
        model,
        pricing.input_per_mtk,
        pricing.output_per_mtk,
        pricing.cache_creation_per_mtk,
        pricing.cache_read_per_mtk,
    )
}

#[async_trait]
impl SlashCommand for CostCommand {
    fn name(&self) -> &str {
        "cost"
    }
    fn description(&self) -> &str {
        "Show token usage and cost for this session"
    }
    fn help(&self) -> &str {
        "Usage: /cost\n\n\
         Shows per-category token counts and the estimated cost for this session.\n\
         Cache write tokens are priced slightly higher than input; cache read tokens\n\
         are ~10x cheaper — caching reduces cost significantly in long sessions.\n\
         For per-call breakdown use /extra-usage. For account quotas use /usage."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let tracker = &ctx.cost_tracker;
        let model = ctx.config.effective_model();

        let input = tracker.input_tokens();
        let output = tracker.output_tokens();
        let cache_create = tracker.cache_creation_tokens();
        let cache_read = tracker.cache_read_tokens();
        let total = tracker.total_tokens();
        let cost = tracker.total_cost_usd();

        // Every dollar figure below comes from the same per-model source, so
        // the rows add up to the total even when several models ran.
        let split = tracker.cost_by_category();
        let spenders = tracker.by_model();

        // One set of rates only explains the rows when one model spent them.
        //
        // The rates come off the spend row, which carries what the tokens were
        // actually billed at. Re-deriving them from `ModelPricing::for_model`
        // printed a name-based guess above a registry-priced total, so a
        // Gemini row advertised Anthropic's rates.
        let pricing_line = match spenders.as_slice() {
            [only] => rates_line(&only.model, only.pricing),
            // Nothing has been spent yet, so there is no billed rate to show.
            // The heuristic is the only answer available, and it is named as
            // an estimate rather than presented as this model's rate card.
            [] => format!(
                "{}  (estimated; nothing billed yet)",
                rates_line(model, mikmik_core::cost::ModelPricing::for_model(model))
            ),
            _ => "  Rates ($/MTok): vary by model — see the breakdown below".to_string(),
        };

        // Cache savings note: how much input cost was avoided by using cache-read
        // instead of re-sending those tokens as normal input.
        let savings = if cache_read > 0 {
            format!(
                "\n  Cache savings:  ${:.4}  ({} tokens served from cache)",
                split.cache_savings, cache_read
            )
        } else {
            String::new()
        };

        let by_model = stats::by_model_block(tracker);

        CommandResult::Message(format!(
            "Session Cost — {model}\n\
             ──────────────────────────────\n\
             {pricing_line}\n\n\
               Input tokens:   {input:>10}   ${input_cost:.4}\n\
               Output tokens:  {output:>10}   ${output_cost:.4}\n\
               Cache write:    {cache_create:>10}   ${cc_cost:.4}\n\
               Cache read:     {cache_read:>10}   ${cr_cost:.4}\n\
             ─────────────────────────────\n\
               Total tokens:   {total:>10}\n\
               Total cost:              ${cost:.4}{savings}\n\
             {by_model}\n\
             Use /usage for quota info · /extra-usage for per-call breakdown",
            model = model,
            pricing_line = pricing_line,
            by_model = by_model,
            input = input,
            input_cost = split.input,
            output = output,
            output_cost = split.output,
            cache_create = cache_create,
            cc_cost = split.cache_creation,
            cache_read = cache_read,
            cr_cost = split.cache_read,
            total = total,
            cost = cost,
            savings = savings,
        ))
    }
}

// ---- /exit ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for ExitCommand {
    fn name(&self) -> &str {
        "exit"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["quit", "q"]
    }
    fn description(&self) -> &str {
        "Exit MikMik"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::Exit
    }
}

// ---- /model --------------------------------------------------------------

#[async_trait]
impl SlashCommand for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }
    fn description(&self) -> &str {
        "Show or change the current model"
    }
    fn help(&self) -> &str {
        "Usage: /model [<model-id>]\n\n\
         Without arguments, shows the current model.\n\n\
         With a model ID, switches to that model.  Accepts both bare model\n\
         names (e.g. claude-sonnet-4-6) and provider-prefixed format\n\
         (e.g. openai/gpt-4o, google/gemini-2.0-flash).\n\n\
         Examples:\n\
           /model                        — show current model\n\
           /model claude-opus-4-6        — switch to Claude Opus 4.6\n\
           /model openai/gpt-4o          — switch to GPT-4o via OpenAI\n\
           /model google/gemini-2.0-flash — switch to Gemini 2.0 Flash"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();
        if args.is_empty() {
            CommandResult::Message(format!("Current model: {}", ctx.config.effective_model()))
        } else {
            // Accept both "account/model" and bare model names. The split is
            // `resolve_route`'s to make: a bare `split_once('/')` here read
            // `meta-llama/Llama-3.3-70B`, one OpenRouter model id, as an
            // account called `meta-llama` and pointed the session at an
            // endpoint that does not exist.
            let route = ctx.config.resolve_route(args);
            let mut new_config = ctx.config.clone();
            new_config.model = Some(ctx.config.canonical_model(&route.account, &route.model));
            new_config.provider = Some(route.account.clone());
            // Naming both halves lets a mistyped account be spotted at once.
            // An unrecognised prefix is read as part of the model id rather
            // than rejected, so it does not look wrong on its own.
            let confirmation = format!(
                "Switched to '{}' on account '{}'.",
                route.model, route.account
            );
            CommandResult::ConfigChangeMessage(new_config, confirmation)
        }
    }
}

// ---- /version ------------------------------------------------------------

#[async_trait]
impl SlashCommand for VersionCommand {
    fn name(&self) -> &str {
        "version"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["v"]
    }
    fn description(&self) -> &str {
        "Show version information"
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::Message(format!("MikMik v{}", mikmik_core::constants::APP_VERSION))
    }
}

// ---- /resume -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ResumeCommand {
    fn name(&self) -> &str {
        "resume"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["r", "continue"]
    }
    fn description(&self) -> &str {
        "Resume a previous conversation"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        if args.is_empty() {
            let sessions = mikmik_core::history::list_sessions().await.sessions;
            let Some(last) = sessions.first() else {
                return CommandResult::Message("No previous sessions found.".to_string());
            };
            match mikmik_core::history::load_session(&last.id).await {
                Ok(session) => CommandResult::ResumeSession(session),
                Err(e) => {
                    CommandResult::Error(format!("Failed to load session {}: {}", last.id, e))
                }
            }
        } else {
            match mikmik_core::history::load_session(args.trim()).await {
                Ok(session) => CommandResult::ResumeSession(session),
                Err(e) => {
                    CommandResult::Error(format!("Failed to load session {}: {}", args.trim(), e))
                }
            }
        }
    }
}

// ---- /status -------------------------------------------------------------

#[async_trait]
impl SlashCommand for StatusCommand {
    fn name(&self) -> &str {
        "status"
    }
    fn description(&self) -> &str {
        "Show comprehensive system and session status"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        // Auth status
        let auth_status = match mikmik_core::oauth::OAuthTokens::load().await {
            Some(tokens) => {
                let sub = tokens.subscription_type.as_deref().unwrap_or("oauth");
                format!("Authenticated ({})", sub)
            }
            None => {
                if ctx.config.resolve_api_key().is_some() {
                    "Authenticated (API key)".to_string()
                } else {
                    "Not authenticated".to_string()
                }
            }
        };

        // MCP status
        let mcp_count = ctx.config.mcp_servers.len();
        let mcp_status = if mcp_count == 0 {
            "none configured".to_string()
        } else {
            format!("{} server(s) configured", mcp_count)
        };

        // Hook status
        let hook_count: usize = ctx.config.hooks.values().map(|v| v.len()).sum();

        // UI settings
        let ui = load_ui_settings();
        let editor_mode = ui.editor_mode.as_deref().unwrap_or("normal");
        let fast_mode = ui.fast_mode.unwrap_or(false);

        // Git status
        let git_branch = tokio::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&ctx.working_dir)
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "n/a".to_string());

        CommandResult::Message(format!(
            "MikMik Status\n\
             ══════════════════\n\
             Auth:           {auth_status}\n\
             Model:          {model}\n\
             Permission mode: {perm:?}\n\
             Fast mode:      {fast}\n\
             Editor mode:    {editor}\n\n\
             Session\n\
             ───────\n\
             Session ID:     {sid}\n\
             Title:          {title}\n\
             Messages:       {msgs}\n\
             Working dir:    {wd}\n\
             Git branch:     {branch}\n\n\
             Integrations\n\
             ────────────\n\
             MCP servers:    {mcp}\n\
             Hooks:          {hooks} configured\n\n\
             Usage\n\
             ─────\n\
             {summary}",
            auth_status = auth_status,
            model = ctx.config.effective_model(),
            perm = ctx.config.permission_mode,
            fast = if fast_mode { "on" } else { "off" },
            editor = editor_mode,
            sid = &ctx.session_id[..ctx.session_id.len().min(12)],
            title = ctx.session_title.as_deref().unwrap_or("(untitled)"),
            msgs = ctx.messages.len(),
            wd = ctx.working_dir.display(),
            branch = git_branch,
            mcp = mcp_status,
            hooks = hook_count,
            summary = ctx.cost_tracker.summary(),
        ))
    }
}

// ---- /diff ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for DiffCommand {
    fn name(&self) -> &str {
        "diff"
    }
    fn description(&self) -> &str {
        "Show git diff of changes in the working directory"
    }
    fn help(&self) -> &str {
        "Usage: /diff [--stat|--staged|<ref>]\n\n\
         Shows git diff output for the current working directory.\n\n\
         Options:\n\
           /diff           — diff of all unstaged changes (git diff)\n\
           /diff --stat    — summary of changed files\n\
           /diff --staged  — diff of staged changes (git diff --cached)\n\
           /diff <ref>     — diff against a branch, tag, or commit"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        let git_args: Vec<&str> = if args == "--stat" {
            vec!["diff", "--stat"]
        } else if args == "--staged" || args == "--cached" {
            vec!["diff", "--cached"]
        } else if args.is_empty() {
            vec!["diff"]
        } else {
            // Treat as a ref
            vec!["diff", args]
        };

        let output = tokio::process::Command::new("git")
            .args(&git_args)
            .current_dir(&ctx.working_dir)
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() || out.status.code() == Some(1) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if stdout.trim().is_empty() {
                    CommandResult::Message(
                        "No changes found. Working tree is clean (or not a git repository)."
                            .to_string(),
                    )
                } else {
                    // Truncate very long diffs
                    let text = stdout.as_ref();
                    let display = if text.len() > 8000 {
                        format!(
                            "{}\n… (truncated — {} total bytes; use `git diff` for full output)",
                            &text[..8000],
                            text.len()
                        )
                    } else {
                        text.to_string()
                    };
                    CommandResult::Message(format!("Changes:\n{}", display))
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                CommandResult::Error(format!(
                    "git diff failed (exit {}): {}",
                    out.status.code().unwrap_or(-1),
                    stderr.trim()
                ))
            }
            Err(e) => CommandResult::Error(format!("Failed to run git diff: {}", e)),
        }
    }
}

// ---- /init ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for InitCommand {
    fn name(&self) -> &str {
        "init"
    }
    fn description(&self) -> &str {
        "Initialize a new project with AGENTS.md"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let path = ctx.working_dir.join("AGENTS.md");
        if path.exists() {
            return CommandResult::Message(format!(
                "AGENTS.md already exists at {}",
                path.display()
            ));
        }

        let default_content = "# Project Instructions\n\n\
            Add project-specific instructions and context here.\n\n\
            ## Guidelines\n\n\
            - Describe your project structure\n\
            - Note any coding conventions\n\
            - List important files and their purposes\n";

        match tokio::fs::write(&path, default_content).await {
            Ok(()) => CommandResult::Message(format!("Created AGENTS.md at {}", path.display())),
            Err(e) => CommandResult::Error(format!("Failed to create AGENTS.md: {}", e)),
        }
    }
}

// ---- /import-config ------------------------------------------------------

#[async_trait]
impl SlashCommand for ImportConfigCommand {
    fn name(&self) -> &str {
        "import-config"
    }
    fn description(&self) -> &str {
        "Import CLAUDE.md and settings.json from ~/.claude"
    }
    fn help(&self) -> &str {
        "Usage: /import-config [apply]\n\
         Import user-level Claude Code configuration from ~/.claude:\n\
           - ~/.claude/CLAUDE.md\n\
           - ~/.claude/settings.json\n\n\
         On a terminal this opens an import dialog with preview and confirmation.\n\
         Elsewhere it prints the same preview, and /import-config apply performs it."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        use mikmik_core::import_config::{build_import_preview, execute_import, ImportSelection};

        let args = args.trim();
        if args.eq_ignore_ascii_case("apply") {
            return match execute_import(ImportSelection::Both) {
                Ok(result) => CommandResult::Message(import_outcome(&result)),
                Err(e) => CommandResult::Error(format!("Import failed: {e}")),
            };
        }
        if !args.is_empty() {
            return CommandResult::Error(format!(
                "/import-config takes no argument, or \"apply\". Got \"{args}\"."
            ));
        }

        if ctx.interactive {
            return CommandResult::OpenImportConfigOverlay;
        }
        match build_import_preview(ImportSelection::Both) {
            Ok(preview) => CommandResult::Message(import_preview_text(&preview)),
            Err(e) => CommandResult::Error(format!("Could not read the configuration: {e}")),
        }
    }
}

/// What an import would do, for a caller with no dialog to confirm it in.
fn import_preview_text(preview: &mikmik_core::import_config::ImportPreview) -> String {
    let mut out = String::from("Run /import-config apply to carry this out.\n");

    match &preview.claude_md {
        Some(md) => out.push_str(&format!(
            "\nCLAUDE.md  {} → {}\n  {} lines, {} characters{}\n",
            md.plan.source_path.display(),
            md.plan.target_path.display(),
            md.line_count,
            md.char_count,
            if md.plan.target_exists {
                " (replaces the file already there)"
            } else {
                ""
            }
        )),
        None => out.push_str("\nCLAUDE.md  nothing to import\n"),
    }

    match &preview.settings {
        Some(settings) => {
            out.push_str(&format!(
                "\nsettings.json  {} → {}\n",
                settings.plan.source_path.display(),
                settings.plan.target_path.display()
            ));
            for field in &settings.fields {
                let reason = field
                    .reason
                    .as_deref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {:<8} {}{reason}\n",
                    field.action.label(),
                    field.name
                ));
            }
        }
        None => out.push_str("\nsettings.json  nothing to import\n"),
    }

    out.trim_end().to_string()
}

/// What an import did.
fn import_outcome(result: &mikmik_core::import_config::ImportExecutionResult) -> String {
    let mut out = String::new();
    if result.wrote_claude_md {
        out.push_str("Wrote CLAUDE.md.\n");
    }
    if result.wrote_settings {
        out.push_str(&format!(
            "Wrote settings.json: {} field{} imported.\n",
            result.imported_fields.len(),
            if result.imported_fields.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if !result.skipped_fields.is_empty() {
        out.push_str(&format!(
            "Left alone: {}.\n",
            result.skipped_fields.join(", ")
        ));
    }
    if out.is_empty() {
        // Saying nothing at all would read as success with no explanation.
        return "Nothing was imported: there was nothing to take.".to_string();
    }
    out.trim_end().to_string()
}

// ---- /hooks --------------------------------------------------------------

#[async_trait]
impl SlashCommand for HooksCommand {
    fn name(&self) -> &str {
        "hooks"
    }
    fn description(&self) -> &str {
        "Show configured event hooks"
    }
    fn help(&self) -> &str {
        "Usage: /hooks\n\
         Show hooks configured in settings.json under 'hooks'.\n\
         Hooks fire shell commands on events: PreToolUse, PostToolUse, Stop, UserPromptSubmit."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        // In TUI mode this command is intercepted by intercept_slash_command("hooks")
        // before execute() is ever called, so this path only runs in non-TUI
        // contexts (e.g., `claude hooks` on the CLI, pipes, or tests).
        //
        // Signal to the CLI driver that it should open the TUI overlay if possible;
        // the CLI will fall back to the text listing when no TUI is active.
        if ctx.config.hooks.is_empty() {
            // If there is nothing to show in the overlay, emit a helpful message
            // so the user knows what to do.
            return CommandResult::Message(
                "No hooks configured.\n\
                 Add hooks to ~/.config/mikmik/settings.json under the 'hooks' key.\n\
                 Example:\n\
                 \x20 \"hooks\": {\n\
                 \x20   \"PreToolUse\": [{ \"command\": \"cat\", \"tool_filter\": \"Bash\", \"blocking\": true }]\n\
                 \x20 }"
                    .to_string(),
            );
        }

        if !ctx.interactive {
            return CommandResult::Message(hooks_listing(&ctx.config.hooks));
        }

        // Return the overlay-open signal; the CLI driver will call
        // app.hooks_config_menu.open() or fall back to text output if running
        // without a TUI.
        CommandResult::OpenHooksOverlay
    }
}

/// The configured hooks as text, grouped by the event that fires them.
fn hooks_listing(hooks: &std::collections::HashMap<HookEvent, Vec<HookEntry>>) -> String {
    let mut events: Vec<(&HookEvent, &Vec<HookEntry>)> = hooks.iter().collect();
    // A map has no order of its own, and a listing that reshuffles itself
    // between runs is hard to read.
    events.sort_by_key(|(event, _)| format!("{event:?}"));

    let mut out = String::new();
    for (event, entries) in events {
        out.push_str(&format!("{event:?}\n"));
        for entry in entries {
            let filter = entry
                .tool_filter
                .as_deref()
                .map(|f| format!(" [{f}]"))
                .unwrap_or_default();
            let blocking = if entry.blocking { " (blocking)" } else { "" };
            out.push_str(&format!("  {}{filter}{blocking}\n", entry.command));
        }
    }
    out.trim_end().to_string()
}

// ---- /thinking -----------------------------------------------------------

#[async_trait]
impl SlashCommand for ThinkingCommand {
    fn name(&self) -> &str {
        "thinking"
    }
    fn description(&self) -> &str {
        "Toggle extended thinking mode"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["think"]
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        // Extended thinking is configured through the model; just inform the user
        let model = ctx.config.effective_model();
        if model.contains("claude-3-5") || model.contains("claude-3.5") {
            CommandResult::Message(
                "Extended thinking is not available for Claude 3.5 models.\n\
                 Use claude-opus-4-6 or claude-sonnet-4-6 for extended thinking."
                    .to_string(),
            )
        } else {
            CommandResult::Message(format!(
                "Extended thinking is available with {}.\n\
                 You can request thinking by asking MikMik to 'think step by step' or \
                 'think carefully before answering'.",
                model
            ))
        }
    }
}

// ---- Named-command slash adapters ----------------------------------------

#[async_trait]
impl SlashCommand for NamedCommandAdapter {
    fn name(&self) -> &str {
        self.slash_name
    }

    fn aliases(&self) -> Vec<&str> {
        self.slash_aliases.to_vec()
    }

    fn description(&self) -> &str {
        self.slash_description
    }

    fn help(&self) -> &str {
        self.slash_help
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        execute_named_command_from_slash(self.target_name, args, ctx)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Return all built-in slash commands.
pub fn all_commands() -> Vec<Box<dyn SlashCommand>> {
    vec![
        Box::new(HelpCommand),
        Box::new(ClearCommand),
        Box::new(CompactCommand),
        Box::new(CostCommand),
        Box::new(ExitCommand),
        Box::new(ModelCommand),
        Box::new(ConfigCommand),
        Box::new(ColorCommand),
        Box::new(PluginCommand),
        Box::new(VersionCommand),
        Box::new(ResumeCommand),
        Box::new(ReloadPluginsCommand),
        Box::new(StatusCommand),
        Box::new(DiffCommand),
        Box::new(MemoryCommand),
        Box::new(MemoriesCommand),
        Box::new(UsageCommand),
        Box::new(DoctorCommand),
        Box::new(LoginCommand),
        Box::new(LogoutCommand),
        Box::new(AccountsCommand),
        Box::new(SwitchCommand),
        Box::new(RefreshCommand),
        Box::new(InitCommand),
        Box::new(ReviewCommand),
        Box::new(HooksCommand),
        Box::new(ImportConfigCommand),
        Box::new(McpCommand),
        Box::new(PermissionsCommand),
        Box::new(PlanCommand),
        Box::new(TasksCommand),
        Box::new(SessionCommand),
        Box::new(ForkCommand),
        Box::new(ThinkingCommand),
        Box::new(ThemeCommand),
        Box::new(OutputStyleCommand),
        Box::new(KeybindingsCommand),
        Box::new(PrivacySettingsCommand),
        // New commands
        Box::new(ExportCommand),
        Box::new(ShareCommand),
        Box::new(LinksCommand),
        Box::new(SkillsCommand),
        Box::new(RewindCommand),
        Box::new(StatsCommand),
        Box::new(FilesCommand),
        Box::new(RenameCommand),
        Box::new(EffortCommand),
        Box::new(SummaryCommand),
        Box::new(CommitCommand),
        Box::new(NamedCommandAdapter {
            slash_name: "add-dir",
            target_name: "add-dir",
            slash_aliases: &[],
            slash_description: "Add a directory to MikMik's allowed workspace paths",
            slash_help: "Usage: /add-dir <path>",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "agents",
            target_name: "agents",
            slash_aliases: &[],
            slash_description: "Manage and configure sub-agents",
            slash_help: "Usage: /agents [list|create|edit|delete] [name]",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "branch",
            target_name: "branch",
            slash_aliases: &[],
            slash_description: "Create a branch of the current conversation at this point",
            slash_help: "Usage: /branch [create|switch|list] [name]",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "tag",
            target_name: "tag",
            slash_aliases: &[],
            slash_description: "Toggle a searchable tag on the current session",
            slash_help: "Usage: /tag [list|add|remove] [tag]",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "passes",
            target_name: "passes",
            slash_aliases: &[],
            slash_description: "Share a free week of MikMik with friends",
            slash_help: "Usage: /passes",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "ide",
            target_name: "ide",
            slash_aliases: &[],
            slash_description: "Manage IDE integrations and show status",
            slash_help: "Usage: /ide [status|connect|disconnect|open]",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "pr-comments",
            target_name: "pr-comments",
            slash_aliases: &[],
            slash_description: "Get comments from a GitHub pull request",
            slash_help: "Usage: /pr-comments <PR-number>",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "desktop",
            target_name: "desktop",
            slash_aliases: &[],
            slash_description: "Open the MikMik desktop app",
            slash_help: "Usage: /desktop",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "mobile",
            target_name: "mobile",
            slash_aliases: &[],
            slash_description: "Set up MikMik on mobile",
            slash_help: "Usage: /mobile",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "install-github-app",
            target_name: "install-github-app",
            slash_aliases: &[],
            slash_description: "Set up MikMik GitHub Actions for a repository",
            slash_help: "Usage: /install-github-app",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "web-setup",
            target_name: "remote-setup",
            slash_aliases: &["remote-setup"],
            slash_description: "Configure a remote MikMik environment",
            slash_help: "Usage: /web-setup",
        }),
        Box::new(NamedCommandAdapter {
            slash_name: "stickers",
            target_name: "stickers",
            slash_aliases: &[],
            slash_description: "View collected stickers",
            slash_help: "Usage: /stickers",
        }),
        // Batch-1 new commands
        Box::new(RemoteControlCommand),
        Box::new(WorkspaceCommand),
        Box::new(RemoteEnvCommand),
        Box::new(ContextCommand),
        Box::new(CopyCommand),
        Box::new(ChromeCommand),
        Box::new(VimCommand),
        Box::new(TimelineCommand),
        Box::new(VoiceCommand),
        Box::new(UpgradeCommand),
        Box::new(ReleaseNotesCommand),
        Box::new(RateLimitOptionsCommand),
        Box::new(StatuslineCommand),
        Box::new(SecurityReviewCommand),
        Box::new(TerminalSetupCommand),
        Box::new(ExtraUsageCommand),
        Box::new(FastCommand),
        Box::new(ThinkBackCommand),
        Box::new(ThinkBackPlayCommand),
        Box::new(ColorSetCommand),
        // New commands: teleport, btw, sandbox-toggle
        Box::new(TeleportCommand),
        Box::new(BtwCommand),
        Box::new(SandboxToggleCommand),
        // Advisor
        Box::new(AdvisorCommand),
        // Companion
        Box::new(BuddyCommand),
        // Diagnostics / analysis
        Box::new(HeapdumpCommand),
        Box::new(InsightsCommand),
        Box::new(UltrareviewCommand),
        // Snapshot / revert system
        Box::new(UndoCommand),
        Box::new(RevertCommand),
        Box::new(CheckpointsCommand),
        Box::new(CheckpointCommand),
        Box::new(SnapshotDiffCommand),
        // Multi-provider support
        Box::new(ProvidersCommand),
        Box::new(ConnectCommand),
        // Named agent system
        Box::new(AgentCommand),
        // Session search (SQLite)
        Box::new(SearchCommand),
        // Managed agent (manager-executor) architecture
        Box::new(ManagedAgentsCommand),
        // Durable long-running goals
        Box::new(GoalCommand),
        // Guided goal setup: draw out the objective, then create the goal
        Box::new(GuidedGoalCommand),
        // The session's TodoWrite list
        Box::new(TodosCommand),
        // The agentic turn limit
        Box::new(PokeCommand),
        Box::new(TurnsCommand),
        Box::new(YoloCommand),
        // Session navigation ported from opencode: /new (lazy home) + /move.
        Box::new(NewCommand),
        Box::new(MoveCommand),
    ]
}

/// Find a command by name or alias.
pub fn find_command(name: &str) -> Option<Box<dyn SlashCommand>> {
    let name = name.trim_start_matches('/');
    all_commands()
        .into_iter()
        .find(|c| c.name() == name || c.aliases().contains(&name))
}

/// Build `HelpEntry` values for all non-hidden commands, suitable for
/// populating `HelpOverlay::commands` at startup.
pub fn build_help_entries() -> Vec<mikmik_tui::overlays::HelpEntry> {
    all_commands()
        .iter()
        .filter(|c| !c.hidden())
        .map(|c| mikmik_tui::overlays::HelpEntry {
            name: c.name().to_string(),
            aliases: c.aliases().join(", "),
            description: c.description().to_string(),
            category: command_category(c.name()).to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// User-defined command templates (Feature 2)
// ---------------------------------------------------------------------------

/// A slash command backed by a user-defined template in `settings.json`.
struct TemplateCommand {
    name: String,
    template: mikmik_core::CommandTemplate,
}

#[async_trait]
impl SlashCommand for TemplateCommand {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        self.template
            .description
            .as_deref()
            .unwrap_or("Custom command")
    }
    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let mut words = args.split_whitespace();
        let arg1 = words.next().unwrap_or("");
        let arg2 = words.next().unwrap_or("");
        let prompt = self
            .template
            .template
            .replace("$ARGUMENTS", args)
            .replace("$1", arg1)
            .replace("$2", arg2);
        CommandResult::UserMessage(prompt)
    }
}

/// Build slash commands from user-defined command templates stored in
/// `settings.commands`.
pub fn commands_from_settings(settings: &mikmik_core::Settings) -> Vec<Box<dyn SlashCommand>> {
    settings
        .commands
        .iter()
        .map(|(name, template)| {
            Box::new(TemplateCommand {
                name: name.clone(),
                template: template.clone(),
            }) as Box<dyn SlashCommand>
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Discovered skill commands (from .mikmik/skills/ and git URLs)
// ---------------------------------------------------------------------------

/// A slash command backed by a discovered skill markdown file.
struct SkillCommand {
    name: String,
    description: String,
    template: String,
}

#[async_trait]
impl SlashCommand for SkillCommand {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let mut words = args.split_whitespace();
        let arg1 = words.next().unwrap_or("");
        let arg2 = words.next().unwrap_or("");
        let prompt = self
            .template
            .replace("$ARGUMENTS", args)
            .replace("$1", arg1)
            .replace("$2", arg2);
        CommandResult::UserMessage(prompt)
    }
}

/// Build slash commands from skill markdown files discovered on the filesystem
/// and from configured git URLs.
///
/// Pass the project `cwd` and the `skills` section of the effective config.
/// Bundled skills take precedence — any discovered skill whose name clashes
/// with a built-in command will be silently skipped.
pub fn commands_from_discovered_skills(
    cwd: &std::path::Path,
    skills_config: &mikmik_core::SkillsConfig,
) -> Vec<Box<dyn SlashCommand>> {
    let discovered = mikmik_core::discover_skills(cwd, skills_config);
    // Build a set of built-in command names so we can skip collisions.
    let all_cmds = all_commands();
    let builtin_names: std::collections::HashSet<&str> =
        all_cmds.iter().map(|c| c.name()).collect();

    discovered
        .into_iter()
        // A skill whose bare command name clashes with a built-in stays skipped
        // (the built-in wins that slot); a qualified `name@origin` never clashes
        // with a built-in, so its siblings remain reachable.
        .filter(|resolved| !builtin_names.contains(resolved.command_name.as_str()))
        .map(|resolved| {
            let description = resolved.tagged_description();
            Box::new(SkillCommand {
                name: resolved.command_name,
                description,
                template: resolved.skill.template,
            }) as Box<dyn SlashCommand>
        })
        .collect()
}

/// Execute a slash command string (with leading /).
pub async fn execute_command(input: &str, ctx: &mut CommandContext) -> Option<CommandResult> {
    if !mikmik_tui::input::is_slash_command(input) {
        return None;
    }
    let (name, args) = mikmik_tui::input::parse_slash_command(input);

    // First check built-in commands.
    if let Some(cmd) = find_command(name) {
        return Some(cmd.execute(args, ctx).await);
    }

    // Check user-defined command templates from settings.
    let cmd_name = name.trim_start_matches('/');
    if let Some(tmpl) = ctx.config.commands.get(cmd_name).cloned() {
        let tc = TemplateCommand {
            name: cmd_name.to_string(),
            template: tmpl,
        };
        return Some(tc.execute(args, ctx).await);
    }

    // Check discovered skill commands (from .mikmik/skills/, git URLs, etc.).
    // A skill is reached by its resolved command name, which is the bare name
    // for the highest-priority skill and `name@origin` for any that clash.
    {
        let discovered = mikmik_core::discover_skills(&ctx.working_dir, &ctx.config.skills);
        if let Some(resolved) = discovered.into_iter().find(|r| r.command_name == cmd_name) {
            let sc = SkillCommand {
                name: resolved.command_name,
                description: resolved.skill.description,
                template: resolved.skill.template,
            };
            return Some(sc.execute(args, ctx).await);
        }
    }

    // Then check plugin-defined slash commands.
    let project_dir = ctx.working_dir.clone();
    let registry = mikmik_plugins::load_plugins(&project_dir, &[]).await;
    for cmd_def in registry.all_command_defs() {
        if cmd_def.name == cmd_name {
            let adapter = PluginSlashCommandAdapter { def: cmd_def };
            return Some(adapter.execute(args, ctx).await);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Named commands module (top-level `claude <name>` subcommands)
// ---------------------------------------------------------------------------
pub mod named_commands;

// ---------------------------------------------------------------------------
// Stats analytics (persisted transcript aggregation) — backs `mikmik stats`.
// The current-session `/stats` slash command lives above; this module reads
// JSONL transcripts on disk.
// ---------------------------------------------------------------------------
pub mod stats;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::cost::CostTracker;

    fn make_ctx() -> CommandContext {
        CommandContext {
            context_window: 200_000,
            context_used_tokens: 0,
            config: mikmik_core::config::Config::default(),
            cost_tracker: CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("."),
            session_id: "test-session".to_string(),
            session_title: None,
            effort_level: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            interactive: true,
            active_agent: None,
        }
    }

    // ---- Commands that would open a view ------------------------------------

    /// The example `/hooks` prints is the only shape a first-time reader has.
    ///
    /// It used to print the plugin manifest's nested `matcher`/`hooks` form
    /// with a `"type": "command"` field. `HookEntry` has neither: pasting it
    /// into `settings.json` fails to deserialize, and a settings file that
    /// fails to parse takes the model, the provider and every other setting
    /// down with it. Parse the example rather than eyeballing it.
    #[tokio::test]
    async fn the_hooks_example_parses_as_a_hook_map() {
        use mikmik_core::config::{HookEntry, HookEvent};
        use std::collections::HashMap;

        let mut ctx = make_ctx();
        ctx.interactive = false;
        let CommandResult::Message(text) = HooksCommand.execute("", &mut ctx).await else {
            panic!("an empty hook map should print the example");
        };

        // Take the hook map the message embeds, braces included.
        let start = text.find('{').expect("the example has an object");
        let end = text.rfind('}').expect("the example closes it");
        let object = &text[start..=end];

        let parsed: HashMap<HookEvent, Vec<HookEntry>> = serde_json::from_str(object)
            .unwrap_or_else(|e| panic!("the printed example does not parse ({e}):\n{object}"));

        let entries = parsed
            .get(&HookEvent::PreToolUse)
            .expect("the example names PreToolUse");
        assert_eq!(entries.len(), 1);
        assert!(
            !entries[0].command.is_empty(),
            "a hook entry without a command runs nothing"
        );
    }

    /// What `/config` offers has to be what `/config set` accepts.
    ///
    /// The usage line used to spell the style names out, and named two
    /// (`formal`, `casual`) that no longer exist, so following the command's
    /// own instructions produced "Unsupported output style".
    #[tokio::test]
    async fn the_config_usage_offers_only_styles_set_accepts() {
        let mut ctx = make_ctx();
        let CommandResult::Message(text) =
            crate::config_cmd::ConfigCommand.execute("", &mut ctx).await
        else {
            panic!("an argument-less /config should print the configuration");
        };

        let line = text
            .lines()
            .find(|l| l.contains("/config set output-style"))
            .expect("the usage names output-style");
        let offered: Vec<&str> = line
            .rsplit_once('<')
            .and_then(|(_, rest)| rest.strip_suffix('>'))
            .expect("the styles are listed between angle brackets")
            .split('|')
            .collect();

        let accepted = available_output_style_names();
        for name in &offered {
            assert!(
                accepted.iter().any(|a| a == name),
                "/config offers '{name}', which /config set refuses. Offered: {offered:?}"
            );
        }
        assert_eq!(
            offered.len(),
            accepted.len(),
            "the usage line drops styles /config set would take"
        );
    }

    /// `/context` reports the window it was handed, not a constant.
    ///
    /// It used to hardcode 200_000 and call it "all current Claude models".
    /// Opus 5 carries 1M and Gemini carries more, so the percentage was wrong
    /// for every model that does not share the Anthropic default.
    #[tokio::test]
    async fn the_context_report_uses_the_window_it_was_given() {
        let mut ctx = make_ctx();
        ctx.context_window = 1_000_000;
        ctx.context_used_tokens = 250_000;

        let CommandResult::Message(text) =
            crate::display::ContextCommand.execute("", &mut ctx).await
        else {
            panic!("/context should print a report");
        };

        assert!(
            text.contains("1000000 tokens"),
            "the report names a window that is not the one it was given:\n{text}"
        );
        assert!(
            text.contains("(25.0%)"),
            "250k of a 1M window is 25%, not {}:\n{text}",
            "50%"
        );
    }

    /// The measured figure is the last turn's, never the session total.
    ///
    /// `/context` used to divide `cost_tracker.total_tokens()` by the window.
    /// That tracker accumulates every turn's usage and is never reset, so the
    /// percentage passed 100% while the context still had room.
    #[tokio::test]
    async fn the_context_report_ignores_the_accumulating_tracker() {
        let mut ctx = make_ctx();
        ctx.context_window = 200_000;
        ctx.context_used_tokens = 20_000;
        // Five turns of 60k each: far past the window, and no reason to be.
        for _ in 0..5 {
            ctx.cost_tracker.add_usage(
                "claude-opus-5",
                mikmik_core::cost::ModelPricing::default(),
                60_000,
                0,
                0,
                0,
            );
        }
        assert!(
            ctx.cost_tracker.total_tokens() > ctx.context_window,
            "the test needs a tracker total that exceeds the window"
        );

        let CommandResult::Message(text) =
            crate::display::ContextCommand.execute("", &mut ctx).await
        else {
            panic!("/context should print a report");
        };

        assert!(
            text.contains("20000 tokens (10.0%)"),
            "the report should show the last turn's 20k, not the tracker's total:\n{text}"
        );
    }

    /// The rate card `/cost` prints must be the rates the total was billed at.
    ///
    /// It used to call `ModelPricing::for_model`, which reads a model name for
    /// `opus`, `haiku` or `free` and prices everything else as Claude Sonnet.
    /// A Gemini turn is billed from the registry, so `/cost` printed Sonnet's
    /// $3/$15 above a total computed at Gemini's rates.
    #[tokio::test]
    async fn the_cost_rate_card_shows_the_rates_that_were_billed() {
        let billed = mikmik_core::cost::ModelPricing {
            input_per_mtk: 0.3,
            output_per_mtk: 2.5,
            cache_creation_per_mtk: 0.375,
            cache_read_per_mtk: 0.03,
        };
        assert_ne!(
            billed,
            mikmik_core::cost::ModelPricing::for_model("gemini-2.5-flash"),
            "the test needs a rate the name heuristic would not guess"
        );

        let mut ctx = make_ctx();
        ctx.cost_tracker
            .add_usage("gemini-2.5-flash", billed, 1_000_000, 0, 0, 0);

        let CommandResult::Message(text) = CostCommand.execute("", &mut ctx).await else {
            panic!("/cost should print a report");
        };
        let rates = text
            .lines()
            .find(|line| line.contains("Rates ($/MTok)"))
            .expect("the report carries a rate card");

        assert!(
            rates.contains("input $0.30"),
            "the rate card should show the billed $0.30, not a guess: {rates}"
        );
        assert!(
            !rates.contains("input $3.00"),
            "the rate card fell back to Claude Sonnet's rate: {rates}"
        );
    }

    /// `/ctx-viz` was folded into `/context`; its names must still resolve.
    #[test]
    fn the_old_context_visualizer_names_still_resolve() {
        for name in ["context", "ctx", "ctx-viz", "context-visualizer"] {
            let command =
                find_command(name).unwrap_or_else(|| panic!("/{name} resolves to nothing"));
            assert_eq!(
                command.name(),
                "context",
                "/{name} should reach /context, not /{}",
                command.name()
            );
        }
    }

    #[tokio::test]
    async fn rewind_lists_the_messages_for_a_caller_with_no_overlay() {
        let mut ctx = make_ctx();
        ctx.interactive = false;
        ctx.messages = vec![
            Message::user("fix the parser"),
            Message::assistant("which one"),
        ];

        let result = crate::session_tools::RewindCommand
            .execute("", &mut ctx)
            .await;

        let CommandResult::Message(text) = result else {
            panic!("expected a listing, got {result:?}");
        };
        assert!(text.contains("fix the parser"), "{text}");
        assert!(text.contains("which one"), "{text}");
        // The listing has to say how to act on it, or it is a dead end.
        assert!(text.contains("/rewind <n>"), "{text}");
    }

    #[tokio::test]
    async fn rewind_with_a_count_keeps_that_many_messages() {
        let mut ctx = make_ctx();
        ctx.interactive = false;
        ctx.messages = vec![
            Message::user("one"),
            Message::assistant("two"),
            Message::user("three"),
        ];

        let result = crate::session_tools::RewindCommand
            .execute("2", &mut ctx)
            .await;

        let CommandResult::SetMessages(kept) = result else {
            panic!("expected a rewind, got {result:?}");
        };
        assert_eq!(kept.len(), 2);
    }

    #[tokio::test]
    async fn rewind_past_the_end_is_refused_rather_than_clamped() {
        // Clamping would answer a request nobody made, and the caller would
        // never learn the number was wrong.
        let mut ctx = make_ctx();
        ctx.interactive = false;
        ctx.messages = vec![Message::user("one")];

        let result = crate::session_tools::RewindCommand
            .execute("9", &mut ctx)
            .await;

        assert!(
            matches!(result, CommandResult::Error(_)),
            "expected a refusal, got {result:?}"
        );
    }

    /// `/compact` asks the session loop to summarise. It used to answer with
    /// `UserMessage("[Compact requested (N messages)...]")`, which appended a
    /// paragraph to the conversation and so made the context larger.
    #[tokio::test]
    async fn compact_asks_for_a_real_compaction() {
        let mut ctx = make_ctx();
        ctx.messages = vec![Message::user("one"), Message::assistant("two")];

        let result = CompactCommand.execute("", &mut ctx).await;

        assert!(
            matches!(result, CommandResult::RunCompaction { instruction: None }),
            "expected a compaction request, got {result:?}"
        );
    }

    /// `/compact <instruction>` steers the summary rather than being dropped.
    #[tokio::test]
    async fn compact_carries_the_users_instruction() {
        let mut ctx = make_ctx();

        let result = CompactCommand
            .execute("  keep every file path  ", &mut ctx)
            .await;

        let CommandResult::RunCompaction { instruction } = result else {
            panic!("expected a compaction request");
        };
        assert_eq!(instruction.as_deref(), Some("keep every file path"));
    }

    #[tokio::test]
    async fn rewind_still_opens_the_overlay_on_a_terminal() {
        let mut ctx = make_ctx();
        ctx.messages = vec![Message::user("one")];

        let result = crate::session_tools::RewindCommand
            .execute("", &mut ctx)
            .await;

        assert!(matches!(result, CommandResult::OpenRewindOverlay));
    }

    #[tokio::test]
    async fn hooks_are_printed_for_a_caller_with_no_overlay() {
        let mut ctx = make_ctx();
        ctx.interactive = false;
        ctx.config.hooks.insert(
            HookEvent::PreToolUse,
            vec![HookEntry {
                command: "echo before".to_string(),
                tool_filter: Some("Bash".to_string()),
                blocking: true,
                timeout_ms: None,
            }],
        );

        let result = HooksCommand.execute("", &mut ctx).await;

        let CommandResult::Message(text) = result else {
            panic!("expected a listing, got {result:?}");
        };
        assert!(text.contains("PreToolUse"), "{text}");
        assert!(text.contains("echo before"), "{text}");
        assert!(text.contains("Bash"), "{text}");
        assert!(text.contains("blocking"), "{text}");
    }

    #[tokio::test]
    async fn hooks_still_opens_the_overlay_on_a_terminal() {
        let mut ctx = make_ctx();
        ctx.config.hooks.insert(
            HookEvent::Stop,
            vec![HookEntry {
                command: "echo done".to_string(),
                ..Default::default()
            }],
        );

        let result = HooksCommand.execute("", &mut ctx).await;

        assert!(matches!(result, CommandResult::OpenHooksOverlay));
    }

    #[tokio::test]
    async fn import_config_refuses_an_argument_it_does_not_know() {
        // Anything but "apply" would otherwise be taken as a request to
        // preview, which is not what was asked.
        let mut ctx = make_ctx();
        ctx.interactive = false;

        let result = ImportConfigCommand.execute("everything", &mut ctx).await;

        assert!(
            matches!(result, CommandResult::Error(_)),
            "expected a refusal, got {result:?}"
        );
    }

    #[tokio::test]
    async fn import_config_still_opens_the_dialog_on_a_terminal() {
        let mut ctx = make_ctx();

        let result = ImportConfigCommand.execute("", &mut ctx).await;

        assert!(matches!(result, CommandResult::OpenImportConfigOverlay));
    }

    // ---- Command registry tests ---------------------------------------------

    #[test]
    fn test_all_commands_non_empty() {
        assert!(!all_commands().is_empty());
    }

    #[test]
    fn test_all_commands_have_unique_names() {
        let mut names = std::collections::HashSet::new();
        for cmd in all_commands() {
            assert!(
                names.insert(cmd.name().to_string()),
                "Duplicate command name: {}",
                cmd.name()
            );
        }
    }

    #[test]
    fn test_find_command_by_name() {
        assert!(find_command("help").is_some());
        assert!(find_command("clear").is_some());
        assert!(find_command("exit").is_some());
        assert!(find_command("model").is_some());
        assert!(find_command("refresh").is_some());
        assert!(find_command("version").is_some());
    }

    #[test]
    fn test_find_command_with_slash_prefix() {
        // find_command should strip the leading / before lookup
        assert!(find_command("/help").is_some());
        assert!(find_command("/clear").is_some());
    }

    #[test]
    fn test_find_command_by_alias() {
        // /help has aliases "h" and "?"
        assert!(find_command("h").is_some());
        assert!(find_command("?").is_some());
        // /clear has alias "c"
        assert!(find_command("c").is_some());
        assert!(find_command("settings").is_some());
        assert!(find_command("continue").is_some());
        assert!(find_command("bashes").is_some());
        assert!(find_command("remote").is_some());
        assert!(find_command("remote-setup").is_some());
    }

    #[test]
    fn test_find_command_not_found() {
        assert!(find_command("nonexistent_command_xyz").is_none());
    }

    #[test]
    fn test_core_commands_present() {
        let expected = [
            "help",
            "clear",
            "compact",
            "cost",
            "exit",
            "model",
            "config",
            "version",
            "status",
            "diff",
            "memory",
            "hooks",
            "permissions",
            "plan",
            "tasks",
            "session",
            "login",
            "logout",
            "refresh",
            "usage",
            "plugin",
            "reload-plugins",
            "add-dir",
            "agents",
            "branch",
            "tag",
            "passes",
            "ide",
            "pr-comments",
            "desktop",
            "mobile",
            "install-github-app",
            "web-setup",
            "stickers",
        ];
        for name in &expected {
            assert!(
                find_command(name).is_some(),
                "Expected command '{}' not in all_commands()",
                name
            );
        }
    }

    // ---- Command execution tests --------------------------------------------

    /// The report has to read the session, not name a level.
    ///
    /// It used to answer "normal" whatever the session was doing, and a remote
    /// client has no picker to check that against.
    #[tokio::test]
    async fn effort_reports_the_level_in_force() {
        let mut ctx = make_ctx();
        ctx.effort_level = Some(mikmik_core::effort::EffortLevel::XHigh);
        let cmd = find_command("effort").expect("the command exists");

        let CommandResult::Message(text) = cmd.execute("", &mut ctx).await else {
            panic!("an argument-less /effort should report");
        };
        assert!(text.contains("xhigh"), "{text}");
        assert!(!text.contains("normal"), "{text}");
    }

    #[tokio::test]
    async fn effort_says_so_when_nothing_was_chosen() {
        // Unset is not the same as the default: it sends no reasoning
        // configuration at all, so the report must not invent a level.
        let mut ctx = make_ctx();
        let cmd = find_command("effort").expect("the command exists");

        let CommandResult::Message(text) = cmd.execute("", &mut ctx).await else {
            panic!("an argument-less /effort should report");
        };
        assert!(text.contains("unset"), "{text}");
    }

    /// The picker offers the whole ladder, and a remote client cannot open the
    /// picker, so the command has to reach every level too.
    #[tokio::test]
    async fn effort_accepts_the_whole_ladder() {
        let mut ctx = make_ctx();
        let cmd = find_command("effort").expect("the command exists");

        for level in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let result = cmd.execute(level, &mut ctx).await;
            assert!(
                matches!(result, CommandResult::ConfigChange(_)),
                "/effort {level} was refused: {result:?}"
            );
            assert_eq!(
                ctx.effort_level,
                mikmik_core::effort::EffortLevel::from_str(level)
            );
        }
    }

    #[tokio::test]
    async fn the_three_original_words_still_move_the_output_limit() {
        // Kept on purpose. Widening it to the rest of the ladder would change
        // the limit for levels that never touched it.
        let mut ctx = make_ctx();
        let cmd = find_command("effort").expect("the command exists");

        cmd.execute("low", &mut ctx).await;
        assert_eq!(ctx.config.max_tokens, Some(4096));
        cmd.execute("high", &mut ctx).await;
        assert_eq!(ctx.config.max_tokens, Some(32768));
        cmd.execute("normal", &mut ctx).await;
        assert_eq!(ctx.config.max_tokens, None);

        cmd.execute("xhigh", &mut ctx).await;
        assert_eq!(ctx.config.max_tokens, None, "xhigh must leave it alone");
    }

    // ---- per-model cost reporting ------------------------------------------

    #[tokio::test]
    async fn cost_lists_every_model_that_spent() {
        let mut ctx = make_ctx();
        ctx.cost_tracker.add_usage(
            "claude-opus-4-6",
            mikmik_core::cost::ModelPricing::for_model("claude-opus-4-6"),
            1000,
            500,
            0,
            0,
        );
        ctx.cost_tracker.add_usage(
            "claude-haiku-4-5",
            mikmik_core::cost::ModelPricing::for_model("claude-haiku-4-5"),
            4000,
            2000,
            0,
            0,
        );

        let cmd = find_command("cost").expect("the command exists");
        let CommandResult::Message(text) = cmd.execute("", &mut ctx).await else {
            panic!("/cost should report");
        };

        assert!(text.contains("By model:"), "no breakdown in:\n{text}");
        assert!(text.contains("claude-opus-4-6"), "missing Opus in:\n{text}");
        assert!(
            text.contains("claude-haiku-4-5"),
            "missing Haiku in:\n{text}"
        );
    }

    #[tokio::test]
    async fn stats_lists_every_model_that_spent() {
        let mut ctx = make_ctx();
        ctx.cost_tracker.add_usage(
            "claude-opus-4-6",
            mikmik_core::cost::ModelPricing::for_model("claude-opus-4-6"),
            1000,
            500,
            0,
            0,
        );
        ctx.cost_tracker.add_usage(
            "claude-haiku-4-5",
            mikmik_core::cost::ModelPricing::for_model("claude-haiku-4-5"),
            4000,
            2000,
            0,
            0,
        );

        let cmd = find_command("stats").expect("the command exists");
        let CommandResult::Message(text) = cmd.execute("", &mut ctx).await else {
            panic!("/stats should report");
        };

        assert!(text.contains("By model:"), "no breakdown in:\n{text}");
        assert!(text.contains("claude-opus-4-6"), "missing Opus in:\n{text}");
        assert!(
            text.contains("claude-haiku-4-5"),
            "missing Haiku in:\n{text}"
        );
    }

    #[tokio::test]
    async fn cost_rows_add_up_to_the_total_across_models() {
        let mut ctx = make_ctx();
        ctx.cost_tracker.add_usage(
            "claude-opus-4-6",
            mikmik_core::cost::ModelPricing::for_model("claude-opus-4-6"),
            100_000,
            20_000,
            0,
            0,
        );
        ctx.cost_tracker.add_usage(
            "claude-haiku-4-5",
            mikmik_core::cost::ModelPricing::for_model("claude-haiku-4-5"),
            500_000,
            80_000,
            0,
            0,
        );

        let cmd = find_command("cost").expect("the command exists");
        let CommandResult::Message(text) = cmd.execute("", &mut ctx).await else {
            panic!("/cost should report");
        };

        let dollars = |label: &str| -> f64 {
            let line = text
                .lines()
                .find(|l| l.contains(label))
                .unwrap_or_else(|| panic!("no {label} line in:\n{text}"));
            let field = line
                .rsplit('$')
                .next()
                .unwrap_or_else(|| panic!("no amount on: {line}"));
            field
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("{field:?} on {line}: {e}"))
        };

        let rows = dollars("Input tokens:")
            + dollars("Output tokens:")
            + dollars("Cache write:")
            + dollars("Cache read:");
        let total = dollars("Total cost:");
        assert!(
            (rows - total).abs() < 1e-4,
            "rows {rows} against total {total} in:\n{text}"
        );
    }

    #[tokio::test]
    async fn one_rate_card_is_only_shown_when_one_model_spent() {
        let mut ctx = make_ctx();
        ctx.cost_tracker.add_usage(
            "claude-opus-4-6",
            mikmik_core::cost::ModelPricing::for_model("claude-opus-4-6"),
            1000,
            500,
            0,
            0,
        );
        ctx.cost_tracker.add_usage(
            "claude-haiku-4-5",
            mikmik_core::cost::ModelPricing::for_model("claude-haiku-4-5"),
            1000,
            500,
            0,
            0,
        );

        let cmd = find_command("cost").expect("the command exists");
        let CommandResult::Message(text) = cmd.execute("", &mut ctx).await else {
            panic!("/cost should report");
        };
        assert!(
            text.contains("vary by model"),
            "two models need no single rate card:\n{text}"
        );
    }

    #[tokio::test]
    async fn the_rate_card_names_the_model_that_spent() {
        // The single spender need not be the session model; only an advisor
        // may have run.
        let mut ctx = make_ctx();
        ctx.cost_tracker.add_usage(
            "claude-haiku-4-5",
            mikmik_core::cost::ModelPricing::for_model("claude-haiku-4-5"),
            1000,
            500,
            0,
            0,
        );

        let cmd = find_command("cost").expect("the command exists");
        let CommandResult::Message(text) = cmd.execute("", &mut ctx).await else {
            panic!("/cost should report");
        };
        assert!(
            text.contains("Rates ($/MTok) for claude-haiku-4-5:"),
            "the rate card must name its model:\n{text}"
        );
    }

    #[tokio::test]
    async fn a_session_that_spent_nothing_gets_no_breakdown() {
        let mut ctx = make_ctx();
        let cmd = find_command("cost").expect("the command exists");
        let CommandResult::Message(text) = cmd.execute("", &mut ctx).await else {
            panic!("/cost should report");
        };
        assert!(
            !text.contains("By model:"),
            "an empty breakdown is noise:\n{text}"
        );
    }

    #[tokio::test]
    async fn test_clear_command_returns_clear_conversation() {
        let mut ctx = make_ctx();
        let cmd = find_command("clear").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::ClearConversation));
    }

    // ---- /output-style + /keybindings end-to-end (issue #278 point 2) ------

    #[tokio::test]
    async fn output_style_lists_personas_and_current() {
        // The empty-arg path only reads (no disk write) and must surface the
        // built-in styles including the newly-consolidated personas.
        let mut ctx = make_ctx();
        let cmd = find_command("output-style").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        let CommandResult::Message(text) = result else {
            panic!("empty /output-style should list styles, got {result:?}");
        };
        assert!(
            text.contains("caveman"),
            "personas must appear in the list: {text}"
        );
        assert!(text.contains("rocky"));
        assert!(text.contains("default"));
        // Default config → default is the current style.
        assert!(text.contains("Current output style: default"));
    }

    #[tokio::test]
    async fn output_style_rejects_unknown_name() {
        let mut ctx = make_ctx();
        let cmd = find_command("output-style").unwrap();
        let result = cmd.execute("definitely-not-a-style", &mut ctx).await;
        assert!(matches!(result, CommandResult::Error(_)));
    }

    #[test]
    fn available_output_styles_include_personas() {
        let names = available_output_style_names();
        for expected in ["default", "concise", "caveman", "rocky"] {
            assert!(
                names.iter().any(|n| n == expected),
                "output style '{expected}' should be available"
            );
        }
    }

    #[test]
    fn persisted_persona_resolves_to_its_prompt() {
        // End-to-end of the persist path: /output-style / /rocky set
        // config.output_style, which resolves to the persona's prompt text for
        // the system prompt.
        let config = mikmik_core::config::Config {
            output_style: Some("rocky".to_string()),
            ..mikmik_core::config::Config::default()
        };
        let prompt = config
            .resolve_output_style_prompt()
            .expect("rocky must resolve to a prompt");
        assert!(prompt.contains("Project Hail Mary"));
    }

    #[test]
    fn keybindings_template_is_valid_json() {
        // /keybindings writes this template on first run; ensure it always
        // generates and parses so the command cannot fail generating its file.
        let template = generate_keybindings_template().expect("template must generate");
        let parsed: serde_json::Value =
            serde_json::from_str(&template).expect("template must be valid JSON");
        assert!(
            parsed.get("bindings").is_some(),
            "template needs a bindings block"
        );
    }

    #[test]
    fn test_new_and_move_commands_present() {
        assert!(find_command("new").is_some());
        assert!(find_command("move").is_some());
    }

    #[test]
    fn test_clear_no_longer_aliases_new() {
        // /new is now its own lazy-home command; /clear keeps its other aliases.
        let clear = find_command("clear").unwrap();
        assert!(!clear.aliases().contains(&"new"));
        assert_eq!(find_command("new").unwrap().name(), "new");
    }

    #[tokio::test]
    async fn test_new_command_returns_new_session() {
        let mut ctx = make_ctx();
        let cmd = find_command("new").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::NewSession));
    }

    #[tokio::test]
    async fn test_move_command_without_dir_shows_usage() {
        let mut ctx = make_ctx();
        let cmd = find_command("move").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // No target → usage message, never a MoveSession side effect.
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[tokio::test]
    async fn test_move_command_rejects_missing_directory() {
        let mut ctx = make_ctx();
        let cmd = find_command("move").unwrap();
        let result = cmd
            .execute("/definitely/not/a/real/path/xyz123", &mut ctx)
            .await;
        assert!(matches!(result, CommandResult::Error(_)));
    }

    #[tokio::test]
    async fn test_refresh_command_requests_provider_reset() {
        let mut ctx = make_ctx();
        let cmd = find_command("refresh").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::RefreshProviderState));
    }

    #[tokio::test]
    async fn test_exit_command_returns_exit() {
        let mut ctx = make_ctx();
        let cmd = find_command("exit").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Exit));
    }

    #[tokio::test]
    async fn test_version_command_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("version").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Message(_)));
        if let CommandResult::Message(msg) = result {
            assert!(
                msg.contains("claude") || msg.contains("MikMik") || msg.contains('.'),
                "Version message should contain version number, got: {}",
                msg
            );
        }
    }

    #[tokio::test]
    async fn test_cost_command_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("cost").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[tokio::test]
    async fn test_login_command_starts_oauth_flow() {
        let mut ctx = make_ctx();
        let cmd = find_command("login").unwrap();
        // Default (no --console) → Anthropic, login_with_claude_ai = true
        let result = cmd.execute("", &mut ctx).await;
        match result {
            CommandResult::StartLoginForProvider {
                provider,
                login_with_claude_ai,
                label,
            } => {
                assert_eq!(provider, mikmik_core::ProviderId::ANTHROPIC);
                assert!(login_with_claude_ai);
                assert!(label.is_none());
            }
            other => panic!("expected StartLoginForProvider, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_login_command_console_flag() {
        let mut ctx = make_ctx();
        let cmd = find_command("login").unwrap();
        let result = cmd.execute("--console", &mut ctx).await;
        match result {
            CommandResult::StartLoginForProvider {
                provider,
                login_with_claude_ai,
                ..
            } => {
                assert_eq!(provider, mikmik_core::ProviderId::ANTHROPIC);
                assert!(!login_with_claude_ai);
            }
            other => panic!("expected StartLoginForProvider, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_login_command_codex_flag() {
        let mut ctx = make_ctx();
        let cmd = find_command("login").unwrap();
        let result = cmd.execute("--codex --label work", &mut ctx).await;
        match result {
            CommandResult::StartLoginForProvider {
                provider, label, ..
            } => {
                assert_eq!(provider, mikmik_core::ProviderId::CODEX);
                assert_eq!(label.as_deref(), Some("work"));
            }
            other => panic!("expected StartLoginForProvider, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_accounts_command_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("accounts").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // Should return a Message regardless of registry contents.
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[tokio::test]
    async fn test_switch_command_requires_id() {
        let mut ctx = make_ctx();
        let cmd = find_command("switch").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Error(_)));
    }

    #[tokio::test]
    async fn test_help_command_returns_message() {
        let mut ctx = make_ctx();
        let cmd = find_command("help").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        // help returns either Message or Silent
        assert!(
            matches!(result, CommandResult::Message(_) | CommandResult::Silent),
            "help should return Message or Silent"
        );
    }

    #[tokio::test]
    async fn test_web_setup_proxy_executes_named_command() {
        let mut ctx = make_ctx();
        let cmd = find_command("web-setup").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[tokio::test]
    async fn test_import_config_command_opens_overlay() {
        let mut ctx = make_ctx();
        let cmd = find_command("import-config").unwrap();
        let result = cmd.execute("", &mut ctx).await;
        assert!(matches!(result, CommandResult::OpenImportConfigOverlay));
    }

    #[test]
    fn test_split_command_args_preserves_quoted_segments() {
        assert_eq!(
            split_command_args("create \"agent alpha\" 'second value'"),
            vec![
                "create".to_string(),
                "agent alpha".to_string(),
                "second value".to_string(),
            ]
        );
    }

    // ---- Model selection ----------------------------------------------------

    fn ctx_on(account: &str) -> CommandContext {
        let mut ctx = make_ctx();
        ctx.config.provider = Some(account.to_string());
        ctx.config.provider_configs.insert(
            account.to_string(),
            mikmik_core::config::ProviderConfig::default(),
        );
        ctx
    }

    #[tokio::test]
    async fn a_models_own_namespace_does_not_become_the_account() {
        // `/model meta-llama/Llama-3.3-70B` used to set the provider to
        // `meta-llama`, which names no account, so the credential lookup and
        // the base URL both came back empty.
        let mut ctx = ctx_on("openrouter");

        let result = ModelCommand
            .execute("meta-llama/Llama-3.3-70B", &mut ctx)
            .await;

        let CommandResult::ConfigChangeMessage(config, message) = result else {
            panic!("expected a config change, got {result:?}");
        };
        assert_eq!(config.provider.as_deref(), Some("openrouter"));
        assert_eq!(
            config.model.as_deref(),
            Some("openrouter/meta-llama/Llama-3.3-70B")
        );
        assert_eq!(
            config.effective_route().model,
            "meta-llama/Llama-3.3-70B",
            "the namespace belongs to the model id"
        );
        assert!(message.contains("openrouter"), "{message}");
    }

    #[tokio::test]
    async fn an_account_prefix_moves_the_account() {
        let mut ctx = ctx_on("openrouter");
        ctx.config.provider_configs.insert(
            "my_gateway".to_string(),
            mikmik_core::config::ProviderConfig::default(),
        );

        let result = ModelCommand
            .execute("my_gateway/claude-opus-5", &mut ctx)
            .await;

        let CommandResult::ConfigChangeMessage(config, _) = result else {
            panic!("expected a config change, got {result:?}");
        };
        assert_eq!(config.provider.as_deref(), Some("my_gateway"));
        assert_eq!(config.effective_route().model, "claude-opus-5");
    }

    #[test]
    fn the_fast_model_route_keeps_the_accounts_own_name() {
        // The catalogue is keyed by vendor, so an account the user named
        // matched nothing and the fallback handed back a Claude id. The route
        // still has to reach the account, not the vendor.
        let mut config = mikmik_core::config::Config {
            provider: Some("work_openai".to_string()),
            ..Default::default()
        };
        config.provider_configs.insert(
            "work_openai".to_string(),
            mikmik_core::config::ProviderConfig {
                protocol: Some("openai".to_string()),
                ..Default::default()
            },
        );

        let route = resolve_fast_model_route(&config);
        assert_eq!(route.account, "work_openai");
        assert!(
            !route.model.as_str().contains("claude"),
            "a small OpenAI model was expected, got {}",
            route.model
        );
    }
}
