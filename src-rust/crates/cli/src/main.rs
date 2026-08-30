// mikmik CLI entry point
//
// This is the main binary for MikMik. It:
// 1. Parses CLI arguments with clap (mirrors cli.tsx + main.tsx flags)
// 2. Loads configuration from settings.json + env vars
// 3. Builds system/user context (git status, AGENTS.md)
// 4. Runs in either:
//    - Headless (--print / -p) mode: single query, output to stdout
//    - Interactive REPL mode: full TUI with ratatui

// too_many_arguments: the top-level interactive/headless runners thread many
// parameters; grouping them into structs is a larger refactor out of scope here.
#![allow(clippy::too_many_arguments)]

mod codex_oauth_flow;
mod oauth_flow;
mod status_line;
mod upgrade;
mod workspace_cli;

// ---------------------------------------------------------------------------
// Build-time metadata (embedded via build.rs)
// ---------------------------------------------------------------------------

/// Build timestamp in RFC 3339 format
pub const BUILD_TIME: &str = env!("BUILD_TIME");

/// Short git commit hash (or "unknown" if not a git repo)
pub const GIT_COMMIT: &str = env!("GIT_COMMIT");

/// Package/distribution identifier
pub const PACKAGE_URL: &str = env!("PACKAGE_URL");

/// Feedback/issue reporting channel
pub const FEEDBACK_CHANNEL: &str = env!("FEEDBACK_CHANNEL");

/// Explanation of issue routing in this build
pub const ISSUES_EXPLAINER: &str = env!("ISSUES_EXPLAINER");

use anyhow::Context;
use clap::{ArgAction, Parser, ValueEnum};
use mikmik_api::model_cache::{
    load_cached_model_registry, models_cache_path, models_dev_cache_path, models_source_url,
};
use mikmik_core::{
    config::{Config, PermissionMode, Settings},
    constants::APP_VERSION,
    context::ContextBuilder,
    cost::CostTracker,
    permissions::{AutoPermissionHandler, InteractivePermissionHandler, PermissionManager},
};
use mikmik_tools::ToolContext;
use parking_lot::Mutex as ParkingMutex;
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

/// Name the directories the session can reach, for the system prompt.
///
/// Takes the same inputs as [`mikmik_tools::ToolContext::workspace_roots`], so
/// the names the model is told about are the names path arguments resolve by.
fn roots_for_prompt(
    working_dir: &std::path::Path,
    config: &mikmik_core::config::Config,
) -> std::collections::BTreeMap<String, String> {
    mikmik_core::workspace::generate_root_names(
        working_dir,
        &config.additional_dirs,
        &config.workspace_paths,
    )
    .into_iter()
    .map(|(name, path)| (name, path.display().to_string()))
    .collect()
}

// ---------------------------------------------------------------------------
// CLI argument definition (matches TypeScript main.tsx flags)
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "mikmik",
    version = APP_VERSION,
    about = "MikMik - AI-powered coding assistant",
    long_about = None,
)]
struct Cli {
    /// Initial prompt to send (enables headless/print mode)
    prompt: Option<String>,

    /// Print mode: send prompt and exit (non-interactive)
    #[arg(short = 'p', long = "print", action = ArgAction::SetTrue)]
    print: bool,

    /// Model to use
    #[arg(short = 'm', long = "model")]
    model: Option<String>,

    /// Permission mode
    ///
    /// No default: an absent flag has to leave whatever `permission_mode` the
    /// settings file holds alone, or a mode saved by `/yolo` would be reset to
    /// `default` on every launch.
    #[arg(long = "permission-mode", value_enum)]
    permission_mode: Option<CliPermissionMode>,

    /// Resume a previous session by ID (omit ID to resume the most recent session)
    #[arg(long = "resume", num_args(0..=1), default_missing_value("__last__"))]
    resume: Option<String>,

    /// Maximum number of agentic turns (default 10; `maxTurns` in settings)
    #[arg(long = "max-turns")]
    max_turns: Option<u32>,

    /// Custom system prompt
    #[arg(
        long = "system-prompt",
        short = 's',
        conflicts_with = "system_prompt_file"
    )]
    system_prompt: Option<String>,

    /// Append to system prompt
    #[arg(long = "append-system-prompt")]
    append_system_prompt: Option<String>,

    /// Disable AGENTS.md memory files
    #[arg(long = "no-claude-md", action = ArgAction::SetTrue)]
    no_claude_md: bool,

    /// Output format
    #[arg(long = "output-format", value_enum, default_value_t = CliOutputFormat::Text)]
    output_format: CliOutputFormat,

    /// Enable verbose logging
    #[arg(long = "verbose", short = 'v', action = ArgAction::SetTrue)]
    verbose: bool,

    /// API key for the active provider (overrides provider-specific env vars)
    #[arg(long = "api-key")]
    api_key: Option<String>,

    /// Maximum tokens per response
    #[arg(long = "max-tokens")]
    max_tokens: Option<u32>,

    /// Working directory
    #[arg(long = "cwd")]
    cwd: Option<PathBuf>,

    /// Bypass all permission checks (danger!)
    #[arg(long = "dangerously-skip-permissions", visible_alias = "yolo", action = ArgAction::SetTrue)]
    dangerously_skip_permissions: bool,

    /// Dump the system prompt to stdout and exit
    #[arg(long = "dump-system-prompt", action = ArgAction::SetTrue, hide = true)]
    dump_system_prompt: bool,

    /// MCP config JSON string (inline server definitions)
    #[arg(long = "mcp-config")]
    mcp_config: Option<String>,

    /// Trust and auto-launch project-defined MCP servers (declared in a repo's
    /// .mikmik/settings.json) without prompting. Off by default: such servers
    /// can run arbitrary commands, so opening an untrusted repo would otherwise
    /// require explicit per-server approval. Intended for automation/CI.
    #[arg(long = "trust-project-mcp", action = ArgAction::SetTrue)]
    trust_project_mcp: bool,

    /// Disable auto-compaction
    #[arg(long = "no-auto-compact", action = ArgAction::SetTrue)]
    no_auto_compact: bool,

    /// Enable shadow-git auto-commit snapshots (enables /revert, /checkpoints, /snapshot)
    #[arg(long = "auto-commits", action = ArgAction::SetTrue)]
    auto_commits: bool,

    /// Grant MikMik access to an additional directory (can be repeated)
    #[arg(long = "add-dir", value_name = "DIR", action = ArgAction::Append)]
    add_dir: Vec<PathBuf>,

    /// Input format for --print mode (text or stream-json)
    #[arg(long = "input-format", value_enum, default_value_t = CliInputFormat::Text)]
    input_format: CliInputFormat,

    /// Session ID to tag this headless run (for tracking in logs/hooks)
    #[arg(long = "session-id")]
    session_id_flag: Option<String>,

    /// Prefill the first assistant turn with this text
    #[arg(long = "prefill")]
    prefill: Option<String>,

    /// Effort level for extended thinking (low, medium, high, max)
    #[arg(long = "effort", value_name = "LEVEL")]
    effort: Option<String>,

    /// Extended thinking budget in tokens (enables extended thinking)
    #[arg(long = "thinking", value_name = "TOKENS")]
    thinking: Option<u32>,

    /// Continue the most recent conversation
    #[arg(short = 'c', long = "continue", action = ArgAction::SetTrue)]
    continue_session: bool,

    /// Override system prompt from a file
    #[arg(long = "system-prompt-file")]
    system_prompt_file: Option<PathBuf>,

    /// Offer the model only these tools (comma-separated; default: all)
    #[arg(long = "allowed-tools", value_name = "TOOLS")]
    allowed_tools: Option<String>,

    /// Withhold these tools from the model (comma-separated)
    #[arg(long = "disallowed-tools", value_name = "TOOLS")]
    disallowed_tools: Option<String>,

    /// Extra beta feature headers to send (comma-separated)
    #[arg(long = "betas", value_name = "HEADERS")]
    betas: Option<String>,

    /// Disable all slash commands
    #[arg(long = "disable-slash-commands", action = ArgAction::SetTrue)]
    disable_slash_commands: bool,

    /// Run in bare mode (no hooks, no plugins, no AGENTS.md)
    #[arg(long = "bare", action = ArgAction::SetTrue)]
    bare: bool,

    /// Billing workload tag
    #[arg(long = "workload", value_name = "TAG")]
    workload: Option<String>,

    /// Maximum spend in USD before aborting the query loop
    #[arg(long = "max-budget-usd", value_name = "USD")]
    max_budget_usd: Option<f64>,

    /// Fallback model to use if the primary model is overloaded or unavailable
    #[arg(long = "fallback-model")]
    fallback_model: Option<String>,

    /// LLM provider to use (default: anthropic). Examples: openai, google, ollama
    #[arg(long, env = "MIKMIK_PROVIDER")]
    provider: Option<String>,

    /// Override the API base URL for the selected provider
    #[arg(long, env = "MIKMIK_API_BASE")]
    api_base: Option<String>,

    /// Named agent to use (e.g., build, plan, explore)
    #[arg(long, short = 'A')]
    agent: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum CliPermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
}

impl From<CliPermissionMode> for PermissionMode {
    fn from(m: CliPermissionMode) -> Self {
        match m {
            CliPermissionMode::Default => PermissionMode::Default,
            CliPermissionMode::AcceptEdits => PermissionMode::AcceptEdits,
            CliPermissionMode::BypassPermissions => PermissionMode::BypassPermissions,
            CliPermissionMode::Plan => PermissionMode::Plan,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum CliOutputFormat {
    Text,
    Json,
    #[value(name = "stream-json")]
    StreamJson,
}

impl From<CliOutputFormat> for mikmik_core::config::OutputFormat {
    fn from(f: CliOutputFormat) -> Self {
        match f {
            CliOutputFormat::Text => mikmik_core::config::OutputFormat::Text,
            CliOutputFormat::Json => mikmik_core::config::OutputFormat::Json,
            CliOutputFormat::StreamJson => mikmik_core::config::OutputFormat::StreamJson,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum CliInputFormat {
    /// Plain text prompt (default)
    Text,
    /// Newline-delimited JSON messages — each line is {"role":"user"|"assistant","content":"..."}
    #[value(name = "stream-json")]
    StreamJson,
}

/// How long the startup pull may take before the session goes on without it.
///
/// A server that is up but wedged must not hold the session at the door: the
/// providers and the policy already on disk are what a session opens with when
/// this runs out.
const WORKSPACE_STARTUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// Refresh the organisation's providers and policy, if this installation has a
/// server and asked for the startup pull.
///
/// Answers the connection so the caller can start the background triggers on
/// it without opening a second one.
async fn pull_workspace_at_startup() -> Option<(
    mikmik_core::config::WorkspaceSettings,
    mikmik_core::workspace_server::WorkspaceClient,
)> {
    use mikmik_core::workspace_server::session;

    let settings = Settings::load_sync().ok()?;
    let (workspace, client) = session::connect(&settings)?;
    if workspace.sync.pull_at_startup
        && tokio::time::timeout(WORKSPACE_STARTUP_BUDGET, session::pull_at_startup(&client))
            .await
            .is_err()
    {
        warn!(
            server = %workspace.base(),
            "the workspace server did not answer in time; \
             this session uses the providers and policy already on disk"
        );
    }
    Some((workspace, client))
}

fn resolve_bridge_config(
    settings: &Settings,
    auth_credential: &str,
    use_bearer_auth: bool,
    is_headless: bool,
) -> Option<mikmik_bridge::BridgeConfig> {
    if is_headless {
        return None;
    }

    let mut bridge_config = mikmik_bridge::BridgeConfig::from_env();

    if settings.remote_control_at_startup {
        bridge_config.enabled = true;
    }

    // A self-hosted relay configured in settings.json. Environment variables
    // still win, which keeps a temporary redirect during development a one-line
    // change rather than a settings edit.
    if let Some(remote) = settings.remote_control.as_ref() {
        match remote.validate() {
            Ok(()) => {
                let from_env = std::env::var("MIKMIK_BRIDGE_URL").is_ok()
                    || std::env::var("CLAUDE_BRIDGE_BASE_URL").is_ok();
                if !from_env {
                    bridge_config.server_url = remote.url.trim().trim_end_matches('/').to_string();
                }
                if bridge_config.session_token.is_none() {
                    bridge_config.session_token = Some(remote.token.trim().to_string());
                }
                // Without these the phone lists bare identifiers and the
                // operator cannot tell one machine's session from another's.
                bridge_config.label = remote
                    .label
                    .as_ref()
                    .map(|label| label.trim().to_string())
                    .filter(|label| !label.is_empty());
                bridge_config.cwd = std::env::current_dir()
                    .ok()
                    .map(|dir| dir.display().to_string());
                bridge_config.enabled = true;
            }
            Err(e) => {
                // Refuse rather than fall back to the Anthropic credential
                // against a half-configured relay: this token is what stops an
                // outsider from running tools on this machine.
                eprintln!("Remote control is configured but unusable: {e}. Bridge not started.");
                return None;
            }
        }
    }

    if bridge_config.session_token.is_none() && use_bearer_auth && !auth_credential.is_empty() {
        bridge_config.session_token = Some(auth_credential.to_string());
    }

    bridge_config.is_active().then_some(bridge_config)
}

fn handle_exit_key(
    app: &mut mikmik_tui::app::App,
    key: crossterm::event::KeyEvent,
    cancel: &Option<tokio_util::sync::CancellationToken>,
) -> bool {
    if !key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
    {
        return false;
    }

    match key.code {
        crossterm::event::KeyCode::Char('c') => {
            // Cancel background task first, then let app handle state cleanup
            if app.is_streaming {
                if let Some(ref ct) = cancel {
                    ct.cancel();
                }
            }
            app.handle_key_event(key);
            true
        }
        crossterm::event::KeyCode::Char('d') => {
            app.handle_key_event(key);
            true
        }
        _ => false,
    }
}

/// The permission mode a session starts in.
///
/// `from_settings` is what the settings file already resolved to;
/// `--dangerously-skip-permissions` outranks `--permission-mode`, and either
/// flag outranks the file. Kept apart from `main` so the root block below can
/// be shown to read the resolved mode rather than the flag that produced it.
fn startup_permission_mode(
    from_settings: PermissionMode,
    skip_permissions_flag: bool,
    mode_flag: Option<CliPermissionMode>,
) -> PermissionMode {
    if skip_permissions_flag {
        PermissionMode::BypassPermissions
    } else if let Some(mode) = mode_flag {
        mode.into()
    } else {
        from_settings
    }
}

/// What the session loop owes the permission mode it has just observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BypassGate {
    /// Nothing to do.
    Nothing,
    /// Not bypass: record it, so a later refusal has a mode to go back to.
    RememberMode,
    /// Bypass, and the user has not been warned: show the warning.
    Warn,
}

/// Carry a permission mode the session changed onto the shared manager.
///
/// The running turn decides by `PermissionManager::mode`, which is shared, and
/// by its own `ToolContext` copy, which is not. Syncing only after a key press
/// leaves a mode the model set itself, through `EnterPlanMode`, unseen until
/// the user types something: the turn keeps deciding by the mode it started in.
///
/// Returns whether the mode moved, so a caller can report the switch once.
fn sync_permission_mode(
    manager: Option<&Arc<std::sync::Mutex<PermissionManager>>>,
    observed: &mut PermissionMode,
    desired: PermissionMode,
) -> bool {
    if *observed == desired {
        return false;
    }
    *observed = desired;
    if let Some(manager) = manager {
        if let Ok(mut manager) = manager.lock() {
            manager.mode = desired;
        }
    }
    true
}

/// Decide the gate from the mode alone.
///
/// `shift+tab`, `/yolo on`, `/permissions set bypass-permissions` and the
/// settings file all write `config.permission_mode` and share nothing else, so
/// watching the mode catches every one of them with a single check. Wiring the
/// four call sites separately would leave the next one added unguarded.
fn bypass_gate_for(mode: PermissionMode, gate_cleared: bool, dialog_visible: bool) -> BypassGate {
    if mode != PermissionMode::BypassPermissions {
        return BypassGate::RememberMode;
    }
    if gate_cleared || dialog_visible {
        return BypassGate::Nothing;
    }
    BypassGate::Warn
}

/// Split a comma-separated tool list, dropping the empty pieces.
///
/// A trailing comma and a stray space are both easy to type, and neither names
/// a tool; keeping them would put an entry in the roster filter that can never
/// match and make `--allowed-tools Read,` offer nothing.
fn split_tool_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Build the argument list for a `/restart` relaunch of this binary.
///
/// Keeps every configuration flag the user launched with, but drops the ones
/// that pick which session to open (`--resume`, `-c`/`--continue`,
/// `--session-id`), then appends `--resume <session_id>` so the relaunched
/// process reopens the live session. The first item (argv[0]) is skipped
/// because the caller execs `current_exe()`.
///
/// `/restart` only fires from the interactive TUI, and a launch with a
/// positional prompt runs headless instead (`cli.prompt.is_some()` forces
/// `is_headless`), so the argv here never carries a positional; every bare
/// token is a flag value and is kept.
///
/// `args` is the process argv, typically `std::env::args_os()`.
fn restart_argv(
    args: impl IntoIterator<Item = std::ffi::OsString>,
    session_id: &str,
) -> Vec<std::ffi::OsString> {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut iter = args.into_iter().peekable();
    // Skip argv[0]: the relaunch names the executable through current_exe().
    iter.next();
    while let Some(arg) = iter.next() {
        match arg.to_str() {
            // `--resume` takes an optional value: swallow a following token only
            // when it is not another flag, matching clap's num_args(0..=1).
            Some("--resume") => {
                let next_is_value = iter
                    .peek()
                    .is_some_and(|next| !next.to_string_lossy().starts_with('-'));
                if next_is_value {
                    iter.next();
                }
            }
            // `--session-id` takes a mandatory value: always drop both tokens.
            Some("--session-id") => {
                iter.next();
            }
            Some("-c") | Some("--continue") => {}
            _ => out.push(arg),
        }
    }
    out.push(std::ffi::OsString::from("--resume"));
    out.push(std::ffi::OsString::from(session_id));
    out
}

#[cfg(test)]
mod restart_argv_tests {
    use super::restart_argv;

    fn run(tokens: &[&str], session_id: &str) -> Vec<String> {
        let args = tokens.iter().map(std::ffi::OsString::from);
        restart_argv(args, session_id)
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn keeps_configuration_flags() {
        let out = run(&["mikmik", "--model", "gpt-x", "--yolo"], "sid");
        assert_eq!(out, ["--model", "gpt-x", "--yolo", "--resume", "sid"]);
    }

    #[test]
    fn appends_resume_and_drops_the_old_resume_with_its_id() {
        let out = run(&["mikmik", "--resume", "old", "--model", "m"], "sid");
        assert_eq!(out, ["--model", "m", "--resume", "sid"]);
    }

    #[test]
    fn drops_continue_in_both_forms() {
        assert_eq!(run(&["mikmik", "-c"], "sid"), ["--resume", "sid"]);
        assert_eq!(run(&["mikmik", "--continue"], "sid"), ["--resume", "sid"]);
    }

    #[test]
    fn drops_session_id_with_its_value() {
        let out = run(&["mikmik", "--session-id", "foo", "--fast"], "sid");
        assert_eq!(out, ["--fast", "--resume", "sid"]);
    }

    #[test]
    fn keeps_a_space_separated_flag_value() {
        // An interactive launch has no positional prompt (that forces headless),
        // so a bare token can only be a flag value and must survive.
        let out = run(&["mikmik", "--append-system-prompt", "be terse"], "sid");
        assert_eq!(
            out,
            ["--append-system-prompt", "be terse", "--resume", "sid"]
        );
    }

    #[test]
    fn valueless_resume_at_the_end_does_not_swallow_a_following_flag() {
        // `--resume` with no id, then another flag: the flag must survive.
        let out = run(&["mikmik", "--resume", "--fast"], "sid");
        assert_eq!(out, ["--fast", "--resume", "sid"]);
    }
}

/// Tells the model that the tools it can see are not all the tools there are.
///
/// Added only while `schemaDeferral` is on.
const DEFERRED_SCHEMA_NOTE: &str = "\
## Finding a tool
The tools declared to you are the ones this session starts with, not all of \
them. When a task needs a capability you cannot see (a language server, a \
scheduled job, a notebook cell, an MCP resource, a REPL, a team of agents), \
call `ToolSearch` with a phrase describing it, or `select:ToolName` when you \
know the name. What it finds is declared to you from the next turn on and \
stays for the rest of the session.";

/// Tells the model that GitHub is reached through `gh`, not through a fetch.
///
/// Only added to a run on a machine where `gh` is on the PATH.
const GH_SYSTEM_PROMPT_NOTE: &str = "\
## GitHub
The `gh` CLI is installed on this machine. Reach GitHub through it with Bash: \
`gh pr view`, `gh pr diff`, `gh issue view`, `gh api`. It reads and it writes. \
Do not fetch a github.com page to read a pull request, an issue or a diff; the \
page carries navigation chrome and no comment thread. If `gh` reports an \
authentication error, tell the user to run `gh auth login`.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Fast-path: handle --version before parsing everything
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.iter().any(|a| a == "--version" || a == "-V") {
        println!("mikmik {}", APP_VERSION);
        return Ok(());
    }

    // Relocate any plaintext key in `settings.json` before anything reads a
    // credential, so every path below resolves it from the same place. Runs
    // ahead of the subcommand fast-paths because `auth`, `codex` and
    // `accounts` all read credentials too.
    let moved_keys = mikmik_core::AuthStore::migrate_plaintext_provider_keys();
    if !moved_keys.is_empty() {
        eprintln!(
            "mikmik: moved the API key for {} out of settings.json into auth.json, \
             which is the only credential file locked to your user.",
            moved_keys.join(", ")
        );
    }

    // Fold the old per-provider account registry into the same two files, so
    // every account is one credential plus one providers entry.
    let moved_accounts = mikmik_core::AuthStore::migrate_account_registry();
    if !moved_accounts.is_empty() {
        eprintln!(
            "mikmik: moved {} into auth.json and settings.json. The old accounts.json \
             and accounts/ directory are kept under accounts-backup-<timestamp>/.",
            moved_accounts.join(", ")
        );
    }

    // Fast-path: `claude auth <login|logout|status>` — mirrors TypeScript cli.tsx pattern
    if raw_args.get(1).map(|s| s.as_str()) == Some("auth") {
        return handle_auth_command(&raw_args[2..]).await;
    }

    // Fast-path: `mikmik codex <login|logout|list|switch|remove>` — manage
    // OpenAI Codex (ChatGPT) accounts. Mirrors `mikmik auth` for symmetry.
    if raw_args.get(1).map(|s| s.as_str()) == Some("codex") {
        return handle_codex_account_command(&raw_args[2..]).await;
    }

    // Fast-path: `mikmik workspace <login|logout|status>` — the organisation's
    // configuration server. Ahead of the parser for the same reason as `auth`.
    if raw_args.get(1).map(|s| s.as_str()) == Some("workspace") {
        return workspace_cli::run(&raw_args[2..]).await;
    }

    // Fast-path: `mikmik accounts` — list all stored accounts across providers.
    if raw_args.get(1).map(|s| s.as_str()) == Some("accounts") {
        handle_accounts_command(&raw_args[2..]);
        return Ok(());
    }

    // Fast-path: `mikmik upgrade [--version <v>] [--force]` — self-update.
    if raw_args.get(1).map(|s| s.as_str()) == Some("upgrade") {
        return upgrade::run_upgrade(&raw_args[2..]).await;
    }

    // Fast-path: `claude acp` — start the Agent Client Protocol stdio server.
    if raw_args.get(1).map(|s| s.as_str()) == Some("acp") {
        return mikmik_acp::run_acp_server(Some(acp_login_runner())).await;
    }

    // Fast-path: `mikmik rules [list|test]` — see and try the conditional
    // rules this directory would load.
    if raw_args.get(1).map(|s| s.as_str()) == Some("rules") {
        return run_rules_command(&raw_args[2..]).await;
    }

    // Fast-path: `mikmik models [provider] [--refresh] [--verbose] [--json]`
    //   — list all available providers and models from the bundled snapshot
    //     plus any disk-cached overlay from models.dev.
    if raw_args.get(1).map(|s| s.as_str()) == Some("models") {
        return run_models_command(&raw_args[2..]).await;
    }

    // Fast-path: named commands (`claude agents`, `claude ide`, `claude branch`, …)
    // Check before Cli::parse() so these names don't conflict with positional prompt arg.
    if let Some(cmd_name) = raw_args.get(1).map(|s| s.as_str()) {
        // Only intercept if it looks like a subcommand (no leading `-` or `/`)
        if !cmd_name.starts_with('-') && !cmd_name.starts_with('/') {
            if let Some(named_cmd) = mikmik_commands::named_commands::find_named_command(cmd_name) {
                // Build a minimal CommandContext (named commands are pre-session)
                let settings = Settings::load().await.unwrap_or_default();
                let config = settings.effective_config();
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                // A named command runs before any session, so there is no turn
                // to have measured and no registry loaded to ask. The provider
                // default is the most this path can say.
                let context_window = mikmik_api::ModelRegistry::default_context_window(
                    &config.resolve_route(config.effective_model()).account,
                );
                let cmd_ctx = mikmik_commands::CommandContext {
                    config,
                    context_window,
                    context_used_tokens: 0,
                    cost_tracker: CostTracker::new(),
                    messages: vec![],
                    working_dir: cwd,
                    session_id: "pre-session".to_string(),
                    session_title: None,
                    effort_level: None,
                    remote_session_url: None,
                    mcp_manager: None,
                    mcp_auth_runner: None,
                    // A named subcommand prints and exits; there is no view
                    // for a command to open.
                    interactive: false,
                    // Named subcommands run before any session, so no agent.
                    active_agent: None,
                };
                // Collect remaining args after the command name
                let rest: Vec<&str> = raw_args[2..].iter().map(|s| s.as_str()).collect();
                let result = named_cmd.execute_named(&rest, &cmd_ctx);
                match result {
                    mikmik_commands::CommandResult::Message(msg)
                    | mikmik_commands::CommandResult::UserMessage(msg) => {
                        println!("{}", msg);
                        std::process::exit(0);
                    }
                    mikmik_commands::CommandResult::Error(e) => {
                        eprintln!("Error: {}", e);
                        eprintln!("Usage: {}", named_cmd.usage());
                        std::process::exit(1);
                    }
                    _ => {
                        // For any other result variant, fall through to normal startup
                    }
                }
                return Ok(());
            }
        }
    }

    let cli = Cli::parse();

    // Setup logging
    let log_level = if cli.verbose { "debug" } else { "warn" };
    let base_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));
    let log_filter = base_filter
        .add_directive(
            "rmcp::service::client=error"
                .parse()
                .expect("valid rmcp directive"),
        )
        // Suppress error/warn logs from providers and query — errors are already shown as error modals
        .add_directive(
            "mikmik_api::providers::free=off"
                .parse()
                .expect("valid directive"),
        )
        .add_directive("mikmik_query=off".parse().expect("valid directive"));
    tracing_subscriber::fmt()
        .with_env_filter(log_filter)
        .with_target(false)
        .without_time()
        .init();

    // Determine working directory
    let cwd = cli
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    debug!(cwd = %cwd.display(), "Starting MikMik");

    // Determine mode early (needed for settings error reporting, auth error
    // handling, and permission handler selection).
    let is_headless = cli.print || cli.prompt.is_some();

    // Take the organisation's providers and policy before the settings are
    // read, so this session runs on what the server says now rather than on
    // what it said last time. Both are written to disk, which is what the load
    // below picks up.
    //
    // Bounded, because a server that is up but wedged must not hold the
    // session at the door. Whatever is already on disk is applied instead.
    let workspace_session = pull_workspace_at_startup().await;

    // Load settings from disk (hierarchical: global < project). A malformed
    // global file is kept intact; interactive mode displays the error in the
    // startup dialog, while headless mode reports it on stderr.
    let (mut settings, project_overlay, settings_load_error) =
        match Settings::load_hierarchical_detailed(&cwd).await {
            Ok((settings, overlay)) => (settings, overlay, None),
            Err(error) => {
                let message = error.to_string();
                if is_headless {
                    eprintln!("Warning: {}", message);
                }
                (Settings::default(), None, Some(message))
            }
        };
    // Keep the settings backup current while the session runs. Headless runs
    // are excluded: a `--print` call is over in seconds, and a loop that polls
    // a file for a change would never reach its first upload.
    if let Some((workspace, client)) = workspace_session {
        if !is_headless && (workspace.sync.on_change || workspace.sync.interval_minutes.is_some()) {
            let cancel = mikmik_core::workspace_server::session::Cancel::new();
            tokio::spawn(mikmik_core::workspace_server::session::run_triggers(
                client, workspace, cancel,
            ));
        }
    }

    // `--trust-project-mcp` (and automation use cases) flip on the same global
    // trust the user could set via `trustProjectMcpServers`. Folding it into
    // `settings` here keeps a single source of truth for the gate, including
    // for the interactive reconnect path.
    if cli.trust_project_mcp {
        settings.trust_project_mcp_servers = true;
    }

    // Build effective config (CLI args override settings)
    let mut config = settings.effective_config();
    if let Some(ref key) = cli.api_key {
        config.api_key = Some(key.clone());
    }
    if let Some(ref m) = cli.model {
        config.model = Some(m.clone());
    }
    if let Some(mt) = cli.max_tokens {
        config.max_tokens = Some(mt);
    }
    config.verbose = cli.verbose;
    config.output_format = cli.output_format.into();
    // --bare implies --no-claude-md: opening an untrusted repo in bare mode
    // must not load or inject AGENTS.md memory files.
    config.disable_claude_mds = cli.no_claude_md || cli.bare;
    if cli.bare {
        // Bare mode runs no event hooks. Drop any hooks resolved from
        // settings so no `run_hooks` call site has anything to execute.
        config.hooks.clear();
    }
    if let Some(sp) = cli.system_prompt.clone() {
        config.custom_system_prompt = Some(sp);
    }
    if let Some(ref path) = cli.system_prompt_file {
        // Fail rather than fall back to the built-in prompt: a run that
        // silently ignores the requested prompt produces work the user did not
        // ask for.
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read --system-prompt-file {}", path.display()))?;
        config.custom_system_prompt = Some(contents);
    }
    if let Some(asp) = cli.append_system_prompt.clone() {
        config.append_system_prompt = Some(asp);
    }
    // These two name which tools the roster offers, not which calls are
    // approved; a call is decided against `permission_rules`. The flag replaces
    // the settings value rather than adding to it, because a run that asks for
    // exactly these tools means exactly these.
    if let Some(ref names) = cli.allowed_tools {
        config.allowed_tools = split_tool_names(names);
    }
    if let Some(ref names) = cli.disallowed_tools {
        config.disallowed_tools = split_tool_names(names);
    }
    config.permission_mode = startup_permission_mode(
        config.permission_mode,
        cli.dangerously_skip_permissions,
        cli.permission_mode,
    );
    config.additional_dirs = cli.add_dir.clone();
    if cli.no_auto_compact {
        config.auto_compact = Some(false);
    }
    if cli.auto_commits {
        config.auto_commits = Some(true);
    }
    config.project_dir = Some(cwd.clone());
    if let Some(p) = &cli.provider {
        config.provider = Some(p.clone());
    }
    if let Some(base) = &cli.api_base {
        // Store in the provider's config entry
        let provider_id = config
            .provider
            .clone()
            .unwrap_or_else(|| "anthropic".to_string());
        config
            .provider_configs
            .entry(provider_id)
            .or_default()
            .api_base = Some(base.clone());
    }

    // Build context
    let ctx_builder = ContextBuilder::new(cwd.clone())
        .disable_claude_mds(config.disable_claude_mds)
        .memory_filenames(mikmik_core::agentsmd::MemoryFilenames::from_config(&config));
    let system_ctx = ctx_builder.build_system_context().await;
    let user_ctx = ctx_builder.build_user_context().await;
    let loaded_instructions = !system_ctx.trim().is_empty() || !user_ctx.trim().is_empty();

    // Build system prompt
    let mut system_parts = vec![
        include_str!("system_prompt.txt").to_string(),
        system_ctx,
        user_ctx,
    ];
    if let Some(ref custom) = config.custom_system_prompt {
        // replace base system prompt
        system_parts[0] = custom.clone();
    } else if which::which("gh").is_ok() {
        // Held out of `system_prompt.txt` because that file is a static
        // `include_str!` and this is only true on a machine that has `gh`.
        // Telling a model to run a missing binary costs it a turn on the
        // failure, which is why `PowerShellTool` also stays out of the roster
        // where `pwsh` is absent. Nothing else names GitHub, so without this
        // the model reaches for WebFetch and reads a rendered page.
        //
        // Skipped when `--system-prompt` replaced the base, because that flag
        // means the caller supplies the guidance.
        //
        // `gh auth status` is not run: it costs a subprocess and a network
        // call at startup, and it would make the prompt depend on a network
        // condition. `gh` reports its own auth failure well enough.
        system_parts.insert(1, GH_SYSTEM_PROMPT_NOTE.to_string());
    }
    // Only while the setting is on: with it off every tool is declared and a
    // note telling the model to search for one would send it looking for
    // something already in front of it.
    if config.schema_deferral {
        system_parts.push(DEFERRED_SCHEMA_NOTE.to_string());
    }
    if let Some(ref append) = config.append_system_prompt {
        system_parts.push(append.clone());
    }
    let system_prompt = system_parts.join("\n\n");

    // --dump-system-prompt: print exactly what a run would send.
    //
    // The string assembled above is only the *custom* part; the core builder
    // wraps it with the capabilities, tool-use guidelines, output style and
    // safety sections. Rendering goes through the same `build_system_prompt`
    // the query loop calls, so this output cannot drift from the real one.
    //
    // The tool list is built without MCP so the dump stays a side-effect-free
    // fast path. That does not change the output: only the built-in tools in
    // `GUIDELINE_TOOLS` contribute per-tool guidance, and MCP tools are never
    // in that set.
    if cli.dump_system_prompt {
        let model_registry = load_cached_model_registry(&config);
        let mut dump_config =
            mikmik_query::QueryConfig::from_config_with_registry(&config, &model_registry);
        dump_config.system_prompt = Some(system_prompt);
        dump_config.append_system_prompt = None;
        dump_config.working_directory = Some(cwd.display().to_string());
        dump_config.workspace_roots = roots_for_prompt(&cwd, &config);
        dump_config.enabled_tools = Some(
            mikmik_query::build_tool_roster(None, &config, &cwd)
                .iter()
                .map(|tool| tool.name().to_string())
                .collect(),
        );
        // The REPL sets this per turn from the companion it loaded at startup.
        // The dump has no REPL, so it reads the same files itself — otherwise
        // this output silently omits a block a real run sends.
        if config.companion.as_ref().is_some_and(|c| c.enabled) {
            let identity = mikmik_core::accounts::stable_identity();
            let companion = mikmik_buddy::get_companion(&identity, &mikmik_core::mikmik_home());
            dump_config.companion_addendum = mikmik_buddy::intro_for(&companion);
        }

        match mikmik_query::build_system_prompt(&dump_config) {
            mikmik_api::SystemPrompt::Text(text) => println!("{text}"),
            mikmik_api::SystemPrompt::Blocks(blocks) => {
                for block in blocks {
                    println!("{}", block.text);
                }
            }
        }
        return Ok(());
    }

    // Initialize API client.
    // Try config/env first; fall back to saved OAuth tokens.
    // If no Anthropic credentials are found, check whether any other provider is
    // configured (OpenAI, Google, Ollama, Groq, etc.) — if so, proceed without
    // requiring Anthropic auth. Only launch the OAuth flow when Anthropic is
    // explicitly the intended provider and no key exists at all.
    let active_provider = config.selected_provider_id();
    let (api_key, use_bearer_auth) = if active_provider == "anthropic" {
        match config.resolve_anthropic_auth_async().await {
            Some(auth) => auth,
            None => {
                if is_headless {
                    anyhow::bail!(
                        "No API key found. Options:\n\
                         - Set ANTHROPIC_API_KEY for Anthropic\n\
                         - Set OPENAI_API_KEY for OpenAI\n\
                         - Set GOOGLE_API_KEY for Google Gemini\n\
                         - Set GROQ_API_KEY for Groq (fast, free tier available)\n\
                         - Run `mikmik --provider ollama` for local models (no key needed)\n\
                         - Run `mikmik auth login` for Anthropic OAuth"
                    );
                } else {
                    (String::new(), false)
                }
            }
        }
    } else {
        (String::new(), false)
    };

    // Apply the user-configured request timeout (issue #175) before building any
    // client so the Anthropic client and all providers honour it.
    mikmik_api::set_request_timeout_secs(config.resolve_request_timeout_secs_active());
    let client_config = mikmik_api::client::ClientConfig {
        api_key: api_key.clone(),
        api_base: config.resolve_anthropic_api_base(),
        use_bearer_auth,
        ..Default::default()
    };
    let client = Arc::new(
        mikmik_api::AnthropicClient::new(client_config.clone())
            .context("Failed to create API client")?,
    );

    // Build provider registry: auto-registers all env-configured providers
    // AND providers with keys stored in ~/.config/mikmik/auth.json (from /connect).
    // Anthropic is always the default; additional providers (OpenAI, Google,
    // Bedrock, Azure, Copilot, Cohere, local providers) are registered when
    // their respective environment variables or auth store entries are found.
    let provider_registry = mikmik_api::ProviderRegistry::from_config(&config, client_config);

    let bridge_config = resolve_bridge_config(&settings, &api_key, use_bearer_auth, is_headless);
    if let Some(cfg) = bridge_config.as_ref() {
        info!(
            server_url = %cfg.server_url,
            startup_enabled = settings.remote_control_at_startup,
            "Remote control bridge configured for interactive startup"
        );
    }

    let permission_manager = Arc::new(std::sync::Mutex::new(PermissionManager::new(
        config.permission_mode,
        &settings,
    )));

    let permission_handler: Arc<dyn mikmik_core::PermissionHandler> = if is_headless {
        Arc::new(AutoPermissionHandler::with_manager(
            permission_manager.clone(),
        ))
    } else {
        Arc::new(InteractivePermissionHandler::with_manager(
            permission_manager.clone(),
        ))
    };
    let cost_tracker = CostTracker::new();
    // Use --session-id if provided, otherwise generate a fresh UUID.
    let session_id = cli
        .session_id_flag
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let file_history = Arc::new(ParkingMutex::new(
        mikmik_core::file_history::FileHistory::new(),
    ));
    // One store for the session, so a read in one turn still authorises an
    // edit in the next. See `mikmik_tools::edit_guard`.
    let file_snapshots = Arc::new(ParkingMutex::new(
        mikmik_core::file_snapshot::FileSnapshotStore::new(),
    ));
    let current_turn = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Load plugins and register any plugin-provided MCP servers into the
    // in-memory config (does not modify the settings file on disk). This runs
    // before the MCP runtime is built so a plugin server connects at startup
    // rather than only after a manual reconnect, and so a project-scoped one
    // passes the same trust gate as a server declared in the repository's
    // settings file.
    // Bare mode skips plugin discovery entirely and uses an empty registry so
    // no plugin commands, hooks, or MCP servers are loaded from an untrusted
    // repo. Downstream code still works against the empty registry.
    let plugin_registry = if cli.bare {
        mikmik_plugins::PluginRegistry::new()
    } else {
        mikmik_plugins::load_plugins(&cwd, &[]).await
    };
    {
        let plugin_cmd_count = plugin_registry.all_command_defs().len();
        let hook_registry = plugin_registry.build_hook_registry();
        let plugin_hook_count = hook_registry.values().map(|v| v.len()).sum::<usize>();
        info!(
            plugins = plugin_registry.enabled_count(),
            commands = plugin_cmd_count,
            hooks = plugin_hook_count,
            "Plugins loaded"
        );

        // The tool loop reads its plugin hooks from this static; without it
        // every hook a plugin declares is parsed and then never runs.
        mikmik_plugins::set_global_hooks(hook_registry);

        apply_plugin_contributions(&plugin_registry, None, &mut config);
    }

    // Publish the registry: sub-agent prompts read the plugins' `agents/`
    // definitions from here.
    mikmik_plugins::set_global_registry(plugin_registry);

    // Setup, then SessionStart: a plugin gets one chance to prepare itself
    // before the session tells it a session is under way.
    mikmik_plugins::run_global_hook(
        mikmik_plugins::HookEventKind::Setup,
        None,
        serde_json::json!({ "working_dir": cwd.display().to_string() }),
    )
    .await;
    mikmik_plugins::run_global_hook(
        mikmik_plugins::HookEventKind::SessionStart,
        None,
        serde_json::json!({
            "working_dir": cwd.display().to_string(),
            "session_id": session_id,
        }),
    )
    .await;
    mikmik_plugins::run_global_hook(
        mikmik_plugins::HookEventKind::InstructionsLoaded,
        None,
        serde_json::json!({
            "working_dir": cwd.display().to_string(),
            "has_instructions": loaded_instructions,
        }),
    )
    .await;

    // Initialize MCP servers first (needed for ToolContext.mcp_manager).
    //
    // SECURITY (issue #123): project-defined MCP servers (from a repo's
    // .mikmik/settings.json) can run arbitrary commands. Gate them behind
    // explicit trust so opening a cloned repo never auto-spawns attacker
    // processes. User/global servers are unaffected. The untrusted project
    // servers are surfaced to the TUI for an approval prompt, or skipped (with
    // a notice) in headless mode unless trust was granted.
    let mcp_project_root = mikmik_core::mcp_trust::project_root_for(&cwd);
    let mcp_decision = {
        let store = mikmik_core::mcp_trust::McpTrustStore::load();
        mikmik_core::mcp_trust::partition_mcp_servers(
            &config.mcp_servers,
            mcp_project_root.as_deref(),
            settings.trust_project_mcp_servers,
            &std::collections::HashSet::new(),
            &store,
        )
    };
    let pending_project_mcp = mcp_decision.pending.clone();
    if !pending_project_mcp.is_empty() {
        let names: Vec<&str> = pending_project_mcp
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        if is_headless {
            warn!(
                servers = ?names,
                "Skipping project-defined MCP server(s) pending approval. \
                 Approve them in the interactive TUI, or pass --trust-project-mcp \
                 (or set trustProjectMcpServers) to launch them in headless mode."
            );
        } else {
            info!(
                servers = ?names,
                "Project-defined MCP server(s) require approval before launching."
            );
        }
    }
    // SECURITY (issue #389): the same reasoning, for the rest of the project
    // settings file. Hooks, formatters, language servers and skill sources all
    // name something to run or fetch, so they wait for the user to see them and
    // agree. The keys a repository may never set were already dropped by the
    // merge; they are reported here so the file does not fail silently.
    let project_trust_root = project_overlay.as_ref().and_then(|o| o.root.clone());
    let project_trust_pending = project_overlay.as_ref().and_then(|overlay| {
        (!overlay.approved && !overlay.gated.is_empty()).then(|| overlay.gated.clone())
    });
    if let Some(overlay) = project_overlay.as_ref() {
        if !overlay.refused.is_empty() {
            warn!(
                keys = ?overlay.refused,
                "Ignoring project settings keys that only your own settings may set."
            );
        }
    }
    if is_headless {
        if let Some(pending) = project_trust_pending.as_ref() {
            warn!(
                declares = ?pending.describe(),
                "Skipping what this project's settings file wants to run: \
                 approving it needs the interactive TUI."
            );
        }
    }

    let mcp_manager_arc = connect_mcp_manager_arc(&mcp_decision.allowed).await;

    let pending_permissions = Arc::new(ParkingMutex::new(
        mikmik_tools::PendingPermissionStore::default(),
    ));

    let is_non_interactive = cli.print || cli.prompt.is_some();

    // Side-channel for the AskUserQuestion tool to send questions to the TUI.
    // Only created in interactive mode; None in headless/print mode.
    let (user_question_tx, user_question_rx) =
        tokio::sync::mpsc::unbounded_channel::<mikmik_tools::UserQuestionEvent>();
    let user_question_rx = if is_non_interactive {
        None
    } else {
        Some(user_question_rx)
    };

    // The same arrangement for ExitPlanMode, which blocks on the user's
    // decision about a plan.
    let (plan_approval_tx, plan_approval_rx) =
        tokio::sync::mpsc::unbounded_channel::<mikmik_tools::PlanApprovalEvent>();
    let plan_approval_rx = if is_non_interactive {
        None
    } else {
        Some(plan_approval_rx)
    };

    // And for a tool's output while it is still running. Unlike the two above
    // this one never blocks anything: a chunk nobody drains is just dropped.
    let (tool_output_tx, tool_output_rx) =
        tokio::sync::mpsc::unbounded_channel::<mikmik_tools::ToolOutputChunk>();
    let tool_output_rx = if is_non_interactive {
        None
    } else {
        Some(tool_output_rx)
    };

    // And for EnterPlanMode, which changes the session's mode rather than
    // asking about it, so it needs no reply either.
    let (plan_mode_tx, plan_mode_rx) =
        tokio::sync::mpsc::unbounded_channel::<mikmik_tools::EnterPlanModeEvent>();
    let plan_mode_rx = if is_non_interactive {
        None
    } else {
        Some(plan_mode_rx)
    };

    let tool_ctx = ToolContext {
        working_dir: cwd.clone(),
        permission_mode: config.permission_mode,
        permission_handler: permission_handler.clone(),
        cost_tracker: cost_tracker.clone(),
        session_id: session_id.clone(),
        file_history: file_history.clone(),
        file_snapshots: file_snapshots.clone(),
        current_turn: current_turn.clone(),
        non_interactive: is_non_interactive,
        mcp_manager: mcp_manager_arc.clone(),
        config: config.clone(),
        managed_agent_config: config.managed_agents.clone(),
        completion_notifier: None,
        pending_permissions: Some(pending_permissions.clone()),
        permission_manager: Some(permission_manager.clone()),
        user_question_tx: if is_non_interactive {
            None
        } else {
            Some(user_question_tx)
        },
        plan_approval_tx: if is_non_interactive {
            None
        } else {
            Some(plan_approval_tx)
        },
        tool_output_tx: if is_non_interactive {
            None
        } else {
            Some(tool_output_tx)
        },
        plan_mode_tx: if is_non_interactive {
            None
        } else {
            Some(plan_mode_tx)
        },
        // A primary agent has nobody to advise. `run_query_loop` sets these on
        // the watcher's own context, where the `Advise` tool lives.
        advisor_note_tx: None,
        advisor_name: None,
        // Placeholder token; `run_query_loop` rebinds it to the loop's actual
        // cancel token so the parallel tool executor honours Ctrl-C (issue #218).
        cancel_token: tokio_util::sync::CancellationToken::new(),
        // Filled in per call by the tool dispatcher.
        current_call: None,
        editor: None,
        inbox: Default::default(),
    };

    // Hourly shadow-snapshot GC loop: only runs when snapshot is explicitly enabled.
    if config.auto_commits == Some(true) {
        let gc_dir = cwd.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            loop {
                if let Some(snap) = mikmik_core::snapshot::get_or_create(&gc_dir) {
                    snap.cleanup().await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

    // Register the cc-query-backed agent runner so TeamCreateTool can spawn real
    // sub-agents.  Must be called before any tool execution begins.
    // The function is idempotent if already registered (panics only on double-call,
    // but we guard with a std::sync::OnceLock internally).
    {
        static SWARM_INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        SWARM_INIT.get_or_init(mikmik_query::init_team_swarm_runner);
    }

    // Create the project's memory directory up front. The model writes a
    // memory with the ordinary Write tool, and that tool refuses a path whose
    // parent does not exist, so the first memory of a session would fail
    // without this.
    if mikmik_core::memdir::is_auto_memory_enabled(config.auto_memory_enabled) {
        let project_root = mikmik_core::session_storage::transcript_root_for(&cwd);
        mikmik_core::memdir::ensure_memory_dir_exists(&mikmik_core::memdir::auto_memory_path(
            &project_root,
        ));
    }

    // Build the full tool list: built-ins from cc-tools plus AgentTool from cc-query
    // (AgentTool lives in cc-query to avoid a circular cc-tools ↔ cc-query dependency).
    // Wrap in Arc so the list can be shared by the main loop AND the cron scheduler.
    let tools = mikmik_query::build_tool_roster(mcp_manager_arc.clone(), &config, &cwd);

    // Build model registry for dynamic model/provider resolution.
    // The registry is pre-populated with a hardcoded snapshot and enriched
    // from the models.dev cache if available.
    let model_registry = load_cached_model_registry(&config);

    // Build query config
    let mut query_config =
        mikmik_query::QueryConfig::from_config_with_registry(&config, &model_registry);
    query_config.model_registry = Some(model_registry.clone());
    // The flag overrides the `maxTurns` setting that `from_config` already
    // read; unset leaves the setting in place.
    if let Some(max_turns) = cli.max_turns {
        query_config.max_turns = max_turns;
    }
    query_config.system_prompt = Some(system_prompt);
    query_config.append_system_prompt = None;
    query_config.working_directory = Some(cwd.display().to_string());
    query_config.workspace_roots = roots_for_prompt(&cwd, &config);
    if let Some(tokens) = cli.thinking {
        query_config.thinking_budget = Some(tokens);
    }
    if let Some(ref level_str) = cli.effort {
        if let Some(level) = mikmik_core::effort::EffortLevel::from_str(level_str) {
            query_config.effort_level = Some(level);
        } else {
            eprintln!(
                "Warning: unknown effort level '{}' — expected low/medium/high/max",
                level_str
            );
        }
    }
    if let Some(usd) = cli.max_budget_usd {
        query_config.max_budget_usd = Some(usd);
    }
    if let Some(ref fb) = cli.fallback_model {
        query_config.fallback_model = Some(fb.clone());
    }
    // Wire in the provider registry so non-Anthropic providers can be dispatched.
    let provider_registry = std::sync::Arc::new(provider_registry);
    query_config.provider_registry = Some(provider_registry.clone());

    // Wire in the named agent (--agent flag).
    // Built-in defaults, settings.json agents and folder agents, folder-most-wins.
    let tools = if let Some(ref agent_name) = cli.agent {
        query_config.agent_name = Some(agent_name.clone());
        let all_agents = mikmik_core::resolve_agents(&cwd, &config.agents);
        if let Some(def) = all_agents.get(agent_name) {
            let access = def.access.clone();
            query_config.agent_definition = Some(def.clone());
            // Override max_turns from agent definition when specified.
            if let Some(turns) = def.max_turns {
                query_config.max_turns = turns;
            }
            filter_tools_for_agent(tools, &access)
        } else {
            eprintln!(
                "Warning: unknown agent '{}'. Run /agent to see available agents.",
                agent_name
            );
            tools
        }
    } else {
        tools
    };

    // Spawn the background cron scheduler (fires cron tasks at scheduled times).
    // Cancelled automatically when the process exits since we use a shared token.
    let cron_cancel = tokio_util::sync::CancellationToken::new();
    mikmik_query::start_cron_scheduler(
        client.clone(),
        tools.clone(),
        tool_ctx.clone(),
        query_config.clone(),
        cron_cancel.clone(),
    );

    // `-c` is `--resume` with no id: continue the most recent conversation.
    // Resolved once, before the modes split, so both read the same request.
    let resume_request = cli
        .resume
        .clone()
        .or_else(|| cli.continue_session.then(|| "__last__".to_string()));

    // --print mode (headless)
    let result = if is_headless {
        run_headless(
            &cli,
            client,
            tools,
            tool_ctx,
            query_config,
            cost_tracker,
            resume_request,
        )
        .await
    } else {
        let auth_store = mikmik_core::AuthStore::load();
        let has_saved_credentials = !auth_store.credentials.is_empty()
            || mikmik_core::oauth_config::get_codex_tokens().is_some();
        let has_credentials = !api_key.is_empty()
            || has_saved_credentials
            || config.provider.as_deref().is_some_and(|p| p != "anthropic");
        run_interactive(
            config,
            settings,
            settings_load_error,
            client,
            tools,
            tool_ctx,
            query_config,
            cost_tracker,
            resume_request,
            bridge_config,
            has_credentials,
            model_registry,
            user_question_rx,
            plan_approval_rx,
            tool_output_rx,
            plan_mode_rx,
            pending_project_mcp,
            mcp_project_root,
            project_trust_pending,
            project_trust_root,
        )
        .await
    };

    cron_cancel.cancel();
    result
}

/// The names of the MCP servers a registry contributes, sorted, so two
/// registries can be compared for "did the plugin servers move".
fn plugin_mcp_names(registry: &mikmik_plugins::PluginRegistry) -> Vec<String> {
    let mut names: Vec<String> = registry
        .all_mcp_servers()
        .into_iter()
        .map(|s| s.name)
        .collect();
    names.sort();
    names
}

/// Merge everything the plugins contribute into a `Config`.
///
/// Each contribution has its own consumer: MCP servers and language servers
/// are read from the config, skills join the discovery search path, and output
/// styles go into a process-wide registry that the style lookup consults.
/// Called at startup and again on a reload, so a config that already carries a
/// contribution keeps exactly one copy.
///
/// `previous` is the registry this one replaces. What that registry
/// contributed and this one no longer does is dropped from the config, which is
/// how a disabled or deleted plugin stops providing an MCP server, a language
/// server or a skill directory without a restart. Matching is by name, so an
/// entry that also comes from the settings file survives: the current registry
/// still names it.
fn apply_plugin_contributions(
    registry: &mikmik_plugins::PluginRegistry,
    previous: Option<&mikmik_plugins::PluginRegistry>,
    config: &mut mikmik_core::Config,
) {
    if let Some(previous) = previous {
        let current_mcp: std::collections::HashSet<String> = registry
            .all_mcp_servers()
            .into_iter()
            .map(|s| s.name)
            .collect();
        let stale_mcp: std::collections::HashSet<String> = previous
            .all_mcp_servers()
            .into_iter()
            .map(|s| s.name)
            .filter(|name| !current_mcp.contains(name))
            .collect();
        config.mcp_servers.retain(|s| !stale_mcp.contains(&s.name));

        let current_lsp: std::collections::HashSet<String> = registry
            .all_lsp_servers()
            .into_iter()
            .map(|s| s.name)
            .collect();
        let stale_lsp: std::collections::HashSet<String> = previous
            .all_lsp_servers()
            .into_iter()
            .map(|s| s.name)
            .filter(|name| !current_lsp.contains(name))
            .collect();
        config.lsp_servers.retain(|s| !stale_lsp.contains(&s.name));

        let current_skills: std::collections::HashSet<String> = registry
            .all_skill_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let stale_skills: std::collections::HashSet<String> = previous
            .all_skill_paths()
            .iter()
            .map(|p| p.display().to_string())
            .filter(|path| !current_skills.contains(path))
            .collect();
        config.skills.paths.retain(|p| !stale_skills.contains(p));
    }

    let existing_names: std::collections::HashSet<String> =
        config.mcp_servers.iter().map(|s| s.name.clone()).collect();
    for mcp_server in registry.all_mcp_servers() {
        if !existing_names.contains(&mcp_server.name) {
            config.mcp_servers.push(mcp_server);
        }
    }

    // Feed the plugins' `skills/` directories into skill discovery, which is
    // the only route by which a skill can be listed and then run.
    for skill_dir in registry.all_skill_paths() {
        let path = skill_dir.display().to_string();
        if !config.skills.paths.contains(&path) {
            config.skills.paths.push(path);
        }
    }

    // Same for language servers: the LSP tool seeds its manager from the
    // config, so a plugin's server has to be in there to ever start.
    let existing_lsp: std::collections::HashSet<String> =
        config.lsp_servers.iter().map(|s| s.name.clone()).collect();
    for lsp_server in registry.all_lsp_servers() {
        if !existing_lsp.contains(&lsp_server.name) {
            config.lsp_servers.push(lsp_server);
        }
    }

    // Output styles live in a runtime registry rather than the config, because
    // a style is chosen by name and resolved on demand. Registering the same
    // name twice is ignored there.
    for style_dir in registry.all_output_style_paths() {
        for style in mikmik_core::output_styles::load_output_styles_dir(&style_dir) {
            mikmik_core::output_styles::register_runtime_style(style);
        }
    }
}

/// Slash commands that exist only because of this session's plugins and
/// skills, as `(name, description)` pairs for the TUI's command lists.
///
/// Without this the typeahead, the palette and the help overlay show the
/// built-in table alone, so a plugin command is reachable only by someone who
/// already knows its name.
/// Returns the commands alongside how many of them came from skill discovery.
/// The merged list cannot answer that afterwards, and discovery walks the
/// filesystem, so counting here avoids a second walk.
fn session_slash_commands(
    cwd: &std::path::Path,
    config: &mikmik_core::Config,
) -> (Vec<(String, String)>, usize) {
    let mut commands: Vec<(String, String)> = Vec::new();

    if let Some(registry) = mikmik_plugins::global_plugin_registry() {
        for def in registry.all_command_defs() {
            commands.push((def.name.clone(), def.description.clone()));
        }
    }

    let mut skill_count = 0;
    for resolved in mikmik_core::discover_skills(cwd, &config.skills) {
        commands.push((resolved.command_name.clone(), resolved.tagged_description()));
        skill_count += 1;
    }

    commands.sort();
    commands.dedup_by(|a, b| a.0 == b.0);
    (commands, skill_count)
}

async fn connect_mcp_manager_arc(
    servers: &[mikmik_core::config::McpServerConfig],
) -> Option<Arc<mikmik_mcp::McpManager>> {
    if servers.is_empty() {
        return None;
    }

    info!(count = servers.len(), "Connecting to MCP servers");
    let mcp_manager = Arc::new(mikmik_mcp::McpManager::connect_all(servers).await);
    mcp_manager.clone().spawn_notification_poll_loop();
    Some(mcp_manager)
}

/// The `rules` subcommand: `list` and `test`.
///
/// A rule that never fires and a rule that fires on everything look the same
/// from the outside: silence, or noise. This runs the real matcher so the
/// author can see which it is before shipping the file.
async fn run_rules_command(args: &[String]) -> anyhow::Result<()> {
    const USAGE: &str = "Usage:\n  \
        mikmik rules list\n  \
        mikmik rules test <tool> <file> [text]\n  \
        mikmik rules test text|thinking [prose]\n  \
        mikmik rules extract [--write] [name...]\n\n\
        `test` reads the text from stdin when it is not given on the command line.\n\
        Example: mikmik rules test Edit src/a.rs 'let x = y.unwrap();'\n\
        Example: mikmik rules test text 'I will just cast it to any'\n\
        Example: mikmik rules extract --write never-use-unwrap-outside-tests";

    let settings = Settings::load().await.unwrap_or_default();
    let config = settings.effective_config();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_root = mikmik_core::session_storage::transcript_root_for(&cwd);
    let rules = mikmik_core::rules::rules_for(
        &project_root,
        mikmik_core::agentsmd::MemoryFilenames::from_config(&config),
        config.effective_rules_builtin(),
        &config.rules_disabled,
    );

    match args.first().map(|s| s.as_str()) {
        None | Some("list") => {
            if rules.is_empty() {
                println!("No conditional rules load here.");
                println!(
                    "Write one in {}/.mikmik/rules/, or see docs/configuration.md.",
                    project_root.display()
                );
                return Ok(());
            }
            println!(
                "{} rule(s), from {}:\n",
                rules.len(),
                project_root.display()
            );
            for rule in rules.iter() {
                // A rule on prose has no tool result to ride on, so it always
                // stops the turn. Saying "remind" here would hide that.
                let action = if rule.scope.text || rule.scope.thinking {
                    "interrupt"
                } else {
                    match rule.action {
                        mikmik_core::rules::RuleAction::Block => "block",
                        mikmik_core::rules::RuleAction::Remind => "remind",
                    }
                };
                println!("  {}  [{action}]", rule.name);
                if let Some(description) = &rule.description {
                    println!("      {description}");
                }
                println!("      from {}", rule.path.display());
            }
        }
        Some("test") => {
            let Some(tool) = args.get(1) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            // Prose takes no file, so its text is one argument earlier.
            let prose = match tool.as_str() {
                "text" => Some(mikmik_core::rules::ProseStream::Text),
                "thinking" => Some(mikmik_core::rules::ProseStream::Thinking),
                _ => None,
            };
            let text_arg = if prose.is_some() { 2 } else { 3 };
            let file = if prose.is_some() {
                String::new()
            } else {
                args.get(2).cloned().unwrap_or_default()
            };
            let text = match args.get(text_arg) {
                Some(text) => text.clone(),
                None => {
                    use std::io::Read;
                    let mut buffer = String::new();
                    std::io::stdin().read_to_string(&mut buffer)?;
                    buffer
                }
            };

            if let Some(stream) = prose {
                let matched = rules.match_prose(&text, stream);
                if matched.is_empty() {
                    println!("No rule matches.");
                    return Ok(());
                }
                for rule in matched {
                    println!("{}  (would stop the turn)", rule.name);
                    if let Some(description) = &rule.description {
                        println!("  {description}");
                    }
                }
                return Ok(());
            }

            // Shaped like the tool's own arguments, so the same payload
            // extraction runs as in a session.
            let input = match tool.as_str() {
                "Bash" => serde_json::json!({ "command": text }),
                "Write" => serde_json::json!({ "file_path": file, "content": text }),
                _ => serde_json::json!({
                    "file_path": file,
                    "old_string": "",
                    "new_string": text
                }),
            };

            let matched = rules.match_tool(tool, &input);
            if matched.is_empty() {
                println!("No rule matches.");
                return Ok(());
            }
            for rule in matched {
                let action = match rule.action {
                    mikmik_core::rules::RuleAction::Block => "would block the call",
                    mikmik_core::rules::RuleAction::Remind => "would ride on the result",
                };
                println!("{}  ({action})", rule.name);
                if let Some(description) = &rule.description {
                    println!("  {description}");
                }
            }
        }
        Some("extract") => {
            let write = args.iter().any(|a| a == "--write");
            let wanted: Vec<&str> = args[1..]
                .iter()
                .filter(|a| !a.starts_with('-'))
                .map(|a| a.as_str())
                .collect();

            let filenames = mikmik_core::agentsmd::MemoryFilenames::from_config(&config);
            let files = mikmik_core::agentsmd::load_all_memory_files(&project_root, filenames);
            let proposals: Vec<_> = files
                .iter()
                // A rules directory already holds rules. Only the memory files
                // a person writes as prose have anything to lift out.
                .filter(|file| !mikmik_core::agentsmd::is_conditional_rule(file))
                .flat_map(|file| mikmik_core::rules::propose_rules(file, &project_root))
                .filter(|p| wanted.is_empty() || wanted.contains(&p.name.as_str()))
                .collect();

            if proposals.is_empty() {
                println!("Nothing to lift out of the memory files here.");
                println!(
                    "A line becomes a rule when it carries an inline code span, \
                     because that span is what a regular expression can match."
                );
                return Ok(());
            }

            if !write {
                println!(
                    "{} proposal(s). Each condition and scope is a guess from the \
                     text: read them, then write the ones you want.\n",
                    proposals.len()
                );
                for proposal in &proposals {
                    println!("# {}", proposal.target.display());
                    println!("# from {}\n", proposal.source.display());
                    println!("{}", mikmik_core::rules::render_proposal(proposal));
                }
                println!(
                    "Write them with: mikmik rules extract --write [name...]\n\
                     Without a name, every proposal above is written."
                );
                return Ok(());
            }

            let mut written = 0usize;
            for proposal in &proposals {
                if proposal.target.exists() {
                    println!("skipped {} (already there)", proposal.target.display());
                    continue;
                }
                if let Some(parent) = proposal.target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(
                    &proposal.target,
                    mikmik_core::rules::render_proposal(proposal),
                )?;
                println!("wrote {}", proposal.target.display());
                written += 1;
            }
            if written > 0 {
                println!(
                    "\nThe line each rule came from is still in its memory file. \
                     Remove it there if you want the rule to speak only when it \
                     is broken."
                );
            }
        }
        Some(other) => {
            eprintln!("mikmik rules: unknown action '{other}'\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    Ok(())
}

/// Implementation of the `mikmik models` subcommand.
///
/// Flags:
/// * `--refresh` — force-fetch from models.dev (ignoring the 5-minute freshness window), then list.
/// * `--verbose` — also print release date, status, modalities, cache pricing, and capability flags.
/// * `--json` — emit the registry as a JSON object keyed by `provider/model` (suitable for piping into `jq`).
/// * `<provider>` — first non-flag arg filters by provider id (e.g. `mikmik models openai`).
async fn run_models_command(args: &[String]) -> anyhow::Result<()> {
    let mut refresh = false;
    let mut verbose = false;
    let mut as_json = false;
    let mut provider_filter: Option<String> = None;

    for arg in args {
        match arg.as_str() {
            "--refresh" | "-r" => refresh = true,
            "--verbose" | "-v" => verbose = true,
            "--json" => as_json = true,
            s if s.starts_with("--") => {
                eprintln!("mikmik models: unknown flag: {}", s);
                eprintln!("Usage: mikmik models [<provider>] [--refresh] [--verbose] [--json]");
                std::process::exit(2);
            }
            s => {
                if provider_filter.is_some() {
                    eprintln!("mikmik models: only one provider id may be supplied");
                    std::process::exit(2);
                }
                provider_filter = Some(s.to_string());
            }
        }
    }

    let mut registry = mikmik_api::ModelRegistry::new().with_cache_path(models_cache_path());

    if refresh {
        // Force-refresh by clearing the freshness check first.
        let _ = std::fs::remove_file(models_cache_path());
        match registry.refresh_from_models_dev().await {
            Ok(true) => eprintln!("✓ Refreshed from {}", models_source_url()),
            Ok(false) => eprintln!("(no refresh performed — disabled via env or cache fresh)"),
            Err(err) => eprintln!("⚠ refresh failed: {}", err),
        }
    } else {
        // Best-effort: overlay any disk-cached copy on top of the bundled
        // snapshot.  Path may not exist on first run — that's fine.
        registry.load_cache(&models_cache_path());
    }

    // Layer user metadata overrides on top of the catalog (issue #309) so the
    // listing matches what the TUI picker and context logic use.
    let overrides = mikmik_core::config::Settings::load_sync()
        .map(|s| s.effective_config().model_overrides)
        .unwrap_or_default();
    registry.apply_model_overrides(&overrides);

    let mut entries: Vec<&mikmik_api::ModelEntry> = match &provider_filter {
        Some(pid) => registry.list_by_provider(pid),
        None => registry.list_all(),
    };

    // Stable order: provider id, then by descending release_date so newest
    // models appear first.
    entries.sort_by(|a, b| {
        (*a.info.provider_id)
            .cmp(&*b.info.provider_id)
            .then_with(|| {
                let rd_a = a.release_date.as_deref().unwrap_or("");
                let rd_b = b.release_date.as_deref().unwrap_or("");
                rd_b.cmp(rd_a)
            })
            .then_with(|| (*a.info.id).cmp(&*b.info.id))
    });

    if as_json {
        // Re-key by `provider/model` for jq-friendly output.
        let mut map: std::collections::BTreeMap<String, &mikmik_api::ModelEntry> =
            std::collections::BTreeMap::new();
        for e in &entries {
            map.insert(format!("{}/{}", e.info.provider_id, e.info.id), *e);
        }
        let json = serde_json::to_string_pretty(&map)?;
        println!("{}", json);
        return Ok(());
    }

    if entries.is_empty() {
        if let Some(pid) = &provider_filter {
            eprintln!("No models found for provider '{}'.", pid);
            eprintln!("Try: mikmik models                # list all providers");
            eprintln!("     mikmik models --refresh      # pull latest from models.dev");
        } else {
            eprintln!("No models in registry.  Try `mikmik models --refresh`.");
        }
        return Ok(());
    }

    let total = entries.len();

    for entry in &entries {
        let ctx_k = entry.info.context_window / 1000;
        let in_cost = entry.cost_input.unwrap_or(0.0);
        let out_cost = entry.cost_output.unwrap_or(0.0);

        let mut flags = Vec::new();
        if entry.tool_calling {
            flags.push("tools");
        }
        if entry.reasoning {
            flags.push("reasoning");
        }
        if entry.vision() {
            flags.push("vision");
        }
        if entry.audio_input() {
            flags.push("audio");
        }
        if entry.pdf_input() {
            flags.push("pdf");
        }
        let flags_str = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(","))
        };

        if verbose {
            println!(
                "{}/{}  {}  ctx={}K  out={}K  in=${:.2}/M  out=${:.2}/M{}",
                entry.info.provider_id,
                entry.info.id,
                entry.info.name,
                ctx_k,
                entry.info.max_output_tokens / 1000,
                in_cost,
                out_cost,
                flags_str,
            );
            if let Some(rd) = &entry.release_date {
                println!("    released {}", rd);
            }
            if let Some(k) = &entry.knowledge {
                println!("    knowledge cutoff {}", k);
            }
            if let (Some(cr), Some(cw)) = (entry.cost_cache_read, entry.cost_cache_write) {
                println!("    cache: read=${:.2}/M  write=${:.2}/M", cr, cw);
            } else if let Some(cr) = entry.cost_cache_read {
                println!("    cache read=${:.2}/M", cr);
            }
            if !matches!(entry.status, mikmik_api::ModelStatus::Active) {
                println!("    status: {:?}", entry.status);
            }
            if !entry.modalities_input.is_empty() {
                println!(
                    "    modalities: in=[{}] out=[{}]",
                    entry
                        .modalities_input
                        .iter()
                        .map(|m| format!("{:?}", m).to_lowercase())
                        .collect::<Vec<_>>()
                        .join(","),
                    entry
                        .modalities_output
                        .iter()
                        .map(|m| format!("{:?}", m).to_lowercase())
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
        } else {
            println!(
                "{}/{} — {} (ctx: {}K, in: ${:.2}/M, out: ${:.2}/M){}",
                entry.info.provider_id,
                entry.info.id,
                entry.info.name,
                ctx_k,
                in_cost,
                out_cost,
                flags_str,
            );
        }
    }

    if provider_filter.is_none() {
        eprintln!(
            "\n{} models across {} providers.  Use `mikmik models <provider>` to filter.",
            total,
            registry.provider_count()
        );
    }

    Ok(())
}

/// Whether the cache file is fresh enough to skip refreshing.
fn cache_is_fresh(path: &std::path::Path, ttl: std::time::Duration) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mtime = match meta.modified() {
        Ok(t) => t,
        Err(_) => return false,
    };
    match mtime.elapsed() {
        Ok(age) => age < ttl,
        Err(_) => true, // future mtime → treat as fresh
    }
}

/// Background-refresh the models cache from the configured source URL.
///
/// Honors:
/// * `MIKMIK_DISABLE_MODELS_FETCH` — skips the network call entirely.
/// * `MIKMIK_MODELS_URL` / `MODELS_DEV_URL` — overrides the source URL.
/// * 5-minute mtime-based freshness check — avoids hammering models.dev
///   on every CLI invocation.
fn spawn_models_cache_refresh() {
    if std::env::var("MIKMIK_DISABLE_MODELS_FETCH").is_ok() {
        tracing::debug!("MIKMIK_DISABLE_MODELS_FETCH set — skipping models.dev refresh");
        return;
    }
    tokio::spawn(async move {
        refresh_models_cache_once().await;
    });
}

/// TUI-startup analogue of opencode's `ModelsDev` background refresh
/// (models-dev.ts:233-236): fire one refresh now (gated by the 5-minute TTL),
/// then repeat spaced ~60 minutes so a long-running session keeps a fresh
/// catalog on disk for the `/model` picker to reload. Non-blocking — the UI is
/// never held on the network.
fn spawn_models_cache_refresh_loop() {
    if std::env::var("MIKMIK_DISABLE_MODELS_FETCH").is_ok() {
        tracing::debug!("MIKMIK_DISABLE_MODELS_FETCH set — skipping models.dev refresh loop");
        return;
    }
    tokio::spawn(async move {
        loop {
            refresh_models_cache_once().await;
            tokio::time::sleep(std::time::Duration::from_secs(60 * 60)).await;
        }
    });
}

/// Fetch the models.dev catalog into the on-disk cache once, honoring the
/// 5-minute mtime freshness check (mirrors opencode `ModelsDev.fresh()`). All
/// network/parse failures are silent — the bundled snapshot is always
/// sufficient.
async fn refresh_models_cache_once() {
    let cache_path = models_cache_path();
    let legacy_cache_path = models_dev_cache_path();
    let ttl = std::time::Duration::from_secs(5 * 60);

    if cache_is_fresh(&cache_path, ttl) {
        tracing::debug!("Models cache fresh — skipping models.dev refresh");
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let url = models_source_url();
    let resp = match client
        .get(&url)
        .header("User-Agent", concat!("MikMik/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::debug!(?err, "models.dev refresh: network error");
            return;
        }
    };
    if !resp.status().is_success() {
        tracing::debug!(status = ?resp.status(), "models.dev refresh: non-2xx");
        return;
    }
    let text = match resp.text().await {
        Ok(t) => t,
        Err(_) => return,
    };
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write canonical path + legacy path so older installs keep working.
    let _ = std::fs::write(&cache_path, &text);
    let _ = std::fs::write(&legacy_cache_path, &text);
    tracing::info!(path = %cache_path.display(), "Models cache refreshed from {}", url);
}

/// How `/login` and `/connect` are carried out for an editor over ACP.
///
/// The flows live here because they open a browser and listen on loopback,
/// which is the binary's business, not the protocol layer's. The variants that
/// report through a channel are the ones used: the printing ones would put
/// their text on stdout, where every byte is parsed as JSON-RPC.
fn acp_login_runner() -> mikmik_acp::LoginRunner {
    Arc::new(|request: mikmik_acp::LoginRequest, notes| {
        Box::pin(async move {
            let (event_tx, mut event_rx) =
                tokio::sync::mpsc::channel::<mikmik_tui::DeviceAuthEvent>(8);
            let relay = tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    if let mikmik_tui::DeviceAuthEvent::GotBrowserUrl { url } = event {
                        let _ = notes.send(format!(
                            "Opening a browser to sign in. If it did not open, visit:\n{url}"
                        ));
                    }
                }
            });

            let who = if request.provider == mikmik_core::ProviderId::CODEX {
                codex_oauth_flow::run_oauth_flow_with_label(event_tx, request.label.as_deref())
                    .await
                    .map(|tokens| {
                        tokens
                            .account_id
                            .unwrap_or_else(|| "the account".to_string())
                    })
            } else {
                oauth_flow::run_oauth_login_flow_tui(
                    event_tx,
                    request.login_with_claude_ai,
                    request.label.as_deref(),
                )
                .await
                .map(|result| {
                    result
                        .tokens
                        .email
                        .or(result.tokens.account_uuid)
                        .unwrap_or_else(|| "the account".to_string())
                })
            };
            relay.abort();
            who
        })
    })
}

struct RefreshedProviderRuntime {
    config: Config,
    client: Arc<mikmik_api::AnthropicClient>,
    provider_registry: Arc<mikmik_api::ProviderRegistry>,
    model_registry: Arc<mikmik_api::ModelRegistry>,
    auth_store: mikmik_core::AuthStore,
}

async fn refresh_provider_runtime_state(
    current_config: &Config,
) -> anyhow::Result<RefreshedProviderRuntime> {
    mikmik_api::provider_state::clear_saved_provider_state().await?;

    let mut config = current_config.clone();
    config.api_key = None;
    config.provider = None;
    config.model = None;

    let (api_key, use_bearer_auth) = config
        .resolve_anthropic_auth_async()
        .await
        .unwrap_or((String::new(), false));
    // Apply the user-configured request timeout (issue #175) before rebuilding.
    mikmik_api::set_request_timeout_secs(config.resolve_request_timeout_secs_active());
    let client_config = mikmik_api::client::ClientConfig {
        api_key,
        api_base: config.resolve_anthropic_api_base(),
        use_bearer_auth,
        ..Default::default()
    };
    let client = Arc::new(
        mikmik_api::AnthropicClient::new(client_config.clone())
            .context("Failed to rebuild Anthropic client")?,
    );
    let provider_registry = Arc::new(mikmik_api::ProviderRegistry::from_config(
        &config,
        client_config,
    ));
    let model_registry = load_cached_model_registry(&config);

    spawn_models_cache_refresh();

    Ok(RefreshedProviderRuntime {
        config,
        client,
        provider_registry,
        model_registry,
        auth_store: mikmik_core::AuthStore::default(),
    })
}

/// Non-destructive counterpart to [`refresh_provider_runtime_state`]: re-resolve
/// credentials for the CURRENT provider (e.g. right after an in-session OAuth
/// login) and rebuild the client + provider registry, keeping the active
/// provider and model. Mirrors the startup resolution so a just-completed
/// Claude Pro/Max login works without a restart.
async fn reload_provider_runtime_state(
    current_config: &Config,
) -> anyhow::Result<RefreshedProviderRuntime> {
    let config = current_config.clone();

    let (api_key, use_bearer_auth) = if config.selected_provider_id() == "anthropic" {
        config
            .resolve_anthropic_auth_async()
            .await
            .unwrap_or((String::new(), false))
    } else {
        (config.resolve_api_key().unwrap_or_default(), false)
    };

    mikmik_api::set_request_timeout_secs(config.resolve_request_timeout_secs_active());
    let client_config = mikmik_api::client::ClientConfig {
        api_key,
        api_base: config.resolve_anthropic_api_base(),
        use_bearer_auth,
        ..Default::default()
    };
    let client = Arc::new(
        mikmik_api::AnthropicClient::new(client_config.clone())
            .context("Failed to rebuild Anthropic client")?,
    );
    let provider_registry = Arc::new(mikmik_api::ProviderRegistry::from_config(
        &config,
        client_config,
    ));
    let model_registry = load_cached_model_registry(&config);

    Ok(RefreshedProviderRuntime {
        config,
        client,
        provider_registry,
        model_registry,
        auth_store: mikmik_core::AuthStore::default(),
    })
}

/// Record the models an account was discovered to serve.
///
/// Written to disk rather than kept in memory so the list is visible and
/// editable: discovery seeds it once, and the file is the source of truth from
/// then on.
/// Keep `config.provider` in step with a `"<account>/<model>"` model string.
///
/// Only a first segment that actually names an account moves the account.
/// A bare `split_once('/')` also fired on model ids that contain a slash of
/// their own (`meta-llama/Llama-3.3` on OpenRouter), which set the provider to
/// a vendor namespace that is not a provider at all.
///
/// The wire model is not rewritten here; `Config::resolve_route` strips the
/// prefix at request time for both dispatch arms.
fn normalize_provider_from_model(config: &mut Config) {
    let Some(model) = config.model.clone() else {
        return;
    };
    config.provider = Some(config.resolve_route(&model).account);
}

/// The model string a session records and the query loop resolves.
///
/// Canonical, so it still names the account it was chosen from when it is read
/// back under a different selection. `effective_model_for_config` answered
/// with the composite `"<account>/<model>"` about half the time and the bare
/// wire id the other half, with nothing in the type to say which, and this is
/// the string that lands in `session.model` and `qcfg.model`.
fn session_model_string(config: &Config, registry: &mikmik_api::ModelRegistry) -> String {
    let route = mikmik_api::resolve_effective_route(config, registry);
    config.canonical_model(&route.account, &route.model)
}

/// Whether `config` points at the cheap model `/fast` would choose.
///
/// `model.contains("haiku")` only ever answered for Anthropic, so turning fast
/// mode on anywhere else left the badge off while fast mode was on.
fn is_fast_mode_model(config: &Config, registry: &mikmik_api::ModelRegistry) -> bool {
    let Some(model) = config.model.as_deref() else {
        return false;
    };
    let chosen = config.resolve_route(model);
    let fast = mikmik_api::resolve_small_model_route(config, registry);
    chosen.account == fast.account && chosen.model == fast.model
}

/// Filter the tool list based on the agent's access level.
/// - "full"        → all tools allowed (no filtering)
/// - "read-only"   → only ReadOnly/None permission tools and AskUserQuestion
/// - "search-only" → only Grep, Glob, Read, WebSearch, WebFetch tools
fn filter_tools_for_agent(
    tools: Arc<Vec<Box<dyn mikmik_tools::Tool>>>,
    access: &str,
) -> Arc<Vec<Box<dyn mikmik_tools::Tool>>> {
    use mikmik_tools::PermissionLevel as PL;
    match access {
        "read-only" => {
            // Collect names of tools that are read-only, then rebuild from all_tools
            // (Box<dyn Tool> is not Clone so we can't directly filter-and-keep).
            let allowed_names: Vec<String> = tools
                .iter()
                .filter(|t| {
                    matches!(t.permission_level(), PL::ReadOnly | PL::None)
                        || t.name() == "AskUserQuestion"
                })
                .map(|t| t.name().to_string())
                .collect();
            let filtered: Vec<Box<dyn mikmik_tools::Tool>> = mikmik_tools::all_tools()
                .into_iter()
                .filter(|t| allowed_names.iter().any(|n| n == t.name()))
                .collect();
            Arc::new(filtered)
        }
        "search-only" => {
            const SEARCH_TOOLS: &[&str] = &["Grep", "Glob", "Read", "WebSearch", "WebFetch"];
            let filtered: Vec<Box<dyn mikmik_tools::Tool>> = mikmik_tools::all_tools()
                .into_iter()
                .filter(|t| SEARCH_TOOLS.contains(&t.name()))
                .collect();
            Arc::new(filtered)
        }
        _ => tools, // "full" — allow all tools unchanged
    }
}

// ---------------------------------------------------------------------------
// Headless mode: read prompt from arg/stdin, run, print response
// ---------------------------------------------------------------------------

async fn run_headless(
    cli: &Cli,
    client: Arc<mikmik_api::AnthropicClient>,
    tools: Arc<Vec<Box<dyn mikmik_tools::Tool>>>,
    tool_ctx: ToolContext,
    query_config: mikmik_query::QueryConfig,
    cost_tracker: Arc<CostTracker>,
    resume_request: Option<String>,
) -> anyhow::Result<()> {
    use mikmik_query::{QueryEvent, QueryOutcome};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    let mut tool_ctx = tool_ctx;

    // A conversation resumed here continues the same record the TUI writes, so
    // the two modes share one history rather than each keeping its own.
    let mut session = match resolve_resume(resume_request.as_deref()).await {
        ResumeOutcome::Resumed(session) => {
            tool_ctx.session_id = session.id.clone();
            *session
        }
        ResumeOutcome::NothingToResume => {
            eprintln!("Warning: no previous sessions found, starting a new one.");
            mikmik_core::history::ConversationSession::new(query_config.model.clone())
        }
        // A script that asked to continue a conversation and silently got a
        // fresh one is worse off than one that was told.
        ResumeOutcome::Failed(message) => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        ResumeOutcome::NotRequested => {
            let mut session =
                mikmik_core::history::ConversationSession::new(query_config.model.clone());
            session.id = tool_ctx.session_id.clone();
            session
        }
    };
    let resumed_messages = session.messages.clone();

    // Build initial messages list from input.
    // --input-format stream-json: stdin is newline-delimited JSON, each line is
    //   {"role":"user"|"assistant","content":"..."} (mirrors TS --input-format stream-json).
    // --input-format text (default): read prompt from positional arg or entire stdin as text.
    let mut messages: Vec<mikmik_core::types::Message> =
        if cli.input_format == CliInputFormat::StreamJson {
            use tokio::io::{self, AsyncBufReadExt, BufReader};
            let stdin = io::stdin();
            let mut reader = BufReader::new(stdin);
            let mut line = String::new();
            let mut parsed: Vec<mikmik_core::types::Message> = Vec::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(v) => {
                        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                        let content = v
                            .get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        if role == "assistant" {
                            parsed.push(mikmik_core::types::Message::assistant(content));
                        } else {
                            parsed.push(mikmik_core::types::Message::user(content));
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: skipping malformed JSON line: {} ({:?})",
                            trimmed, e
                        );
                    }
                }
            }
            if parsed.is_empty() {
                // Also check positional arg as fallback
                if let Some(ref p) = cli.prompt {
                    parsed.push(mikmik_core::types::Message::user(p.clone()));
                }
            }
            parsed
        } else {
            // Plain text mode
            let prompt = if let Some(ref p) = cli.prompt {
                p.clone()
            } else {
                use tokio::io::{self, AsyncReadExt};
                let mut stdin = io::stdin();
                let mut buf = String::new();
                stdin.read_to_string(&mut buf).await?;
                buf.trim().to_string()
            };

            if prompt.is_empty() {
                eprintln!("Error: No prompt provided. Use --print <prompt> or pipe text to stdin.");
                std::process::exit(1);
            }

            vec![mikmik_core::types::Message::user(prompt)]
        };

    // --prefill: inject a partial assistant turn before the query so the model
    // continues from that text (mirrors TS --prefill flag).
    if let Some(ref prefill_text) = cli.prefill {
        messages.push(mikmik_core::types::Message::assistant(prefill_text.clone()));
    }

    if messages.is_empty() {
        eprintln!("Error: No messages provided.");
        std::process::exit(1);
    }

    // The resumed turns go in front of this run's prompt, or the model answers
    // without the conversation the user asked to continue.
    if !resumed_messages.is_empty() {
        let mut history = resumed_messages;
        history.append(&mut messages);
        messages = history;
    }

    let is_json_output = matches!(
        cli.output_format,
        CliOutputFormat::Json | CliOutputFormat::StreamJson
    );
    let is_stream_json = matches!(cli.output_format, CliOutputFormat::StreamJson);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<QueryEvent>();
    let cancel = CancellationToken::new();
    let client_clone = client.clone();
    let tool_ctx_clone = tool_ctx.clone();
    let qcfg = query_config.clone();
    let tracker_clone = cost_tracker.clone();
    let event_tx_clone = event_tx.clone();
    let cancel_clone = cancel.clone();

    // The final message list comes back out so the session can be saved: a
    // `--print` conversation used to leave nothing behind at all.
    let msgs_arc = Arc::new(tokio::sync::Mutex::new(messages));
    let msgs_arc_clone = msgs_arc.clone();
    let query_handle = tokio::spawn(async move {
        let mut msgs = msgs_arc_clone.lock().await.clone();
        let outcome = mikmik_query::run_query_loop(
            client_clone.as_ref(),
            &mut msgs,
            tools.as_slice(),
            &tool_ctx_clone,
            &qcfg,
            tracker_clone,
            Some(event_tx_clone),
            cancel_clone,
            None,
        )
        .await;
        *msgs_arc_clone.lock().await = msgs;
        outcome
    });

    // Drop the original tx so the channel closes when the task drops its clone
    drop(event_tx);

    // Drain events and print streaming text
    let mut full_text = String::new();

    while let Some(event) = event_rx.recv().await {
        match &event {
            QueryEvent::Stream(mikmik_api::AnthropicStreamEvent::ContentBlockDelta {
                delta: mikmik_api::streaming::ContentDelta::TextDelta { text },
                ..
            }) => {
                full_text.push_str(text);
                if !is_json_output {
                    print!("{}", text);
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                } else if is_stream_json {
                    let chunk = serde_json::json!({ "type": "text_delta", "text": text });
                    println!("{}", chunk);
                }
            }
            QueryEvent::ToolStart { tool_name, .. } => {
                if !is_json_output {
                    eprintln!("\n[{}...]", tool_name);
                } else {
                    let ev = serde_json::json!({ "type": "tool_start", "tool": tool_name });
                    println!("{}", ev);
                }
            }
            QueryEvent::Error(msg) => {
                if is_json_output {
                    let ev = serde_json::json!({ "type": "error", "error": msg });
                    eprintln!("{}", ev);
                } else {
                    eprintln!("\nError: {}", msg);
                }
            }
            _ => {}
        }
    }

    // Wait for the query task to finish and get the final outcome
    let outcome =
        query_handle
            .await
            .unwrap_or(QueryOutcome::Error(mikmik_core::error::ClaudeError::Other(
                "Query task panicked".to_string(),
            )));

    // Final output
    match cli.output_format {
        CliOutputFormat::Json => match outcome {
            QueryOutcome::EndTurn { message, usage } => {
                let result_text = if full_text.is_empty() {
                    message.get_all_text()
                } else {
                    full_text
                };
                let out = serde_json::json!({
                    "type": "result",
                    "result": result_text,
                    "usage": {
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                        "cache_read_input_tokens": usage.cache_read_input_tokens,
                    },
                    "cost_usd": cost_tracker.total_cost_usd(),
                });
                println!("{}", out);
            }
            QueryOutcome::Error(e) => {
                let out = serde_json::json!({ "type": "error", "error": e.to_string() });
                eprintln!("{}", out);
                std::process::exit(1);
            }
            _ => {}
        },
        CliOutputFormat::StreamJson => {
            // Already streamed above; emit final result event
            match outcome {
                QueryOutcome::EndTurn { usage, .. } => {
                    let out = serde_json::json!({
                        "type": "result",
                        "usage": {
                            "input_tokens": usage.input_tokens,
                            "output_tokens": usage.output_tokens,
                        },
                        "cost_usd": cost_tracker.total_cost_usd(),
                    });
                    println!("{}", out);
                }
                QueryOutcome::Error(e) => {
                    let out = serde_json::json!({ "type": "error", "error": e.to_string() });
                    eprintln!("{}", out);
                    std::process::exit(1);
                }
                _ => {}
            }
        }
        CliOutputFormat::Text => {
            // Streaming text was already printed; add newline
            println!();
            if cli.verbose {
                eprintln!(
                    "\nTokens: {} in / {} out | Cost: ${:.4}",
                    cost_tracker.input_tokens(),
                    cost_tracker.output_tokens(),
                    cost_tracker.total_cost_usd(),
                );
            }
            match outcome {
                QueryOutcome::Error(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
                QueryOutcome::BudgetExceeded {
                    cost_usd,
                    limit_usd,
                } => {
                    eprintln!(
                        "Budget limit ${:.4} reached (spent ${:.4}). Stopping.",
                        limit_usd, cost_usd
                    );
                    std::process::exit(2);
                }
                _ => {}
            }
        }
    }

    // A `--print` conversation used to leave nothing behind: it was absent from
    // `/session`, `/resume`, `/search` and every report, however long it ran.
    {
        let mut final_messages = msgs_arc.lock().await.clone();
        let mut transcript = mikmik_core::session_storage::TranscriptRecorder::new(
            mikmik_core::session_storage::transcript_root_for(&tool_ctx.working_dir),
            session.id.clone(),
        );
        if let Err(e) = transcript
            .record_turn(&mut final_messages, &tool_ctx.working_dir)
            .await
        {
            eprintln!("Warning: could not write the session transcript: {e}");
        }
        session.messages = final_messages;
        session.updated_at = chrono::Utc::now();
        mikmik_core::history::create_checkpoint(&mut session, None);
        session.model = query_config.model.clone();
        session.working_dir = Some(tool_ctx.working_dir.display().to_string());
        if let Err(e) = persist_session(&session).await {
            eprintln!("Warning: could not save the session: {e}");
        }
    }

    // Interpreters started by the REPL tool are kept alive between calls on
    // purpose; this is where that purpose ends.
    mikmik_tools::repl_tool::shutdown_session(&tool_ctx.session_id).await;
    mikmik_tools::computer_script::shutdown_session(&tool_ctx.session_id).await;
    // A language server holds the whole project in memory and outlives the
    // session otherwise: its manager is a global, so nothing else stops it.
    // `shutdown` asks first and kills the process tree after, so a server that
    // spawned a compiler does not leave it behind.
    mikmik_core::lsp::global_lsp_manager()
        .lock()
        .await
        .shutdown_all()
        .await;
    // Which problems this session already reported is session state too.
    mikmik_tools::lsp_after_write::forget_session(&tool_ctx.session_id).await;
    // Which conditional rules already spoke is session state too.
    mikmik_core::rules::forget_session(&tool_ctx.session_id);
    // The auto-compact circuit breaker is keyed by session, so it has to be
    // dropped here or a long-lived process keeps one entry per session it ran.
    mikmik_query::compact::forget_compact_state(&tool_ctx.session_id);

    mikmik_plugins::run_global_hook(
        mikmik_plugins::HookEventKind::SessionEnd,
        None,
        serde_json::json!({ "session_id": tool_ctx.session_id }),
    )
    .await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Interactive REPL mode
// ---------------------------------------------------------------------------

fn permission_request_from_core(
    pending: &mikmik_tools::PendingPermissionRequest,
) -> mikmik_tui::dialogs::PermissionRequest {
    let reason = pending.reason.clone();
    let tool_name = pending.request.tool_name.clone();
    let tool_use_id = pending.tool_use_id.clone();

    match (tool_name.as_str(), pending.request.path.clone()) {
        ("Bash", Some(command)) => {
            // No prefix option for a command that destroys data. The
            // allowlist refuses to cover one, so offering to add a prefix
            // would promise an approval that never applies again.
            let suggested_prefix = mikmik_core::bash_classifier::destructive_command_in(&command)
                .is_none()
                .then(|| {
                    command
                        .split_whitespace()
                        .next()
                        .filter(|prefix| !prefix.is_empty())
                        .map(|prefix| format!("{} ", prefix))
                })
                .flatten();
            mikmik_tui::dialogs::PermissionRequest::bash(
                tool_use_id,
                tool_name,
                reason,
                command,
                suggested_prefix,
            )
        }
        ("PowerShell", Some(command)) => mikmik_tui::dialogs::PermissionRequest::powershell(
            tool_use_id,
            tool_name,
            reason,
            command,
        ),
        ("Read", Some(path)) => {
            mikmik_tui::dialogs::PermissionRequest::file_read(tool_use_id, tool_name, reason, path)
        }
        (_, Some(path)) if matches!(tool_name.as_str(), "Write" | "Edit" | "NotebookEdit") => {
            mikmik_tui::dialogs::PermissionRequest::file_write(tool_use_id, tool_name, reason, path)
        }
        _ => mikmik_tui::dialogs::PermissionRequest::from_reason(
            tool_use_id,
            tool_name,
            reason,
            pending.request.path.clone(),
        ),
    }
}

/// Turns kept in the backfill sent to a remote client on connect.
///
/// The relay holds a bounded ring buffer, so an unbounded backfill would push
/// the live events straight back out of it.
const BRIDGE_HISTORY_TURNS: usize = 40;

/// Timeline rows kept in the backfill sent to a remote client on connect.
///
/// Same reasoning as `BRIDGE_HISTORY_TURNS`, and the ring buffer is shared
/// between the two: the timeline caps at 200 rows, and replaying all of them
/// would push the transcript back out of the buffer it just arrived in.
const BRIDGE_TIMELINE_ROWS: usize = 40;

/// Characters kept per turn in the backfill.
const BRIDGE_HISTORY_CHARS: usize = 4_000;

/// Apply a new session title everywhere it is held, and persist it.
///
/// Four places track the title and each one is read by something different:
/// the session record is what gets saved, the command context is what
/// commands see, the app field is what the export writers read, and the
/// terminal title is what the window manager shows. A rename that misses one
/// leaves that surface stale, which is exactly how `/export` came to write
/// the old name.
async fn apply_session_rename(
    title: String,
    session: &mut mikmik_core::history::ConversationSession,
    cmd_ctx: &mut mikmik_commands::CommandContext,
    app: &mut mikmik_tui::App,
    transcript: &mut mikmik_core::session_storage::TranscriptRecorder,
) {
    session.title = Some(title.clone());
    session.updated_at = chrono::Utc::now();
    cmd_ctx.session_title = session.title.clone();
    app.session_title = session.title.clone();
    if let Err(e) = mikmik_core::history::save_session(session).await {
        app.push_notification(
            mikmik_tui::NotificationKind::Error,
            format!("Renamed the session, but could not save it: {e}"),
            None,
        );
    }
    // The session list reads the title off the transcript's tail, so a rename
    // that stopped at the session record would not reach it.
    if let Err(e) = transcript.record_title(&title).await {
        app.push_notification(
            mikmik_tui::NotificationKind::Error,
            format!("Renamed the session, but could not record it: {e}"),
            None,
        );
    }
    mikmik_tui::update_terminal_title(Some(&title));
    app.status_message = Some(format!("Session renamed to \"{}\".", title));
}

/// Swap the running session for `resumed`, moving every piece of state with it.
///
/// The session id, the model, the working directory and the turn-diff state all
/// belong to the session, so leaving any one of them behind sends the next turn
/// out under the wrong session. Kept in one place because two callers need it:
/// `/resume <id>` and the session browser's Enter.
fn apply_session_resume(
    resumed: mikmik_core::history::ConversationSession,
    session: &mut mikmik_core::history::ConversationSession,
    messages: &mut Vec<mikmik_core::types::Message>,
    cmd_ctx: &mut mikmik_commands::CommandContext,
    tool_ctx: &mut ToolContext,
    app: &mut mikmik_tui::App,
    transcript: &mut mikmik_core::session_storage::TranscriptRecorder,
) {
    *session = resumed;
    *messages = session.messages.clone();
    app.replace_messages(messages.clone());
    cmd_ctx.config.model = Some(session.model.clone());
    app.config.model = Some(session.model.clone());
    tool_ctx.config.model = Some(session.model.clone());
    app.model_name = session.model.clone();
    tool_ctx.session_id = session.id.clone();
    tool_ctx.file_history = Arc::new(ParkingMutex::new(
        mikmik_core::file_history::FileHistory::new(),
    ));
    tool_ctx.current_turn = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    cmd_ctx.session_id = session.id.clone();
    cmd_ctx.session_title = session.title.clone();
    app.session_id = session.id.clone();
    if let Some(saved_dir) = session.working_dir.as_ref() {
        let saved_path = std::path::PathBuf::from(saved_dir);
        if saved_path.exists() {
            tool_ctx.working_dir = saved_path.clone();
            cmd_ctx.working_dir = saved_path;
        }
    }
    app.config.project_dir = Some(tool_ctx.working_dir.clone());
    // The footer reads its own field, so without this it keeps naming the
    // directory the session was resumed *from* while tools run in the new one.
    app.current_dir = tool_ctx.working_dir.to_str().map(|s| s.to_string());
    app.attach_turn_diff_state(tool_ctx.file_history.clone(), tool_ctx.current_turn.clone());
    // The resumed session has its own transcript, under whatever root its
    // working directory resolves to.
    transcript.rebind(
        mikmik_core::session_storage::transcript_root_for(&tool_ctx.working_dir),
        session.id.clone(),
    );
    mikmik_tui::update_terminal_title(session.title.as_deref());
    // By characters, not bytes: an id shorter than eight of them would panic
    // a byte slice, and nothing guarantees the length of one.
    let short_id: String = session.id.chars().take(8).collect();
    app.status_message = Some(format!("Resumed session {}.", short_id));
}

/// Write a finished turn's session to the stores that outlive the process.
///
/// The record is what `/session` and `/resume` read; the SQLite index is what
/// `/search` reads. A session that reaches one and not the other is present on
/// one surface and missing from the next, so both happen here or neither does.
/// The transcript is written separately, by the recorder that owns the message
/// uuids.
/// Summarise a conversation on demand, for `/compact`.
///
/// Picks the same backend the next turn would dispatch through, so the summary
/// is written by the model the session is actually talking to rather than by
/// whichever one happens to have a client lying around.
async fn run_compaction(
    messages: &[mikmik_core::types::Message],
    model: &str,
    instruction: Option<&str>,
    session_id: &str,
    config: &mikmik_core::Config,
    client: &mikmik_api::AnthropicClient,
    provider_registry: Option<&std::sync::Arc<mikmik_api::ProviderRegistry>>,
) -> mikmik_query::compact::CompactionRun {
    // `/compact` honours the compact model too. "Always that one" has to hold
    // on every surface, or the setting means "usually".
    mikmik_query::compact::compact_on_demand(
        &config.resolve_route(model),
        config,
        provider_registry.map(|registry| registry.as_ref()),
        client,
        messages,
        instruction,
        session_id,
    )
    .await
}

/// Summarise the conversation in place and move every surface that follows it.
///
/// Returns whether the transcript was replaced, which the caller uses to decide
/// whether to reload it.
///
/// Shared by `/compact` and by the plan answer that clears the context before
/// the work starts: this is a long piece of bookkeeping (the app's copy, the
/// session record, the transcript's tip, the footer's token count), and a
/// second copy of it is how one surface ends up not moving.
async fn compact_conversation(
    instruction: Option<&str>,
    messages: &mut Vec<mikmik_core::types::Message>,
    app: &mut mikmik_tui::App,
    session: &mut mikmik_core::history::ConversationSession,
    transcript: &mut mikmik_core::session_storage::TranscriptRecorder,
    config: &mikmik_core::Config,
    client: &mikmik_api::AnthropicClient,
    provider_registry: Option<&std::sync::Arc<mikmik_api::ProviderRegistry>>,
    model_registry: &mikmik_api::ModelRegistry,
    session_id: &str,
) -> bool {
    let before = messages.len();
    let model = session_model_string(config, model_registry);
    app.status_message = Some("Compacting the conversation…".to_string());
    let run = run_compaction(
        messages,
        &model,
        instruction,
        session_id,
        config,
        client,
        provider_registry,
    )
    .await;
    // The chosen compact model could not write it and the turn's own did. Said
    // before the outcome, so the reason arrives with it.
    if let Some(note) = run.note {
        app.push_notification(mikmik_tui::NotificationKind::Warning, note, None);
    }
    match run.result {
        Ok(new_msgs) => {
            *messages = new_msgs.clone();
            app.replace_messages(new_msgs);
            session.messages = messages.clone();
            session.updated_at = chrono::Utc::now();
            // The transcript keeps the turns the summary replaced; its tip has
            // to follow the conversation or the next launch reloads them.
            let tip = messages.last().and_then(|m| m.uuid.clone());
            if let Err(e) = transcript.set_active_leaf(tip.as_deref()).await {
                app.push_notification(
                    mikmik_tui::NotificationKind::Error,
                    format!("Could not move the transcript's tip: {e}"),
                    None,
                );
            }
            // The summary is the whole prompt now, so the footer has to say so.
            app.context_used_tokens = mikmik_query::compact::estimate_context_size(messages);
            app.token_warning_threshold_shown = 0;
            let removed = before.saturating_sub(messages.len());
            // A conversation that already fits the keep-recent budget comes
            // back untouched, and saying "compacted 0 messages" reads as a
            // failure rather than as nothing needing to be done.
            app.status_message = Some(if removed == 0 {
                "Nothing to summarise: the conversation already fits.".to_string()
            } else {
                format!(
                    "Compacted {removed} message{} into a summary.",
                    if removed == 1 { "" } else { "s" }
                )
            });
            true
        }
        Err(e) => {
            // The conversation is untouched.
            app.status_message = None;
            app.push_notification(
                mikmik_tui::NotificationKind::Error,
                format!("Could not compact: {e}"),
                None,
            );
            false
        }
    }
}

async fn persist_session(
    session: &mikmik_core::history::ConversationSession,
) -> anyhow::Result<()> {
    mikmik_core::history::save_session(session).await?;

    let db_path = mikmik_core::config::Settings::config_dir().join("sessions.db");
    let store = mikmik_core::SqliteSessionStore::open(&db_path)?;
    store.save_session(&session.id, session.title.as_deref(), &session.model)?;
    for msg in &session.messages {
        let content_str = match &msg.content {
            mikmik_core::types::MessageContent::Text(t) => t.clone(),
            mikmik_core::types::MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    if let mikmik_core::types::ContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        };
        let role = match msg.role {
            mikmik_core::types::Role::User => "user",
            mikmik_core::types::Role::Assistant => "assistant",
        };
        let msg_id = msg.uuid.as_deref().unwrap_or("unknown");
        store.save_message(&session.id, msg_id, role, &content_str, None)?;
    }
    Ok(())
}

/// What `--resume` (or `-c`) resolved to.
///
/// Both modes go through this, so a flag cannot work at the keyboard and
/// silently do nothing in `--print`, which is how `--resume` behaved: headless
/// accepted an id, ignored it, and started a fresh conversation.
enum ResumeOutcome {
    /// No resume was asked for.
    NotRequested,
    Resumed(Box<mikmik_core::history::ConversationSession>),
    /// The most recent session was asked for and there is none.
    NothingToResume,
    /// A named session could not be loaded.
    Failed(String),
}

/// Resolve `--resume` / `-c` into the session to continue.
async fn resolve_resume(resume_id: Option<&str>) -> ResumeOutcome {
    let Some(id) = resume_id else {
        return ResumeOutcome::NotRequested;
    };

    let id = if id == "__last__" {
        let listing = mikmik_core::history::list_sessions().await;
        for failure in &listing.unreadable {
            warn!(
                path = %failure.path.display(),
                error = %failure.error,
                "Session file could not be read; it cannot be resumed"
            );
        }
        match listing.sessions.first() {
            Some(last) => last.id.clone(),
            None => return ResumeOutcome::NothingToResume,
        }
    } else {
        id.to_string()
    };

    match mikmik_core::history::load_session(&id).await {
        Ok(session) => ResumeOutcome::Resumed(Box::new(session)),
        Err(e) => ResumeOutcome::Failed(format!("Could not load session {id}: {e}")),
    }
}

/// Whether the session can turn a queued remote prompt into a turn right now.
///
/// Kept as a free function over plain booleans so the rule can be tested
/// without standing up an `App`, and so both the decision and its reasons stay
/// in one place rather than being spelled out at the call site.
fn remote_turn_can_start(
    is_streaming: bool,
    query_in_flight: bool,
    blocking_modal: bool,
    prompt_box_empty: bool,
) -> bool {
    // `query_in_flight` is checked separately from `is_streaming`: the flag is
    // cleared as soon as the last event arrives, while the task is still being
    // joined, and starting a turn in that window would leak the handle.
    !is_streaming && !query_in_flight && !blocking_modal && prompt_box_empty
}

/// What to tell a caller who asked for something only the terminal can show.
///
/// The command's own help text comes along because that is where the argument
/// form lives: `/model` answers with the way to set a model outright, which is
/// the thing the picker would have done.
fn terminal_only_notice(cmd: &str) -> String {
    let usage = mikmik_commands::find_command(cmd)
        .map(|command| command.help().to_string())
        .unwrap_or_default();
    if usage.is_empty() {
        format!("/{cmd} answers with a view on the terminal.")
    } else {
        format!("/{cmd} answers with a view on the terminal.\n{usage}")
    }
}

/// Why a queued remote prompt has not started yet, phrased for the sender.
///
/// `None` when nothing is holding it, which is the caller's signal that the
/// wait is over rather than a reason worth reporting.
fn remote_wait_reason(
    busy: bool,
    blocking_modal: bool,
    prompt_box_empty: bool,
) -> Option<&'static str> {
    // Order matters: a turn can be running *and* be blocked on a dialog, and
    // the dialog is the thing the operator has to act on.
    if blocking_modal {
        Some("Queued: the terminal is waiting on a dialog.")
    } else if busy {
        Some("Queued: a turn is already running.")
    } else if !prompt_box_empty {
        Some("Queued: someone is typing at the terminal.")
    } else {
        None
    }
}

/// Describe a project MCP server awaiting trust, for a remote client.
///
/// Sent both when the prompt opens and again when a client connects to a
/// session that already has one on screen, which is why the request id is
/// passed in rather than generated here: both announcements have to name the
/// same prompt.
fn mcp_approval_request(
    request_id: &str,
    server: &mikmik_core::config::McpServerConfig,
) -> mikmik_bridge::BridgeOutbound {
    mikmik_bridge::BridgeOutbound::McpApprovalRequest {
        request_id: request_id.to_string(),
        server_name: server.name.clone(),
        command: server.command_line(),
        url: server.url.clone(),
    }
}

/// Describe the bypass-permissions warning for a remote client.
///
/// The wording and the two answers come from the dialog module rather than
/// being written here, so the terminal and the browser cannot end up warning
/// about different things or offering differently worded answers.
fn bypass_warning_request(request_id: &str, at_startup: bool) -> mikmik_bridge::BridgeOutbound {
    mikmik_bridge::BridgeOutbound::BypassWarning {
        request_id: request_id.to_string(),
        message: mikmik_tui::bypass_permissions_dialog::bypass_warning_message(),
        options: mikmik_tui::bypass_permissions_dialog::bypass_answer_labels(at_startup),
    }
}

/// Everything a client needs to see the session as it stands.
///
/// Both the runner's own connect and a client attaching later go through here,
/// so a prompt that only one of those paths knew about cannot exist. Every
/// entry is rebuilt from what is on screen, which is the only state that
/// outlives the relay's ring buffer.
fn session_snapshot(
    app: &mikmik_tui::App,
    question_id: Option<&str>,
    mcp_request_id: Option<&str>,
    bypass_request_id: Option<&str>,
    messages: &[mikmik_core::types::Message],
) -> Vec<mikmik_bridge::BridgeOutbound> {
    let mut snapshot = Vec::new();

    // First, because a client treats History as the whole transcript and
    // replaces what it has; sent after the prompts it would wipe them.
    //
    // Only while idle: mid-turn it would also wipe the bubble the deltas are
    // still filling, and the deltas that follow would have nowhere to land.
    if !app.is_streaming {
        let (entries, omitted) = history_for_bridge(messages);
        if !entries.is_empty() || omitted > 0 {
            snapshot.push(mikmik_bridge::BridgeOutbound::History { entries, omitted });
        }
    }

    // After History, which a client treats as the whole transcript and which
    // clears the timeline with it. The ring buffer may still hold rows this
    // client is about to replay, but those arrive before History and would be
    // wiped by it, so the current rows are resent here.
    let rows = &app.timeline.rows;
    let dropped = rows.len().saturating_sub(BRIDGE_TIMELINE_ROWS);
    for row in &rows[dropped..] {
        snapshot.push(mikmik_bridge::BridgeOutbound::TimelineRow(row.clone()));
    }

    if let Some(request) = app.permission_request.as_ref() {
        snapshot.push(mikmik_bridge::BridgeOutbound::PermissionRequest {
            request_id: request.tool_use_id.clone(),
            tool_use_id: request.tool_use_id.clone(),
            tool_name: request.tool_name.clone(),
            description: request.description.clone(),
            options: request
                .options
                .iter()
                .map(|option| option.label.clone())
                .collect(),
        });
    }

    if app.ask_user_dialog.visible {
        if let Some(question_id) = question_id {
            snapshot.push(mikmik_bridge::BridgeOutbound::UserQuestion {
                question_id: question_id.to_string(),
                question: app.ask_user_dialog.question.clone(),
                options: app.ask_user_dialog.options.clone().unwrap_or_default(),
            });
        }
    }

    if app.mcp_approval.visible {
        if let (Some(request_id), Some(server)) = (mcp_request_id, app.mcp_prompting.as_ref()) {
            snapshot.push(mcp_approval_request(request_id, server));
        }
    }

    // Last, and unconditional on `is_streaming`: nothing runs while this is up,
    // so a client that misses it watches a session that looks idle and will
    // stay that way until someone answers at the terminal.
    if app.bypass_permissions_dialog.visible {
        if let Some(request_id) = bypass_request_id {
            snapshot.push(bypass_warning_request(
                request_id,
                app.bypass_permissions_dialog.at_startup,
            ));
        }
    }

    snapshot
}

/// Translate a remote decision into the choice the TUI dialog settles with.
///
/// Kept separate from the dialog so a remote answer and a keyboard answer end
/// up at the same `handle_mcp_approval_decision` call.
fn mcp_choice_for(
    decision: mikmik_bridge::McpApprovalDecision,
) -> mikmik_tui::dialogs::McpApprovalChoice {
    use mikmik_bridge::McpApprovalDecision;
    use mikmik_tui::dialogs::McpApprovalChoice;
    match decision {
        McpApprovalDecision::AllowSession => McpApprovalChoice::AllowSession,
        McpApprovalDecision::AllowAlways => McpApprovalChoice::AllowAlways,
        McpApprovalDecision::Deny => McpApprovalChoice::Deny,
    }
}

/// Build the conversation backfill for a remote client.
///
/// Returns the kept turns plus how many earlier ones were left out, so the
/// client can say the transcript is partial instead of implying it starts
/// here.
fn history_for_bridge(
    messages: &[mikmik_core::types::Message],
) -> (Vec<mikmik_bridge::BridgeHistoryEntry>, usize) {
    use mikmik_core::types::{ContentBlock, MessageContent, Role};

    let omitted = messages.len().saturating_sub(BRIDGE_HISTORY_TURNS);
    let entries = messages[omitted..]
        .iter()
        .filter_map(|message| {
            let role: &str = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };

            let mut text = String::new();
            let mut tools = Vec::new();
            match &message.content {
                MessageContent::Text(body) => text.push_str(body),
                MessageContent::Blocks(blocks) => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text: body } => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(body);
                            }
                            ContentBlock::ToolUse { name, .. } => tools.push(name.clone()),
                            _ => {}
                        }
                    }
                }
            }

            if text.chars().count() > BRIDGE_HISTORY_CHARS {
                text = text.chars().take(BRIDGE_HISTORY_CHARS).collect::<String>();
                text.push_str("\n… (truncated)");
            }

            if text.trim().is_empty() && tools.is_empty() {
                return None;
            }

            Some(mikmik_bridge::BridgeHistoryEntry {
                role: role.to_string(),
                text,
                tools,
            })
        })
        .collect();

    (entries, omitted)
}

/// Build the user message for a prompt that arrived over the bridge.
///
/// An image becomes an image block the model can actually look at. Text is
/// folded into the prompt under its filename. Anything else is named and
/// skipped rather than pushed through as base64, which would be noise to the
/// model and would hide the fact that the file never arrived.
/// Describe one finished turn for a remote client.
///
/// `model` is the model that ran the turn, which an agent definition or a
/// fallback switch can make different from the session model. The turn is
/// priced here rather than diffed from the running total, because two turns
/// finishing between reads would misattribute the cost.
fn bridge_usage(
    model: &str,
    usage: &mikmik_core::types::UsageInfo,
    session_cost_usd: f64,
) -> mikmik_bridge::BridgeUsage {
    mikmik_bridge::BridgeUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_tokens: usage.cache_creation_input_tokens,
        cache_read_tokens: usage.cache_read_input_tokens,
        cost_usd: Some(mikmik_core::cost::ModelPricing::for_model(model).cost_of(
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        )),
        session_cost_usd: Some(session_cost_usd),
    }
}

fn remote_user_message(
    content: &str,
    attachments: &[mikmik_bridge::BridgeAttachment],
) -> mikmik_core::types::Message {
    use mikmik_core::types::{ContentBlock, ImageSource, MessageContent};

    if attachments.is_empty() {
        return mikmik_core::types::Message::user(content);
    }

    let mut text = content.to_string();
    let mut images = Vec::new();

    for attachment in attachments {
        let mime = attachment.mime_type.as_deref().unwrap_or("");
        if mime.starts_with("image/") {
            images.push(ContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".to_string(),
                    media_type: Some(mime.to_string()),
                    data: Some(attachment.content.clone()),
                    url: None,
                },
            });
        } else if mime.starts_with("text/") || mime.is_empty() {
            text.push_str(&format!(
                "\n\n--- {} ---\n{}",
                attachment.name, attachment.content
            ));
        } else {
            text.push_str(&format!(
                "\n\n[attachment {} ({}) was not sent: unsupported type]",
                attachment.name, mime
            ));
        }
    }

    let mut message = mikmik_core::types::Message::user(String::new());
    let mut blocks = Vec::with_capacity(images.len() + 1);
    if !text.trim().is_empty() {
        blocks.push(ContentBlock::Text { text });
    }
    blocks.extend(images);
    message.content = MessageContent::Blocks(blocks);
    message
}

/// Settle a waiting permission request and release the blocked tool.
///
/// `selected_key` uses the dialog's own option keys: `y` allow once, `Y` allow
/// for the rest of the session, `p` allow persistently, `n` deny. Anything else
/// is treated as a plain allow, matching the dialog's default action.
///
/// Both the keyboard handler and the remote-control handler call this, so a
/// decision made on a phone cannot drift from one made at the terminal.
///
/// Returns `false` when no request was waiting under that id, which happens if
/// the same request was already answered from the other side.
/// What a settled permission prompt decided, for the caller to report on.
struct PermissionSettlement {
    tool_name: String,
    denied: bool,
}

fn settle_pending_permission(
    pending_permissions: &ParkingMutex<mikmik_tools::PendingPermissionStore>,
    permission_manager: Option<&Arc<std::sync::Mutex<mikmik_core::permissions::PermissionManager>>>,
    tool_use_id: &str,
    selected_key: Option<char>,
) -> Option<PermissionSettlement> {
    let mut pending = pending_permissions.lock().waiting.remove(tool_use_id)?;

    let selected_path = pending.request.path.clone();
    let decision = match selected_key {
        Some('n') => mikmik_core::permissions::PermissionDecision::Deny,
        _ => mikmik_core::permissions::PermissionDecision::Allow,
    };

    if let Some(manager) = permission_manager {
        if let Ok(mut manager) = manager.lock() {
            match selected_key {
                Some('Y') => {
                    if let Some(path) = selected_path.as_deref() {
                        manager.add_session_allow_path(&pending.request.tool_name, path);
                    } else {
                        manager.add_session_allow(&pending.request.tool_name);
                    }
                }
                Some('p') => {
                    let mut settings =
                        mikmik_core::config::Settings::load_sync().unwrap_or_default();
                    if let Some(path) = selected_path.as_deref() {
                        let pattern = format!("{}*", path);
                        let _ = manager.add_persistent_allow_path(
                            &pending.request.tool_name,
                            &pattern,
                            &mut settings,
                        );
                    } else {
                        let _ =
                            manager.add_persistent_allow(&pending.request.tool_name, &mut settings);
                    }
                }
                _ => {}
            }
        }
    }

    let denied = matches!(decision, mikmik_core::permissions::PermissionDecision::Deny);
    if let Some(tx) = pending.decision_tx.take() {
        let _ = tx.send(decision);
    }
    Some(PermissionSettlement {
        tool_name: pending.request.tool_name.clone(),
        denied,
    })
}

/// The text of the last thing the model said, if it said anything.
///
/// Used as the body of the turn-complete notification, so that a user who
/// stepped away reads the answer without switching windows. A turn that ended
/// on a tool result or an empty message yields `None`, and the notification
/// carries only its title.
fn last_assistant_text(messages: &[mikmik_core::types::Message]) -> Option<String> {
    let last = messages
        .iter()
        .rev()
        .find(|message| message.role == mikmik_core::types::Role::Assistant)?;
    let text = match &last.content {
        mikmik_core::types::MessageContent::Text(text) => text.trim().to_string(),
        mikmik_core::types::MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                mikmik_core::types::ContentBlock::Text { text } => Some(text.trim()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    };
    (!text.is_empty()).then_some(text)
}

/// Derive the two goal surfaces from a single store read: the footer badge
/// string (only while the goal is still running) and whether the transcript
/// should mute the goal badge block.
///
/// Paused, budget-limited and absent goals clear the footer badge without
/// counting as complete, which is what makes the muted transcript block mean
/// "this goal is closed" rather than "this goal is not running right now".
fn goal_display_state(goal: Option<&mikmik_core::Goal>) -> (Option<String>, bool) {
    let badge = goal
        .filter(|goal| goal.status == mikmik_core::GoalStatus::Active)
        .map(|goal| {
            format!(
                "active · {} · {} turns",
                goal.elapsed_display(),
                goal.turns_used
            )
        });
    let completed = goal.is_some_and(|goal| goal.status == mikmik_core::GoalStatus::Complete);
    (badge, completed)
}

/// Run a `!command` line and render it for the transcript.
///
/// No permission is asked: the rules bound what the model may do, and this is
/// a command the user typed. The result goes back as a system annotation,
/// which `App` keeps out of `messages`, so nothing here reaches the model or
/// costs a token.
async fn run_bang_command(
    command: &str,
    tool_ctx: &mikmik_tools::ToolContext,
) -> (String, mikmik_tui::app::SystemMessageStyle) {
    let result = mikmik_tools::PtyBashTool
        .run_unprompted(command, tool_ctx)
        .await;
    let body = result.content.trim_end();
    let text = if body.is_empty() {
        format!("$ {command}")
    } else {
        format!("$ {command}\n{body}")
    };
    let style = if result.is_error {
        mikmik_tui::app::SystemMessageStyle::Warning
    } else {
        mikmik_tui::app::SystemMessageStyle::Info
    };
    (text, style)
}

/// Whether the plan badge belongs on screen for `mode`.
///
/// Both config-changing command arms derive the flag from the same place, so a
/// command that leaves plan mode through either one cannot leave the badge up
/// with nothing behind it.
fn plan_badge_for(mode: mikmik_core::config::PermissionMode) -> bool {
    matches!(mode, mikmik_core::config::PermissionMode::Plan)
}

async fn run_interactive(
    config: Config,
    settings: mikmik_core::config::Settings,
    settings_load_error: Option<String>,
    client: Arc<mikmik_api::AnthropicClient>,
    tools: Arc<Vec<Box<dyn mikmik_tools::Tool>>>,
    tool_ctx: ToolContext,
    query_config: mikmik_query::QueryConfig,
    cost_tracker: Arc<CostTracker>,
    resume_id: Option<String>,
    bridge_config: Option<mikmik_bridge::BridgeConfig>,
    has_credentials: bool,
    model_registry: Arc<mikmik_api::ModelRegistry>,
    user_question_rx: Option<tokio::sync::mpsc::UnboundedReceiver<mikmik_tools::UserQuestionEvent>>,
    plan_approval_rx: Option<tokio::sync::mpsc::UnboundedReceiver<mikmik_tools::PlanApprovalEvent>>,
    tool_output_rx: Option<tokio::sync::mpsc::UnboundedReceiver<mikmik_tools::ToolOutputChunk>>,
    plan_mode_rx: Option<tokio::sync::mpsc::UnboundedReceiver<mikmik_tools::EnterPlanModeEvent>>,
    pending_project_mcp: Vec<mikmik_core::config::McpServerConfig>,
    mcp_project_root: Option<PathBuf>,
    project_trust_pending: Option<mikmik_core::project_trust::GatedProjectSettings>,
    project_trust_root: Option<PathBuf>,
) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use mikmik_bridge::{BridgeOutbound, TuiBridgeEvent};
    use mikmik_commands::{execute_command, CommandContext, CommandResult};
    use mikmik_query::{QueryEvent, QueryOutcome};
    use mikmik_tui::{
        bridge_state::BridgeConnectionState, device_auth_dialog::DeviceAuthEvent,
        notifications::NotificationKind, render::render_app, restore_terminal, setup_terminal, App,
    };
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    let mut client = client;
    let mut model_registry = model_registry;
    let mut tool_ctx = tool_ctx;
    let mut resume_warning: Option<String> = None;
    let fresh_session = || {
        let mut session = mikmik_core::history::ConversationSession::new(session_model_string(
            &config,
            &model_registry,
        ));
        session.id = tool_ctx.session_id.clone();
        session.working_dir = Some(tool_ctx.working_dir.display().to_string());
        session
    };

    let mut session = match resolve_resume(resume_id.as_deref()).await {
        ResumeOutcome::Resumed(session) => {
            println!("Resumed session: {}", session.id);
            if let Some(saved_dir) = session.working_dir.as_ref() {
                let saved_path = std::path::PathBuf::from(saved_dir);
                if saved_path.exists() {
                    tool_ctx.working_dir = saved_path;
                }
            }
            tool_ctx.session_id = session.id.clone();
            *session
        }
        ResumeOutcome::NothingToResume => {
            resume_warning = Some("No previous sessions found, starting new session.".into());
            fresh_session()
        }
        ResumeOutcome::Failed(message) => {
            resume_warning = Some(format!("{message}. Starting new session."));
            fresh_session()
        }
        ResumeOutcome::NotRequested => fresh_session(),
    };
    let initial_messages = session.messages.clone();
    let mut base_query_config = query_config;
    // Goal autonomy is now an in-loop continuation policy (issue #230 / MI-3):
    // run_query_loop itself decides whether to continue toward an active goal
    // after each turn, instead of the REPL re-dispatching a fresh turn. Select
    // the goal policy for interactive user turns when the /goal feature is on;
    // the GoalPolicy is a no-op (stops after one turn) when no goal is active.
    if mikmik_core::goals_enabled() {
        base_query_config.continuation = mikmik_query::ContinuationMode::Goal;
    }
    let mut live_config = config.clone();
    if !session.model.is_empty() {
        live_config.model = Some(session.model.clone());
    }
    let pending_permissions = tool_ctx.pending_permissions.clone().unwrap_or_else(|| {
        Arc::new(ParkingMutex::new(
            mikmik_tools::PendingPermissionStore::default(),
        ))
    });

    // Appends each completed turn to the session's JSONL transcript, which
    // the welcome screen's recent activity, `/stats` and `/rewind` read.
    let transcript_root = mikmik_core::session_storage::transcript_root_for(&tool_ctx.working_dir);
    let mut transcript = mikmik_core::session_storage::TranscriptRecorder::new(
        transcript_root.clone(),
        session.id.clone(),
    );

    // A conditional rule speaks once. A resumed session has to know which ones
    // already did, or every one of them says its piece again about work that
    // is already finished.
    if let Ok(path) = mikmik_core::session_storage::transcript_path(&transcript_root, &session.id) {
        match mikmik_core::session_storage::rules_fired_in(&path).await {
            Ok(names) => mikmik_core::rules::mark_fired(&session.id, &names),
            Err(e) => tracing::debug!("could not read which rules already spoke: {e}"),
        }
    }

    // Set up terminal
    let mut terminal = setup_terminal(live_config.mouse_capture_enabled())?;
    let mut app = App::new(live_config.clone(), cost_tracker.clone());
    let (session_commands, skill_count) =
        session_slash_commands(&tool_ctx.working_dir, &live_config);
    app.set_extra_slash_commands(session_commands);
    app.skill_count = skill_count;
    if let Some(error) = settings_load_error {
        app.invalid_config_dialog =
            mikmik_tui::InvalidConfigDialogState::show_settings_error(&error);
    }
    // Gate input shift-normalization on whether the terminal speaks the kitty
    // keyboard protocol (detected in setup_terminal). On terminals that don't —
    // Windows conhost / CMD / legacy PowerShell, etc. — printable keys already
    // arrive as their final character, so re-shifting them would corrupt input
    // (issue #183: typing `/` produced `?`).
    app.kitty_keyboard_active = mikmik_tui::keyboard_enhancement_active();
    // The companion reads two files, so it is loaded once here rather than
    // per frame. `/buddy` reports a config change and the loop reloads it.
    app.reload_companion();
    // Seed the project-MCP approval queue: untrusted project servers that the
    // user must approve before they are allowed to launch (issue #123).
    app.session_id = session.id.clone();
    app.mcp_project_root = mcp_project_root;
    app.mcp_pending_project = pending_project_mcp.into_iter().collect();
    // The dialog asks the question; the settings live here, so the answer is
    // applied here. This copy is what gets installed if the user says yes.
    let mut project_gated = project_trust_pending.clone();
    app.project_trust_root = project_trust_root;
    app.project_trust_pending = project_trust_pending;
    if let Some(warning) = resume_warning {
        app.status_message = Some(warning);
    }
    // Sync initial effort level (from --effort flag or /effort command) to TUI indicator.
    // The TUI and query effort types are now the same canonical enum, so this is
    // a direct assignment.
    if let Some(level) = base_query_config.effort_level {
        app.set_effort_level(level);
    }
    app.provider_registry = base_query_config.provider_registry.clone();
    app.refresh_context_window_size();

    // Background: keep the model registry fresh from models.dev for the whole
    // TUI session (opencode-style: refresh now, then every ~60 min, gated by a
    // 5-min TTL). The fetched JSON is saved as a cache file; the App reloads it
    // from disk whenever the /model picker opens. Non-blocking.
    {
        spawn_models_cache_refresh_loop();
    }

    // Wire the ask-user question channel into the app so the TUI can show
    // the dialog and return an answer to the query loop.
    if let Some(rx) = user_question_rx {
        app.user_question_rx = Some(rx);
    }
    if let Some(rx) = plan_approval_rx {
        app.plan_approval_rx = Some(rx);
    }
    if let Some(rx) = tool_output_rx {
        app.tool_output_rx = Some(rx);
    }
    if let Some(rx) = plan_mode_rx {
        app.plan_mode_rx = Some(rx);
    }

    app.config.project_dir = Some(tool_ctx.working_dir.clone());
    app.attach_turn_diff_state(tool_ctx.file_history.clone(), tool_ctx.current_turn.clone());
    if let Some(manager) = tool_ctx.mcp_manager.clone() {
        app.attach_mcp_manager(manager);
    }
    app.replace_messages(initial_messages.clone());

    // Home directory warning: mirror TS feedConfigs.tsx warningText
    let home_dir = dirs::home_dir();
    if home_dir.as_deref() == Some(tool_ctx.working_dir.as_path()) {
        app.home_dir_warning = true;
    }

    // Bypass permissions confirmation dialog: must be accepted before any work
    // Mark whether valid credentials exist so the TUI can show a provider
    // setup dialog instead of failing silently on the first message.
    app.has_credentials = has_credentials;

    // Restore "always allow" bash prefixes persisted from earlier sessions so
    // previously-approved command prefixes don't prompt again.
    app.bash_prefix_allowlist
        .extend(settings.allowed_bash_prefixes.iter().cloned());

    // Set agent mode from the --agent flag (carried on query_config).
    if let Some(ref agent_name) = base_query_config.agent_name {
        app.agent_mode = Some(agent_name.clone());
    }

    // Mirror TS BypassPermissionsModeDialog.tsx startup gate
    // Shown as the highest-priority startup dialog (blocks all other UI).
    // Accepting persists `skipDangerousModePermissionPrompt` to settings.json
    // (TS parity), so the warning is a one-time gate — not re-shown on every
    // launch.
    use mikmik_core::config::PermissionMode;
    // The gate answers to one flag from here on, so the session loop can ask
    // the same question when the mode is switched later without re-reading the
    // settings file.
    app.bypass_gate_cleared = settings.skip_dangerous_mode_permission_prompt;
    if live_config.permission_mode == PermissionMode::BypassPermissions && !app.bypass_gate_cleared
    {
        app.bypass_permissions_dialog.show(true);
    } else if live_config.permission_mode != PermissionMode::BypassPermissions {
        // Show onboarding only if NOT in bypass-permissions mode.
        // Bypass dialog is a mandatory security gate and takes absolute priority.
        if !has_credentials {
            if !settings.has_completed_onboarding {
                app.onboarding_dialog.show();
            } else {
                app.status_message =
                    Some("No provider configured. Run /connect to set one up.".to_string());
            }
        } else if !settings.has_completed_onboarding {
            // User has credentials but hasn't formally completed onboarding — mark it done
            // silently so they never see it.
            let _ = mikmik_tui::App::persist_onboarding_complete_pub();
        }
    }

    // Version-upgrade notice: record the current version for future comparisons.
    // (Actual upgrade notice UI is handled by the release-notes slash command.)
    {
        let current_version = mikmik_core::constants::APP_VERSION.to_string();
        if settings.last_seen_version.as_deref() != Some(&current_version) {
            // Persist asynchronously to avoid blocking startup.
            let version_clone = current_version.clone();
            tokio::spawn(async move {
                if let Ok(mut s) = mikmik_core::config::Settings::load().await {
                    s.last_seen_version = Some(version_clone);
                    let _ = s.save().await;
                }
            });
        }
    }

    // External status line: a shell command from the user's settings whose
    // stdout is rendered above the footer. It runs when the session state
    // changes, so an idle session spawns nothing.
    let (status_line_tx, mut status_line_rx) = mpsc::channel::<String>(4);
    let status_line = live_config
        .status_line
        .as_ref()
        .and_then(|config| status_line::StatusLine::spawn(config, status_line_tx));
    let status_line_project_dir = tool_ctx.working_dir.display().to_string();
    let mut status_line_trigger: Option<status_line::TriggerKey> = None;
    let mut status_line_last_run = std::time::Instant::now();

    // Bridge runtime channels — Some when bridge is configured and started.
    //
    // tui_rx:       TUI-facing events from the bridge worker (connect/disconnect/prompts)
    // outbound_tx:  Forward query events to the bridge worker for upload to server
    // bridge_cancel: CancellationToken to stop the bridge worker task
    struct BridgeRuntime {
        tui_rx: mpsc::Receiver<TuiBridgeEvent>,
        outbound_tx: mpsc::Sender<BridgeOutbound>,
        cancel: CancellationToken,
    }

    // Id of the question the ask-user dialog is currently showing, so an answer
    // arriving over the bridge can be matched to it. `None` when no question is
    // open.
    let mut pending_question_id: Option<String> = None;

    // Same correlation for the project-MCP trust prompt, which has its own
    // dialog and settle path rather than going through `PermissionManager`.
    let mut pending_mcp_approval_id: Option<String> = None;
    // The bypass warning blocks everything while it is up, so a remote client
    // gets to answer it too. Same shape as the MCP prompt above.
    let mut pending_bypass_id: Option<String> = None;
    // Watched so a settings write reaches the ConfigChange hook exactly once.
    let mut settings_saves_seen = app.settings_screen.saves();

    // Remote prompts waiting for the session to be able to take one.
    //
    // Both inbound paths park here rather than starting a turn themselves.
    // They used to each carry their own copy of the spawn code, and the copies
    // had drifted: one started a second concurrent query on top of a running
    // one, the other dropped the prompt without a trace.
    let mut deferred_remote_prompts: std::collections::VecDeque<(
        String,
        Vec<mikmik_bridge::BridgeAttachment>,
    )> = std::collections::VecDeque::new();

    // Whether the client has already been told why its prompt is waiting. One
    // notice per spell of waiting; repeating it every frame would bury the
    // transcript.
    let mut deferred_notice_sent = false;

    // Whether the submit about to happen came from a remote client. Set when a
    // queued prompt is handed to the prompt box and consumed one iteration
    // later, where the synthesised Enter lands.
    let mut remote_submit = false;

    // Remaining segments of a `/a && /b` slash-command chain, fed one at a time
    // through the auto-submit path. Stop-on-error clears it: a failed segment
    // cancels the rest, matching shell `&&`.
    let mut command_chain: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    // Last busy state pushed to the remote client, so only transitions are
    // sent rather than one event per loop iteration.
    let mut bridge_busy_sent = false;

    // Last session facts pushed to the relay, so a re-registration only goes
    // out when one of them actually changed.
    let mut bridge_info_sent: Option<mikmik_bridge::SessionInfo> = None;

    // Preserve the bridge token before consuming bridge_config so we can reconstruct
    // a BridgeSessionInfo once the bridge worker reports it has connected.
    let bridge_token: Option<String> = bridge_config.as_ref().and_then(|c| c.session_token.clone());

    let mut bridge_runtime: Option<BridgeRuntime> = if let Some(cfg) = bridge_config {
        let bridge_cancel = CancellationToken::new();
        let (tui_tx, tui_rx) = mpsc::channel::<TuiBridgeEvent>(64);
        let (outbound_tx, outbound_rx) = mpsc::channel::<BridgeOutbound>(256);

        // Update TUI state to "connecting" before the task starts.
        app.bridge_state = BridgeConnectionState::Connecting;

        let cancel_clone = bridge_cancel.clone();
        tokio::spawn(async move {
            if let Err(e) =
                mikmik_bridge::run_bridge_loop(cfg, tui_tx, outbound_rx, cancel_clone).await
            {
                warn!("Bridge loop exited with error: {}", e);
            }
        });

        Some(BridgeRuntime {
            tui_rx,
            outbound_tx,
            cancel: bridge_cancel,
        })
    } else {
        None
    };

    // Relay channels for the BridgeSessionInfo-based event path.
    //
    // relay_ev_tx:    receives serialised JSON event payloads from the query-event
    //                 drain loop; a background task consumes them and calls
    //                 post_bridge_event so the web UI sees live streaming events.
    // relay_ev_rx_opt: Option wrapper so we can move the Receiver into the relay
    //                 task exactly once when the bridge session comes online.
    // remote_prompt_tx/rx: inbound user messages polled from poll_bridge_messages
    //                 are delivered here; the main loop injects them as query turns.
    let (relay_ev_tx, relay_ev_rx) = mpsc::channel::<String>(256);
    let mut relay_ev_rx_opt: Option<mpsc::Receiver<String>> = Some(relay_ev_rx);
    let (remote_prompt_tx, mut remote_prompt_rx) = mpsc::channel::<String>(32);

    // Once the bridge worker reports Connected we build this from the session
    // credentials so both relay tasks can POST/poll the /api/bridge/sessions API.
    let mut bridge_session_info: Option<std::sync::Arc<mikmik_bridge::BridgeSessionInfo>> = None;

    let mut messages = initial_messages;
    let mut cmd_ctx = CommandContext {
        config: live_config,
        // Refreshed from the app before every command; see the assignment
        // beside `cmd_ctx.messages`.
        context_window: app.context_window_size,
        context_used_tokens: app.context_used_tokens,
        cost_tracker: cost_tracker.clone(),
        messages: messages.clone(),
        working_dir: tool_ctx.working_dir.clone(),
        session_id: session.id.clone(),
        session_title: session.title.clone(),
        effort_level: app.effort_explicit.then_some(app.effort_level),
        remote_session_url: session.remote_session_url.clone(),
        mcp_manager: tool_ctx.mcp_manager.clone(),
        mcp_auth_runner: None,
        // Set per command below: a prompt that arrived from a remote client
        // has nobody at this terminal to read a view.
        interactive: true,
        // Kept in step with `base_query_config` wherever the agent mode changes.
        active_agent: base_query_config.agent_definition.clone(),
    };

    // tools is already Arc<Vec<...>> — share it across spawned tasks without copying.
    // Keep the full unfiltered tool set so agent-mode switching can re-filter.
    let all_tools_arc: Arc<Vec<Box<dyn mikmik_tools::Tool>>> = Arc::new(mikmik_tools::all_tools());
    let mut tools_arc = tools;

    // Current cancel token (replaced each turn)
    let mut cancel: Option<CancellationToken> = None;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<QueryEvent>();
    // The companion's line arrives on its own channel rather than as a
    // `QueryEvent`: it is not part of the turn, and a new `QueryEvent` variant
    // would also have to be given a meaning on the bridge, where the companion
    // does not exist.
    let (bubble_tx, mut bubble_rx) = mpsc::unbounded_channel::<String>();
    type MessagesArc = Arc<tokio::sync::Mutex<Vec<mikmik_core::types::Message>>>;
    let mut current_query: Option<(tokio::task::JoinHandle<QueryOutcome>, MessagesArc)> = None;
    // Background update check: spawned once at startup; result delivered via channel.
    let (update_tx, mut update_rx) = tokio::sync::mpsc::channel::<Option<String>>(1);
    tokio::spawn(async move {
        let info = mikmik_core::check_for_updates().await;
        let version = info.map(|i| i.latest_version);
        let _ = update_tx.send(version).await;
    });

    // Device code / OAuth auth channel — background tasks send events here
    // so the main loop can update the device_auth_dialog state.
    let (device_auth_tx, mut device_auth_rx) = mpsc::channel::<DeviceAuthEvent>(8);

    // MCP OAuth auth channel — background tasks send events here so the main
    // loop can update status and trigger a reconnect after browser auth finishes.
    enum McpAuthEvent {
        /// Browser auth completed and the token was persisted successfully.
        Completed(mikmik_mcp::oauth::McpAuthResult),
        /// Browser auth or token exchange failed.
        Failed(String),
    }
    let (mcp_auth_tx, mut mcp_auth_rx) = mpsc::channel::<McpAuthEvent>(8);
    // Build a non-blocking runner so `/mcp auth` can return immediately while
    // the browser flow continues in the background.
    let mcp_auth_runner: Arc<dyn Fn(mikmik_mcp::oauth::McpAuthSession) + Send + Sync> = {
        let tx = mcp_auth_tx.clone();
        Arc::new(move |session| {
            let tx = tx.clone();
            tokio::spawn(async move {
                let event = match mikmik_mcp::oauth::run_mcp_auth_session(session).await {
                    Ok(result) => McpAuthEvent::Completed(result),
                    Err(err) => McpAuthEvent::Failed(err.to_string()),
                };
                let _ = tx.send(event).await;
            });
        })
    };
    cmd_ctx.mcp_auth_runner = Some(mcp_auth_runner.clone());
    // Tracks the transcript scroll position between frames. When it changes we
    // force a full screen clear before the next draw: ratatui's incremental
    // diff keeps an internal model of the terminal, but ambiguous/wide glyphs
    // (`…`, `—`, `○`, …) that some terminals render two cells wide while
    // unicode-width counts one desync that model from reality, leaving ghost
    // fragments of scrolled-away lines. A physical clear (ESC[2J) on scroll
    // resyncs them; it's only issued while actively scrolling, so motion hides
    // any flash.
    let mut last_scroll_offset = app.scroll_offset;
    let mut last_auto_scroll = app.auto_scroll;
    // The terminal progress bar (OSC 9;4) is opt-out via the terminalProgressBar
    // setting; read it once at startup. `progress_shown` tracks whether we've
    // told the terminal we're "busy", so the escape is only emitted on an actual
    // streaming-state edge.
    let progress_bar_enabled = mikmik_core::config::Settings::load_sync()
        .map(|s| s.terminal_progress_bar)
        .unwrap_or(true);
    let mut progress_shown = false;

    // Start the project's language servers before anything asks, when the user
    // asked for that. A server indexes the whole project before it can answer,
    // and that wait otherwise lands on the first request. It runs in the
    // background: the session must not wait for it either.
    let (lsp_warmup_tx, mut lsp_warmup_rx) = mpsc::channel::<Vec<String>>(1);
    if app.config.effective_lsp_warmup_on_start() && app.config.effective_lsp_auto_detect() {
        let cwd = tool_ctx.working_dir.clone();
        let tx = lsp_warmup_tx.clone();
        tokio::spawn(async move {
            let started = mikmik_core::lsp::global_lsp_manager()
                .lock()
                .await
                .warm_up(&cwd)
                .await;
            if !started.is_empty() {
                let _ = tx.send(started).await;
            }
        });
    }

    'main: loop {
        app.frame_count = app.frame_count.wrapping_add(1);
        app.tick_mikmik_pose();
        app.notifications.tick();

        // The bypass warning, asked of the mode rather than of whoever set it.
        match bypass_gate_for(
            app.config.permission_mode,
            app.bypass_gate_cleared,
            app.bypass_permissions_dialog.visible,
        ) {
            BypassGate::RememberMode => app.mode_before_bypass = app.config.permission_mode,
            BypassGate::Warn => app.bypass_permissions_dialog.show(false),
            BypassGate::Nothing => {}
        }

        // Announce it once, however it was raised: the startup gate opens
        // before the bridge exists, so keying off the dialog rather than off
        // the gate covers both.
        if app.bypass_permissions_dialog.visible && pending_bypass_id.is_none() {
            let request_id = uuid::Uuid::new_v4().to_string();
            if let Some(runtime) = bridge_runtime.as_ref() {
                let _ = runtime.outbound_tx.try_send(bypass_warning_request(
                    &request_id,
                    app.bypass_permissions_dialog.at_startup,
                ));
            }
            pending_bypass_id = Some(request_id);
        }
        // Answered at the terminal. Drop the id so a late remote answer cannot
        // settle a warning already dealt with.
        if pending_bypass_id.is_some() && !app.bypass_permissions_dialog.visible {
            pending_bypass_id = None;
        }

        // Background loads the widgets ask for. Nothing else answers these
        // flags, so a missed call leaves the session browser empty and the
        // welcome screen's recent activity blank.
        app.pump_session_list();
        app.pump_recent_sessions();
        app.pump_usage();
        app.pump_stats();
        app.pump_branch_list();
        app.pump_voice_events();

        // Creating a branch writes a new session record, so the screen asks
        // and this loop does it, then switches to what it made.
        if let Some((name, at_message)) = app.pending_branch_create.take() {
            match mikmik_core::history::branch_session(&session.id, at_message, Some(&name)).await {
                Ok(branched) => {
                    app.status_message = Some(format!(
                        "Created branch \"{name}\" at message {at_message}."
                    ));
                    app.pending_resume_session_id = Some(branched.id);
                }
                Err(e) => app.push_notification(
                    mikmik_tui::NotificationKind::Error,
                    format!("Could not create the branch: {e}"),
                    None,
                ),
            }
        }

        if let Some(id) = app.pending_branch_delete.take() {
            if id == session.id {
                app.status_message = Some("The current branch cannot be deleted.".to_string());
            } else {
                match mikmik_core::history::delete_session(&id).await {
                    Ok(()) => {
                        app.status_message = Some("Branch deleted.".to_string());
                        app.branch_list_pending = app.session_branching.visible;
                    }
                    Err(e) => app.push_notification(
                        mikmik_tui::NotificationKind::Error,
                        format!("Could not delete the branch: {e}"),
                        None,
                    ),
                }
            }
        }

        // The session browser's Enter hands the id over rather than acting on
        // it, because swapping sessions moves state the TUI does not hold.
        if let Some(id) = app.pending_resume_session_id.take() {
            match mikmik_core::history::load_session(&id).await {
                Ok(resumed) => apply_session_resume(
                    resumed,
                    &mut session,
                    &mut messages,
                    &mut cmd_ctx,
                    &mut tool_ctx,
                    &mut app,
                    &mut transcript,
                ),
                Err(e) => {
                    app.push_notification(
                        mikmik_tui::NotificationKind::Error,
                        format!("Could not resume session: {e}"),
                        Some(8),
                    );
                }
            }
        }

        // Process file injection dialog outcome (if any)
        if let Some((outcome, pending_input, pending_imgs)) =
            app.file_injection_dialog.take_outcome()
        {
            use mikmik_tui::FileInjectionOutcome;

            if matches!(outcome, FileInjectionOutcome::Abort) {
                // Abort: input already restored to prompt by app.rs handler
                continue;
            }

            // InjectAll: bypass size limit on resubmission, restore stashed input+images,
            // then synthesize Enter to send immediately.
            app.file_injection_force = true;
            for img in pending_imgs {
                app.prompt_input.add_image(img);
            }
            app.set_prompt_text(pending_input);
            app.pending_auto_submit = true;
        }

        // If the transcript scrolled since the last frame, force a full screen
        // clear so wide/ambiguous-glyph desync can't leave ghost fragments of
        // scrolled-away lines (see note at the top of the loop).
        if app.scroll_offset != last_scroll_offset || app.auto_scroll != last_auto_scroll {
            let _ = terminal.clear();
            last_scroll_offset = app.scroll_offset;
            last_auto_scroll = app.auto_scroll;
        }

        // Draw the UI, and scan the frame that was just painted for URL runs.
        // ratatui swaps its two buffers at the end of draw(), so by the time
        // draw() returns `terminal.current_buffer_mut()` points at the empty
        // next-frame slot; `CompletedFrame.buffer` is the one that holds what
        // the user is looking at.
        let osc8_hits = {
            let completed = terminal.draw(|f| render_app(f, &app))?;
            mikmik_tui::osc8::scan_buffer_for_urls(completed.buffer)
        };

        // Re-emit those cells wrapped in hyperlink escapes so terminals that
        // support OSC 8 make them clickable. A failed write is never worth
        // killing the TUI over.
        if let Err(err) = mikmik_tui::osc8::emit_hits(&osc8_hits) {
            tracing::debug!(target: "osc8", "hyperlink overlay write failed: {err}");
        }

        // Level-sync the terminal progress indicator (OSC 9;4) to streaming
        // state, so supporting terminals (iTerm2, WezTerm, Windows Terminal, …)
        // show a "working" bar while a turn is active and clear it when idle.
        let want_progress = app.is_streaming && progress_bar_enabled;
        if want_progress != progress_shown {
            mikmik_tui::set_terminal_progress(want_progress);
            progress_shown = want_progress;
        }

        // The warmup says which servers came up, once.
        while let Ok(started) = lsp_warmup_rx.try_recv() {
            app.push_notification(
                mikmik_tui::NotificationKind::Info,
                format!("Language server ready: {}", started.join(", ")),
                None,
            );
        }

        // Feed the next segment of a slash-command chain once the session is
        // idle. `any_modal_open` is the same gate a plain Enter submit uses, so
        // the chain waits for a segment's overlay, picker or turn to finish
        // before the next runs; the empty-prompt check keeps it from
        // overwriting text the user is typing.
        if !command_chain.is_empty()
            && !app.pending_auto_submit
            && !app.is_streaming
            && !app.any_modal_open()
            && app.prompt_input.text.is_empty()
        {
            if let Some(next) = command_chain.pop_front() {
                app.set_prompt_text(next);
                app.pending_auto_submit = true;
            }
        }

        // Poll for crossterm events (keyboard/mouse) with short timeout
        // unless an auto-submit (queued message) is pending — in which case
        // synthesize an Enter event to dequeue and submit it.
        let synthetic_event: Option<Event> = if let Some(k) = app.pending_key.take() {
            // A non-character key swallowed by the paste-burst drain — replay
            // it so the keystroke that ended a raw-key paste is not lost.
            Some(Event::Key(k))
        } else if app.pending_auto_submit && !app.is_streaming {
            app.pending_auto_submit = false;
            Some(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )))
        } else {
            None
        };

        // Repaint cadence. The loop already polls at ~60fps for the streaming
        // spinner; the effort picker's animated ultracode spectrum can be open
        // while idle, so cap the poll interval to at least ~30fps whenever it is
        // showing (frame_count advances every draw, moving the spectrum). This is
        // a no-op unless the base cadence is ever relaxed, and it does NOT tick
        // faster when the picker is closed.
        let poll_timeout = if app.effort_picker.wants_animation() {
            Duration::from_millis(16).min(Duration::from_millis(33))
        } else {
            Duration::from_millis(16)
        };
        let evt_opt: Option<Event> = if let Some(e) = synthetic_event {
            Some(e)
        } else if crossterm::event::poll(poll_timeout)? {
            Some(event::read()?)
        } else {
            None
        };

        if let Some(evt) = evt_opt {
            match evt {
                Event::Key(key) => {
                    // On Windows crossterm emits Press + Release for a single key.
                    // Only process Press to avoid double-registering input.
                    if key.kind != crossterm::event::KeyEventKind::Press {
                        // The one exception: push-to-talk stops when the key
                        // comes back up, which is a Release and nothing else.
                        // Kept as narrow as it can be — letting any other
                        // Release through would process that key twice.
                        if key.kind == crossterm::event::KeyEventKind::Release
                            && key.code == KeyCode::Char('v')
                            && key.modifiers == KeyModifiers::NONE
                            && app.voice_recording
                            && app.voice_recorder.is_some()
                        {
                            app.handle_voice_ptt_stop();
                        }
                        continue;
                    }

                    // Ctrl+C and Ctrl+D: exit confirmation handling
                    if handle_exit_key(&mut app, key, &cancel) {
                        if app.should_exit {
                            break 'main;
                        }
                        continue;
                    }

                    // ── Paste-burst detection ─────────────────────────────
                    // Terminals without bracketed paste (notably Windows
                    // Ctrl+V, some tmux configs) dump the clipboard as raw
                    // key events: every pasted newline arrives as Enter and
                    // would submit a truncated prompt. A zero-timeout drain
                    // right after the first character captures the whole
                    // flood as one paste (human typing never queues 2+ chars
                    // in the same instant). Must run BEFORE the Enter/submit
                    // handling below so pasted newlines can't submit.
                    if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT {
                        if let KeyCode::Char(c) = key.code {
                            if app.paste_burst_allowed() {
                                if let Some(burst) = app.try_detect_paste_burst(c) {
                                    app.handle_paste_data(burst);
                                    app.refresh_prompt_input();
                                    continue;
                                }
                            }
                        }
                    }

                    // Enter => submit input (but NOT when ANY dialog/overlay is open —
                    // dialogs handle their own Enter in handle_key_event).
                    let any_dialog_open = app.any_modal_open();
                    // Only a bare Enter submits/queues. A *modified* Enter
                    // (Shift/Alt/Ctrl+Enter) means "insert a newline" and must
                    // fall through to app.handle_key_event below, which inserts
                    // it into the prompt buffer. Ctrl+J (a non-Enter key) is the
                    // other newline escape and never reaches these branches.
                    // (issue #224 — Shift+Enter inserts a newline; Enter submits.)
                    let plain_enter = key.code == KeyCode::Enter
                        && !key.modifiers.contains(KeyModifiers::SHIFT)
                        && !key.modifiers.contains(KeyModifiers::ALT)
                        && !key.modifiers.contains(KeyModifiers::CONTROL);
                    if plain_enter && app.is_streaming && !any_dialog_open {
                        // Queue the message: it will auto-submit once the
                        // current turn finishes (issue #149).
                        let input = app.take_input();
                        if mikmik_tui::input::is_bang_command(&input) {
                            // A queued bang would reach the model as a message
                            // once the turn ended, which is the one thing this
                            // path exists to avoid. Hand the text back instead.
                            app.set_prompt_text(input);
                            app.notifications.push(
                                mikmik_tui::NotificationKind::Warning,
                                "Shell commands wait for the turn to finish.".to_string(),
                                Some(3),
                            );
                            continue;
                        }
                        if !input.is_empty() {
                            let preview: String = input.chars().take(40).collect();
                            app.queued_messages.push_back(input);
                            let total = app.queued_messages.len();
                            app.notifications.push(
                                mikmik_tui::NotificationKind::Info,
                                format!("Queued ({}): {}", total, preview),
                                Some(3),
                            );
                        }
                        continue;
                    }
                    if plain_enter && !app.is_streaming && !any_dialog_open {
                        // If a file-ref suggestion is active, accept it instead of submitting.
                        if app
                            .prompt_input
                            .suggestion_index
                            .and_then(|index| app.prompt_input.suggestions.get(index))
                            .is_some_and(|suggestion| {
                                suggestion.source
                                    == mikmik_tui::prompt_input::TypeaheadSource::FileRef
                            })
                        {
                            app.prompt_input.accept_suggestion();
                            app.prompt_input.insert_char(' ');
                            app.refresh_prompt_input();
                            continue;
                        }
                        // If a slash-command suggestion is active, accept and execute immediately.
                        if !app.prompt_input.suggestions.is_empty()
                            && app.prompt_input.suggestion_index.is_some()
                            && app.prompt_input.text.starts_with('/')
                        {
                            app.prompt_input.accept_suggestion();
                            // Fall through to submit — no second Enter needed
                        }

                        let mut input = app.take_input();
                        if input.is_empty() {
                            continue;
                        }

                        // Command chaining: a slash line `/a && /b` runs each in
                        // turn. Split only slash input — a bang line's `&&` is
                        // shell syntax and stays intact. Run the first segment
                        // now; the rest wait in `command_chain` and are fed one
                        // at a time by the drain at the top of the loop. Guard on
                        // an empty chain so an auto-submitted segment is not
                        // re-split.
                        if command_chain.is_empty() && mikmik_tui::input::is_slash_command(&input) {
                            let mut segments = mikmik_tui::input::split_command_chain(&input);
                            if segments.len() > 1 {
                                input = segments.remove(0);
                                command_chain.extend(segments);
                            }
                        }

                        // Check for a shell command to run here, before the
                        // slash check: a bang line never reaches the model.
                        if mikmik_tui::input::is_bang_command(&input) {
                            let command = mikmik_tui::input::parse_bang_command(&input).to_string();
                            if command.is_empty() {
                                app.set_prompt_text(input);
                                app.status_message =
                                    Some("A bang with no command runs nothing.".to_string());
                                continue;
                            }
                            if app.plan_mode {
                                app.set_prompt_text(input);
                                app.status_message = Some(
                                    "Plan mode touches nothing, shell commands included."
                                        .to_string(),
                                );
                                continue;
                            }
                            let (text, style) = run_bang_command(&command, &tool_ctx).await;
                            app.push_system_message(text, style);
                            continue;
                        }

                        // Check for slash command
                        if input.starts_with('/') {
                            let (cmd_name, cmd_args) =
                                mikmik_tui::input::parse_slash_command(&input);
                            let cmd_name = cmd_name.to_string();
                            let cmd_args = cmd_args.to_string();

                            // A command's outcome reaches the terminal through
                            // `status_message` and nothing else, so a remote
                            // client sees silence. Watch the field across the
                            // whole block instead of emitting from each arm:
                            // an arm added later is then covered without
                            // anyone remembering to wire it up.
                            let status_before = app.status_message.clone();
                            let mut command_failed = false;
                            // Taken, not read: the flag is set one iteration
                            // earlier by the queue drain and must not survive
                            // into whatever the operator types next.
                            let from_remote = std::mem::take(&mut remote_submit);
                            // Raised by any arm that swaps the conversation out
                            // from under the client, which then has to be told
                            // what the transcript is now.
                            let mut transcript_replaced = false;

                            // ── Step 1: TUI-layer intercept (overlays, toggles) ────────
                            // Run first so we know whether a UI overlay opened, which
                            // lets us suppress redundant CLI text output below.
                            //
                            // Skip TUI overlay for arg-bearing commands where the user
                            // wants to SET state, not browse a picker:
                            //   /model claude-haiku  → set model, don't open picker
                            //   /theme dark          → set theme, don't open picker
                            //   /resume <id>         → load session, don't open browser
                            // Also skip TUI for /vim, /voice, /fast with explicit
                            // on|off args so the blind-toggle doesn't misfire.
                            let skip_tui_for_args = !cmd_args.is_empty()
                                && matches!(
                                    cmd_name.as_str(),
                                    "model"
                                        | "theme"
                                        | "effort"
                                        | "resume"
                                        | "session"
                                        | "vim"
                                        | "vi"
                                        | "voice"
                                        | "fast"
                                        | "speed"
                                );
                            // A view opened for someone who is not at the
                            // keyboard helps nobody: they cannot see it, and
                            // the command layer's text answer is thrown away
                            // to make room for it. Let that text through
                            // instead.
                            let remote_wants_text =
                                from_remote && mikmik_tui::App::opens_terminal_view(&cmd_name);
                            let handled_by_tui = if skip_tui_for_args || remote_wants_text {
                                false
                            } else {
                                app.intercept_slash_command_with_args(&cmd_name, &cmd_args)
                            };

                            // Honour exit/quit triggered by TUI intercept immediately.
                            if app.should_exit {
                                break 'main;
                            }

                            // ── Step 2: CLI-layer (real side effects) ──────────────────
                            // Handles: config changes, session ops, file I/O, OAuth, etc.
                            // Always runs — some commands need BOTH (e.g. /clear clears
                            // app state via TUI AND the messages vec via CLI).
                            cmd_ctx.messages = messages.clone();
                            // Whoever sent this is not at the keyboard, so a
                            // command that would open a view answers in text.
                            cmd_ctx.interactive = !from_remote;
                            // The app owns the level; a picker can have moved
                            // it since the last command ran.
                            cmd_ctx.effort_level = app.effort_explicit.then_some(app.effort_level);
                            // The app owns both context figures too. The window
                            // changes when the model changes, and the measured
                            // fill changes every turn, so reading them once at
                            // construction would report the first turn forever.
                            cmd_ctx.context_window = app.context_window_size;
                            cmd_ctx.context_used_tokens = app.context_used_tokens;
                            let cli_result = execute_command(&input, &mut cmd_ctx).await;
                            // Start optimistically true; set false for Silent/None below.
                            let mut handled_by_cli = cli_result.is_some();

                            // Whether we need to fall through and submit a user message.
                            let mut submit_user_msg: Option<String> = None;

                            match cli_result {
                                Some(CommandResult::Exit) => break 'main,
                                Some(CommandResult::ClearConversation) => {
                                    messages.clear();
                                    app.replace_messages(Vec::new());
                                    session.messages.clear();
                                    session.updated_at = chrono::Utc::now();
                                    transcript.reset_branch();
                                    // The shell too. `/clear` means start again,
                                    // and a `cd` or an exported variable that
                                    // survived it is state the user asked to be
                                    // rid of. The next Bash call opens a fresh
                                    // shell in the working directory.
                                    mikmik_tools::clear_session_shell_state(&session.id);
                                    app.status_message = Some("Conversation cleared.".to_string());
                                    transcript_replaced = true;
                                }
                                Some(CommandResult::NewSession) => {
                                    // Fresh lazy-home session (opencode /new):
                                    // preserve the current model / provider / effort
                                    // selection and working directory. The new
                                    // session is only persisted once the first
                                    // message completes a turn, matching opencode's
                                    // lazy-session semantics.
                                    let model =
                                        session_model_string(&cmd_ctx.config, &model_registry);
                                    session = mikmik_commands::build_home_session(
                                        model,
                                        Some(tool_ctx.working_dir.display().to_string()),
                                    );
                                    messages.clear();
                                    app.replace_messages(Vec::new());
                                    tool_ctx.session_id = session.id.clone();
                                    cmd_ctx.session_id = session.id.clone();
                                    cmd_ctx.session_title = None;
                                    app.session_id = session.id.clone();
                                    transcript.rebind(
                                        mikmik_core::session_storage::transcript_root_for(
                                            &tool_ctx.working_dir,
                                        ),
                                        session.id.clone(),
                                    );
                                    // Reset per-turn diff/turn bookkeeping, as
                                    // ResumeSession does when swapping sessions.
                                    tool_ctx.file_history = Arc::new(ParkingMutex::new(
                                        mikmik_core::file_history::FileHistory::new(),
                                    ));
                                    tool_ctx.current_turn =
                                        Arc::new(std::sync::atomic::AtomicUsize::new(0));
                                    app.attach_turn_diff_state(
                                        tool_ctx.file_history.clone(),
                                        tool_ctx.current_turn.clone(),
                                    );
                                    mikmik_tui::update_terminal_title(None);
                                    app.status_message = Some("Started a new session.".to_string());
                                    transcript_replaced = true;
                                }
                                Some(CommandResult::MoveSession {
                                    destination,
                                    moved_changes,
                                }) => {
                                    // Re-home the live session to the destination
                                    // worktree (opencode /move). The working-tree
                                    // changes were already relocated inside the
                                    // command; here we repoint every cwd-aware
                                    // surface so tools, the system prompt and the
                                    // saved session all track the new location.
                                    tool_ctx.working_dir = destination.clone();
                                    cmd_ctx.working_dir = destination.clone();
                                    cmd_ctx.config.project_dir = Some(destination.clone());
                                    tool_ctx.config.project_dir = Some(destination.clone());
                                    app.config.project_dir = Some(destination.clone());
                                    base_query_config.working_directory =
                                        Some(destination.display().to_string());
                                    session.working_dir = Some(destination.display().to_string());
                                    session.updated_at = chrono::Utc::now();
                                    if let Err(e) =
                                        mikmik_core::history::save_session(&session).await
                                    {
                                        app.push_notification(
                                            mikmik_tui::NotificationKind::Error,
                                            format!("Could not save the session: {e}"),
                                            None,
                                        );
                                    }
                                    mikmik_plugins::run_global_hook(
                                        mikmik_plugins::HookEventKind::CwdChanged,
                                        None,
                                        serde_json::json!({
                                            "working_dir": destination.display().to_string(),
                                        }),
                                    )
                                    .await;
                                    // NOTE: opencode appends a synthetic
                                    // <system-reminder> prompt after a move. mikmik
                                    // re-derives working_directory into every turn's
                                    // system prompt (qcfg.working_directory below),
                                    // so repointing tool_ctx.working_dir already
                                    // tells the model on its next turn — we skip the
                                    // dangling user message that would otherwise
                                    // break user/assistant role alternation.
                                    let carried = if moved_changes {
                                        " (carried over uncommitted changes)"
                                    } else {
                                        ""
                                    };
                                    app.status_message = Some(format!(
                                        "Moved session to {}{}",
                                        destination.display(),
                                        carried
                                    ));
                                }
                                Some(CommandResult::SetMessages(new_msgs)) => {
                                    let removed = messages.len().saturating_sub(new_msgs.len());
                                    messages = new_msgs.clone();
                                    app.replace_messages(new_msgs);
                                    session.messages = messages.clone();
                                    session.updated_at = chrono::Utc::now();
                                    // The transcript keeps what was dropped on
                                    // a sibling branch; its tip has to follow
                                    // the conversation or the next launch
                                    // reloads the turns just rewound away.
                                    let tip = messages.last().and_then(|m| m.uuid.clone());
                                    if let Err(e) = transcript.set_active_leaf(tip.as_deref()).await
                                    {
                                        app.push_notification(
                                            mikmik_tui::NotificationKind::Error,
                                            format!("Could not move the transcript's tip: {e}"),
                                            None,
                                        );
                                    }
                                    app.status_message = Some(format!(
                                        "Rewound {} message{}.",
                                        removed,
                                        if removed == 1 { "" } else { "s" }
                                    ));
                                    transcript_replaced = true;
                                }
                                Some(CommandResult::RetryInterrupted) => {
                                    // Drop the aborted turn (from its user prompt
                                    // onward) and resubmit that prompt, so the
                                    // model answers it again from the start.
                                    if app.is_streaming {
                                        app.status_message = Some(
                                            "Wait for the current turn to finish before retrying."
                                                .to_string(),
                                        );
                                    } else if let Some((truncated, prompt)) = app.plan_retry() {
                                        messages = truncated.clone();
                                        app.replace_messages(truncated);
                                        session.messages = messages.clone();
                                        session.updated_at = chrono::Utc::now();
                                        let tip = messages.last().and_then(|m| m.uuid.clone());
                                        if let Err(e) =
                                            transcript.set_active_leaf(tip.as_deref()).await
                                        {
                                            app.push_notification(
                                                mikmik_tui::NotificationKind::Error,
                                                format!("Could not move the transcript's tip: {e}"),
                                                None,
                                            );
                                        }
                                        transcript_replaced = true;
                                        submit_user_msg = Some(prompt);
                                    } else {
                                        app.status_message = Some("Nothing to retry.".to_string());
                                    }
                                }
                                Some(CommandResult::RunCompaction { instruction }) => {
                                    transcript_replaced |= compact_conversation(
                                        instruction.as_deref(),
                                        &mut messages,
                                        &mut app,
                                        &mut session,
                                        &mut transcript,
                                        &cmd_ctx.config,
                                        client.as_ref(),
                                        base_query_config.provider_registry.as_ref(),
                                        &model_registry,
                                        &tool_ctx.session_id,
                                    )
                                    .await;
                                }
                                Some(CommandResult::OpenInEditor { path, message }) => {
                                    // Same handover the plan dialog's ctrl+g
                                    // makes: an editor started under the TUI
                                    // draws over the frame and is redrawn on
                                    // the next one.
                                    if !path.exists() {
                                        if let Some(parent) = path.parent() {
                                            let _ = std::fs::create_dir_all(parent);
                                        }
                                        let _ = std::fs::write(&path, "");
                                    }
                                    let (editor, editor_hint) =
                                        mikmik_core::paths::preferred_editor();
                                    mikmik_tui::restore_terminal(&mut terminal).ok();
                                    let status =
                                        std::process::Command::new(&editor).arg(&path).status();
                                    terminal = mikmik_tui::setup_terminal(
                                        app.config.mouse_capture_enabled(),
                                    )?;
                                    app.kitty_keyboard_active =
                                        mikmik_tui::keyboard_enhancement_active();
                                    app.status_message = Some(match status {
                                        Ok(_) => message,
                                        Err(e) => format!(
                                            "Could not launch '{editor}': {e}. \
                                             Edit {} yourself. {editor_hint}",
                                            path.display()
                                        ),
                                    });
                                }
                                Some(CommandResult::OpenRewindOverlay) => {
                                    app.replace_messages(messages.clone());
                                    app.open_rewind_flow();
                                    app.status_message =
                                        Some("Select a message to rewind to.".to_string());
                                }
                                Some(CommandResult::ReloadPlugins) => {
                                    let previous = mikmik_plugins::global_plugin_registry();
                                    let registry =
                                        mikmik_plugins::load_plugins(&tool_ctx.working_dir, &[])
                                            .await;
                                    let diff = previous
                                        .as_ref()
                                        .map(|old| registry.diff_against(old))
                                        .unwrap_or_default();

                                    mikmik_plugins::set_global_hooks(
                                        registry.build_hook_registry(),
                                    );
                                    // Every cwd-aware copy of the config, or a
                                    // contribution would reach one surface and
                                    // not the next.
                                    let previous_ref = previous.as_deref();
                                    apply_plugin_contributions(
                                        &registry,
                                        previous_ref,
                                        &mut cmd_ctx.config,
                                    );
                                    apply_plugin_contributions(
                                        &registry,
                                        previous_ref,
                                        &mut tool_ctx.config,
                                    );
                                    apply_plugin_contributions(
                                        &registry,
                                        previous_ref,
                                        &mut app.config,
                                    );
                                    let summary =
                                        mikmik_plugins::format_reload_summary(&registry, &diff);
                                    // Reconnecting drops every live MCP session,
                                    // and it reports its own outcome over this
                                    // summary, so it happens only when the set of
                                    // plugin servers actually moved. One that the
                                    // reload added still passes the trust gate on
                                    // that path.
                                    let mcp_changed = previous_ref
                                        .map(|old| {
                                            plugin_mcp_names(old) != plugin_mcp_names(&registry)
                                        })
                                        .unwrap_or(false);
                                    mikmik_plugins::set_global_registry(registry);

                                    let (session_commands, skill_count) = session_slash_commands(
                                        &tool_ctx.working_dir,
                                        &cmd_ctx.config,
                                    );
                                    app.set_extra_slash_commands(session_commands);
                                    app.skill_count = skill_count;
                                    if mcp_changed {
                                        app.pending_mcp_reconnect = true;
                                    }
                                    app.status_message = Some(summary);
                                }
                                Some(CommandResult::OpenHooksOverlay) => {
                                    // Open the 4-screen hooks configuration browser.
                                    // intercept_slash_command("hooks") already does this
                                    // when the user types /hooks in the TUI prompt, so
                                    // this branch only triggers when the command returns
                                    // the variant explicitly (e.g. from a non-prompt context).
                                    app.hooks_config_menu.open();
                                    app.status_message =
                                        Some("Hooks configuration browser".to_string());
                                }
                                Some(CommandResult::OpenImportConfigOverlay) => {
                                    app.open_import_config_picker();
                                    app.status_message =
                                        Some("Select what to import from ~/.claude.".to_string());
                                }
                                Some(CommandResult::ResumeSession(resumed_session)) => {
                                    apply_session_resume(
                                        resumed_session,
                                        &mut session,
                                        &mut messages,
                                        &mut cmd_ctx,
                                        &mut tool_ctx,
                                        &mut app,
                                        &mut transcript,
                                    );
                                    transcript_replaced = true;
                                }
                                Some(CommandResult::RenameSession(title)) => {
                                    apply_session_rename(
                                        title,
                                        &mut session,
                                        &mut cmd_ctx,
                                        &mut app,
                                        &mut transcript,
                                    )
                                    .await;
                                }
                                Some(CommandResult::SyncAccountModels { accounts, force }) => {
                                    // Default to every configured account, so
                                    // `/providers sync` on its own does the
                                    // obvious thing.
                                    let targets: Vec<String> = if accounts.is_empty() {
                                        let mut all: Vec<String> = cmd_ctx
                                            .config
                                            .provider_configs
                                            .keys()
                                            .cloned()
                                            .collect();
                                        all.sort();
                                        all
                                    } else {
                                        accounts
                                    };

                                    let unknown: Vec<&String> = targets
                                        .iter()
                                        .filter(|id| {
                                            !cmd_ctx.config.provider_configs.contains_key(*id)
                                        })
                                        .collect();

                                    if targets.is_empty() {
                                        app.status_message = Some(
                                            "No accounts configured. Add one with /connect."
                                                .to_string(),
                                        );
                                    } else if let Some(missing) = unknown.first() {
                                        app.status_message = Some(format!(
                                            "No account named '{missing}'. Configured: {}.",
                                            cmd_ctx
                                                .config
                                                .provider_configs
                                                .keys()
                                                .cloned()
                                                .collect::<Vec<_>>()
                                                .join(", ")
                                        ));
                                    } else {
                                        for account in &targets {
                                            app.queue_model_sync(account, force);
                                        }
                                        app.status_message =
                                            Some(format!("Syncing {}…", targets.join(", ")));
                                    }
                                }
                                Some(CommandResult::RefreshProviderState) => {
                                    if app.is_streaming || current_query.is_some() {
                                        app.status_message = Some(
                                            "Wait for the current response to finish before running /refresh."
                                                .to_string(),
                                        );
                                    } else {
                                        match refresh_provider_runtime_state(&cmd_ctx.config).await
                                        {
                                            Ok(refreshed) => {
                                                cmd_ctx.config = refreshed.config.clone();
                                                tool_ctx.config = refreshed.config.clone();
                                                base_query_config.provider_registry =
                                                    Some(refreshed.provider_registry.clone());
                                                base_query_config.model_registry =
                                                    Some(refreshed.model_registry.clone());
                                                base_query_config.model = session_model_string(
                                                    &cmd_ctx.config,
                                                    refreshed.model_registry.as_ref(),
                                                );
                                                client = refreshed.client;
                                                model_registry = refreshed.model_registry;
                                                session.model = session_model_string(
                                                    &cmd_ctx.config,
                                                    model_registry.as_ref(),
                                                );
                                                session.updated_at = chrono::Utc::now();
                                                app.apply_provider_refresh(
                                                    refreshed.config,
                                                    Some(refreshed.provider_registry),
                                                    refreshed.auth_store,
                                                    false,
                                                    "Saved provider state cleared. Run /connect to reconnect."
                                                        .to_string(),
                                                );
                                            }
                                            Err(err) => {
                                                app.status_message =
                                                    Some(format!("Error: {}", err));
                                            }
                                        }
                                    }
                                }
                                Some(CommandResult::McpAuthFlow {
                                    server_name,
                                    auth_url,
                                    redirect_uri,
                                }) => {
                                    app.status_message = Some(format!(
                                        "MCP OAuth — '{}' started. Complete authentication in your browser.\nURL: {}\nCallback URL: {}",
                                        server_name, auth_url, redirect_uri
                                    ));
                                }
                                Some(CommandResult::Message(msg)) => {
                                    // Suppress text output when TUI already opened an
                                    // overlay for this command (e.g. /stats opens dialog
                                    // AND would push a text message — drop the text).
                                    if !handled_by_tui {
                                        // The remote client sees only query events, so
                                        // without this a command it sent appears to do
                                        // nothing at all.
                                        if let Some(runtime) = bridge_runtime.as_ref() {
                                            let id = format!("cmd-{}", uuid::Uuid::new_v4());
                                            let _ = runtime.outbound_tx.try_send(
                                                BridgeOutbound::TextDelta {
                                                    delta: msg.clone(),
                                                    message_id: id.clone(),
                                                },
                                            );
                                            let _ = runtime.outbound_tx.try_send(
                                                BridgeOutbound::TurnComplete {
                                                    message_id: id,
                                                    stop_reason: "command".to_string(),
                                                    // A command reply is not a
                                                    // model turn and spends no
                                                    // tokens.
                                                    usage: None,
                                                },
                                            );
                                        }
                                        app.push_message(mikmik_core::types::Message::assistant(
                                            msg,
                                        ));
                                    }
                                }
                                Some(CommandResult::ConfigChange(new_cfg)) => {
                                    let mut applied_cfg = new_cfg;
                                    normalize_provider_from_model(&mut applied_cfg);
                                    cmd_ctx.config = applied_cfg.clone();
                                    tool_ctx.config = applied_cfg.clone();
                                    app.config = applied_cfg.clone();
                                    // Sync model/provider shown in the TUI header.
                                    if let Some(ref model) = applied_cfg.model {
                                        app.set_model(model.clone());
                                    }
                                    // Sync fast_mode visual indicator.
                                    app.fast_mode =
                                        is_fast_mode_model(&applied_cfg, &model_registry);
                                    // Sync plan_mode visual indicator.
                                    app.plan_mode = plan_badge_for(applied_cfg.permission_mode);
                                    app.reload_companion();
                                    session.model =
                                        session_model_string(&cmd_ctx.config, &model_registry);
                                    app.status_message = Some("Configuration updated.".to_string());
                                }
                                Some(CommandResult::ConfigChangeMessage(new_cfg, msg)) => {
                                    let mut applied_cfg = new_cfg;
                                    normalize_provider_from_model(&mut applied_cfg);
                                    cmd_ctx.config = applied_cfg.clone();
                                    tool_ctx.config = applied_cfg.clone();
                                    // Sync model/provider + fast_mode visual indicator.
                                    if let Some(ref model) = applied_cfg.model {
                                        app.set_model(model.clone());
                                    }
                                    // A `None` model means the default is back,
                                    // which is never the fast one.
                                    app.fast_mode =
                                        is_fast_mode_model(&applied_cfg, &model_registry);
                                    app.config = applied_cfg.clone();
                                    // Same sync the `Config` arm above does.
                                    app.plan_mode = plan_badge_for(applied_cfg.permission_mode);
                                    // `/permissions allow|deny` writes a rule to
                                    // settings.json and holds no manager, so the
                                    // running turn would keep deciding by the
                                    // rules it started with. Same problem
                                    // `sync_permission_mode` solves for the mode.
                                    if let (Some(manager), Ok(settings)) = (
                                        tool_ctx.permission_manager.as_ref(),
                                        mikmik_core::Settings::load_sync(),
                                    ) {
                                        if let Ok(mut manager) = manager.lock() {
                                            manager.reload_persistent_rules(&settings);
                                        }
                                    }
                                    app.reload_companion();
                                    session.model =
                                        session_model_string(&cmd_ctx.config, &model_registry);
                                    app.status_message = Some(msg);
                                }
                                Some(CommandResult::UserMessage(msg)) => {
                                    // Queue a user-visible turn for the model.
                                    submit_user_msg = Some(msg);
                                }
                                Some(CommandResult::StartOAuthFlow(with_claude_ai)) => {
                                    mikmik_tui::restore_terminal(&mut terminal).ok();
                                    match oauth_flow::run_oauth_login_flow(with_claude_ai).await {
                                        Ok(_) => {
                                            app.status_message =
                                                Some("Login successful!".to_string());
                                            eprintln!(
                                                "\nLogin successful! Please restart \
                                                 claude to use the new credentials."
                                            );
                                            break 'main;
                                        }
                                        Err(e) => {
                                            eprintln!("\nLogin failed: {}", e);
                                        }
                                    }
                                    terminal = mikmik_tui::setup_terminal(
                                        app.config.mouse_capture_enabled(),
                                    )?;
                                    app.kitty_keyboard_active =
                                        mikmik_tui::keyboard_enhancement_active();
                                }
                                Some(CommandResult::StartLoginForProvider {
                                    provider,
                                    login_with_claude_ai,
                                    label,
                                }) => {
                                    mikmik_tui::restore_terminal(&mut terminal).ok();
                                    if provider == mikmik_core::ProviderId::CODEX {
                                        let (tx, mut rx) = tokio::sync::mpsc::channel::<
                                            mikmik_tui::DeviceAuthEvent,
                                        >(
                                            8
                                        );
                                        tokio::spawn(async move {
                                            while let Some(evt) = rx.recv().await {
                                                if let mikmik_tui::DeviceAuthEvent::GotBrowserUrl {
                                                    url,
                                                } = evt
                                                {
                                                    eprintln!(
                                                        "\nOpening browser for Codex \
                                                         authentication...\nIf the browser \
                                                         did not open, visit:\n\n  {}\n",
                                                        url
                                                    );
                                                }
                                            }
                                        });
                                        match crate::codex_oauth_flow::run_oauth_flow_with_label(
                                            tx,
                                            label.as_deref(),
                                        )
                                        .await
                                        {
                                            Ok(_) => {
                                                app.status_message =
                                                    Some("Codex login successful!".to_string());
                                                eprintln!("\nCodex login successful!");
                                                break 'main;
                                            }
                                            Err(e) => {
                                                eprintln!("\nCodex login failed: {}", e);
                                            }
                                        }
                                    } else {
                                        match oauth_flow::run_oauth_login_flow_with_label(
                                            login_with_claude_ai,
                                            label.as_deref(),
                                        )
                                        .await
                                        {
                                            Ok(_) => {
                                                app.status_message =
                                                    Some("Login successful!".to_string());
                                                eprintln!(
                                                    "\nLogin successful! Please restart \
                                                     mikmik to use the new credentials."
                                                );
                                                break 'main;
                                            }
                                            Err(e) => {
                                                eprintln!("\nLogin failed: {}", e);
                                            }
                                        }
                                    }
                                    terminal = mikmik_tui::setup_terminal(
                                        app.config.mouse_capture_enabled(),
                                    )?;
                                    app.kitty_keyboard_active =
                                        mikmik_tui::keyboard_enhancement_active();
                                }
                                Some(CommandResult::Error(e)) => {
                                    app.status_message = Some(format!("Error: {}", e));
                                    command_failed = true;
                                }
                                Some(CommandResult::Silent) | None => {
                                    handled_by_cli = false;
                                }
                            }

                            // Sync effort visual + API level when CLI handled
                            // /effort with explicit args (/effort high).
                            if handled_by_cli && cmd_name == "effort" && !cmd_args.is_empty() {
                                if let Some(level) =
                                    mikmik_core::effort::EffortLevel::from_str(&cmd_args)
                                {
                                    app.set_effort_level(level);
                                    app.status_message = Some(format!(
                                        "Effort: {} {}",
                                        app.effort_level.symbol(),
                                        app.effort_level.label(),
                                    ));
                                }
                            }

                            // Sync vim mode when CLI handled /vim with explicit args.
                            if handled_by_cli
                                && matches!(cmd_name.as_str(), "vim" | "vi")
                                && !cmd_args.is_empty()
                            {
                                app.prompt_input.vim_enabled =
                                    matches!(cmd_args.trim(), "on" | "vim");
                            }

                            if !handled_by_cli && !handled_by_tui {
                                if remote_wants_text {
                                    // The command layer had nothing to say, so
                                    // the view really was the whole answer.
                                    app.status_message = Some(terminal_only_notice(&cmd_name));
                                } else if mikmik_commands::find_command(&cmd_name).is_none() {
                                    app.status_message =
                                        Some(format!("Unknown command: /{}", cmd_name));
                                    command_failed = true;
                                }
                            }

                            // Ahead of the notice below, because a client treats
                            // History as the whole transcript and replaces what
                            // it has. Sent the other way round, it would wipe
                            // the very notice explaining what just happened.
                            //
                            // Unconditional, unlike the connect-time send: an
                            // empty transcript is exactly the message here.
                            if transcript_replaced {
                                if let Some(runtime) = bridge_runtime.as_ref() {
                                    let (entries, omitted) = history_for_bridge(&messages);
                                    let _ = runtime
                                        .outbound_tx
                                        .try_send(BridgeOutbound::History { entries, omitted });
                                }
                            }

                            // The single point where a command's outcome leaves
                            // for a remote client. `CommandResult::Message` is
                            // absent on purpose: that arm already sends its own
                            // text and never touches `status_message`.
                            if app.status_message != status_before {
                                if let (Some(runtime), Some(message)) =
                                    (bridge_runtime.as_ref(), app.status_message.as_ref())
                                {
                                    let _ = runtime.outbound_tx.try_send(BridgeOutbound::Notice {
                                        message: message.clone(),
                                        is_error: command_failed,
                                    });
                                }
                            }

                            // Stop-on-error: a failed segment cancels the rest
                            // of a `/a && /b` chain, matching shell `&&`.
                            if command_failed {
                                command_chain.clear();
                            }

                            // If a UserMessage was queued (e.g. /compact), submit it.
                            if let Some(msg) = submit_user_msg {
                                messages.push(mikmik_core::types::Message::user(msg.clone()));
                                app.push_message(mikmik_core::types::Message::user(msg));
                                // Fall through to the send path below.
                            } else {
                                continue;
                            }
                        }

                        // Fire UserPromptSubmit hook (non-blocking)
                        // The plugin side runs whether or not settings.json
                        // declares hooks; only the settings side is skipped
                        // when the map is empty.
                        mikmik_plugins::run_global_hook(
                            mikmik_plugins::HookEventKind::UserPromptSubmit,
                            None,
                            serde_json::json!({
                                "prompt": input,
                                "session_id": tool_ctx.session_id,
                            }),
                        )
                        .await;
                        if !cmd_ctx.config.hooks.is_empty() {
                            let hook_ctx = mikmik_core::hooks::HookContext {
                                event: "UserPromptSubmit".to_string(),
                                tool_name: None,
                                tool_input: None,
                                tool_output: Some(input.clone()),
                                is_error: None,
                                session_id: Some(tool_ctx.session_id.clone()),
                            };
                            mikmik_core::hooks::run_hooks(
                                &cmd_ctx.config.hooks,
                                mikmik_core::config::HookEvent::UserPromptSubmit,
                                &hook_ctx,
                                &tool_ctx.working_dir,
                            )
                            .await;
                        }

                        // Regular user message (with optional image attachments + file injection)
                        let pending_imgs = app.prompt_input.clear_images();

                        // Check for file injection if enabled
                        if app.config.file_injection_is_enabled() {
                            use mikmik_tui::file_injection::parse_at_refs;

                            // file_injection_force is set when user chose "inject anyways" in the
                            // warning dialog — pass limit 0 so all files are treated as within
                            // limit. Also drop any directory refs silently on force re-submit so
                            // they don't loop back to the directory warning.
                            let was_force = app.file_injection_force;
                            let effective_limit = if app.file_injection_force {
                                app.file_injection_force = false;
                                0
                            } else {
                                app.config.effective_file_injection_max_size()
                            };
                            let (within_limit, mut oversized) =
                                parse_at_refs(&input, &tool_ctx.working_dir, effective_limit);
                            if was_force {
                                oversized.retain(|f| {
                                    !matches!(f.issue, Some(mikmik_tui::AtFileIssue::IsDirectory))
                                });
                            }

                            if !oversized.is_empty() {
                                // Show either the directory warning or the file warning, never both.
                                // Directories take precedence: if any are present, show only those.
                                let has_dirs = oversized.iter().any(|f| {
                                    matches!(f.issue, Some(mikmik_tui::AtFileIssue::IsDirectory))
                                });
                                let oversized_summaries: Vec<(
                                    String,
                                    usize,
                                    mikmik_tui::AtFileIssue,
                                )> = oversized
                                    .iter()
                                    .filter(|f| {
                                        let is_dir = matches!(
                                            f.issue,
                                            Some(mikmik_tui::AtFileIssue::IsDirectory)
                                        );
                                        if has_dirs {
                                            is_dir
                                        } else {
                                            !is_dir
                                        }
                                    })
                                    .filter_map(|f| {
                                        f.issue.clone().map(|issue| {
                                            (f.path.display().to_string(), f.size_kb, issue)
                                        })
                                    })
                                    .collect();

                                app.file_injection_dialog.show(
                                    input.clone(),
                                    pending_imgs,
                                    oversized_summaries,
                                    app.config.effective_file_injection_max_size(),
                                    Some(tool_ctx.working_dir.clone()),
                                );
                                app.set_prompt_text(input);
                                continue;
                            }

                            // No oversized files: inject within-limit files and send
                            let file_prefix =
                                mikmik_tui::file_injection::build_file_blocks(&within_limit);

                            let user_msg = if !file_prefix.is_empty() || !pending_imgs.is_empty() {
                                let mut blocks: Vec<mikmik_core::types::ContentBlock> = Vec::new();

                                // Add file blocks if there's any file content
                                if !file_prefix.is_empty() {
                                    blocks.push(mikmik_core::types::ContentBlock::Text {
                                        text: file_prefix,
                                    });
                                }

                                // Add image blocks
                                for img in &pending_imgs {
                                    if let Some(b64) =
                                        mikmik_tui::image_paste::encode_image_base64(&img.path)
                                    {
                                        blocks.push(mikmik_core::types::ContentBlock::Image {
                                            source: mikmik_core::types::ImageSource {
                                                source_type: "base64".to_string(),
                                                media_type: Some("image/png".to_string()),
                                                data: Some(b64),
                                                url: None,
                                            },
                                        });
                                    }
                                }

                                // Add the original input text
                                blocks.push(mikmik_core::types::ContentBlock::Text {
                                    text: input.clone(),
                                });

                                mikmik_core::types::Message::user_blocks(blocks)
                            } else {
                                mikmik_core::types::Message::user(input.clone())
                            };

                            messages.push(user_msg.clone());
                            app.push_message(user_msg);
                            session.messages = messages.clone();
                            session.updated_at = chrono::Utc::now();
                        } else {
                            // File injection disabled: send as-is
                            let user_msg = if pending_imgs.is_empty() {
                                mikmik_core::types::Message::user(input.clone())
                            } else {
                                let mut blocks: Vec<mikmik_core::types::ContentBlock> =
                                    pending_imgs
                                        .iter()
                                        .filter_map(|img| {
                                            mikmik_tui::image_paste::encode_image_base64(&img.path)
                                                .map(|b64| {
                                                    mikmik_core::types::ContentBlock::Image {
                                                        source: mikmik_core::types::ImageSource {
                                                            source_type: "base64".to_string(),
                                                            media_type: Some(
                                                                "image/png".to_string(),
                                                            ),
                                                            data: Some(b64),
                                                            url: None,
                                                        },
                                                    }
                                                })
                                        })
                                        .collect();
                                blocks.push(mikmik_core::types::ContentBlock::Text {
                                    text: input.clone(),
                                });
                                mikmik_core::types::Message::user_blocks(blocks)
                            };

                            messages.push(user_msg.clone());
                            app.push_message(user_msg);
                            session.messages = messages.clone();
                            session.updated_at = chrono::Utc::now();
                        }

                        // Update terminal title from session title or first message
                        if session.title.is_some() {
                            mikmik_tui::update_terminal_title(session.title.as_deref());
                        } else {
                            // Use a truncated version of the first user message
                            let topic: String = input.chars().take(60).collect();
                            mikmik_tui::update_terminal_title(Some(&topic));
                        }

                        // The companion answers only when addressed by name.
                        // The previous line goes now either way, so a stale
                        // reply never sits above a new question.
                        app.companion_bubble = None;
                        if app.companion_addressed_in(&input).is_some() {
                            if let Some(companion) = app.companion.clone() {
                                let cfg = cmd_ctx.config.clone();
                                let tracker = cost_tracker.clone();
                                let tx = bubble_tx.clone();
                                let said = input.clone();
                                // Spawned rather than awaited: the turn must
                                // not wait on a decoration.
                                tokio::spawn(async move {
                                    if let Ok(line) = mikmik_commands::companion_reply(
                                        &cfg, &tracker, &companion, &said,
                                    )
                                    .await
                                    {
                                        let _ = tx.send(line);
                                    }
                                });
                            }
                        }

                        // Start async query
                        app.is_streaming = true;
                        app.streaming_text.clear();

                        let ct = CancellationToken::new();
                        cancel = Some(ct.clone());

                        // Use Arc<Mutex> so the task can write updated messages back
                        let msgs_arc = Arc::new(tokio::sync::Mutex::new(messages.clone()));
                        let msgs_arc_clone = msgs_arc.clone();

                        // Share the Arc so the spawned task can access all tools (incl. MCP).
                        let tools_arc_clone = tools_arc.clone();
                        let mut ctx_clone = tool_ctx.clone();
                        let mut qcfg = base_query_config.clone();
                        qcfg.model = session_model_string(&cmd_ctx.config, &model_registry);
                        qcfg.max_tokens = cmd_ctx.config.effective_max_tokens();
                        // Re-read per turn so `/turns` reaches the next run; an agent's
                        // own limit still wins inside the loop.
                        qcfg.max_turns = cmd_ctx
                            .config
                            .max_turns
                            .unwrap_or(mikmik_core::constants::MAX_TURNS_DEFAULT);
                        qcfg.append_system_prompt = cmd_ctx.config.append_system_prompt.clone();
                        qcfg.system_prompt = base_query_config.system_prompt.clone();
                        qcfg.output_style = cmd_ctx.config.effective_output_style();
                        qcfg.output_style_prompt = cmd_ctx.config.resolve_output_style_prompt();
                        qcfg.working_directory = Some(tool_ctx.working_dir.display().to_string());
                        qcfg.workspace_roots =
                            roots_for_prompt(&tool_ctx.working_dir, &tool_ctx.config);
                        // Read per turn rather than once at startup: `/buddy`
                        // can turn the companion on, off, or hatch it mid-session.
                        qcfg.companion_addendum = app.companion_addendum();
                        // The active-goal system-prompt addendum is now injected
                        // inside run_query_loop per turn (issue #230 / MI-3), so
                        // it also covers in-loop continuation turns.
                        // The level the operator chose, wherever they chose it:
                        // the picker, the model picker, `/effort` or the flag.
                        // Left unset it stays unset, which is not the same as
                        // sending the default.
                        if app.effort_explicit {
                            qcfg.effort_level = Some(app.effort_level);
                        }
                        // Wire completion_notifier if a command queue is available.
                        if let Some(ref cq) = qcfg.command_queue {
                            let cq = cq.clone();
                            ctx_clone.completion_notifier =
                                Some(mikmik_tools::CompletionNotifier::new(move |msg| {
                                    cq.push(
                                        mikmik_query::QueuedCommand::InjectSystemMessage(msg),
                                        mikmik_query::CommandPriority::Normal,
                                    );
                                }));
                        }
                        let tracker = cost_tracker.clone();
                        let tx = event_tx.clone();
                        let client_clone = client.clone();

                        let handle = tokio::spawn(async move {
                            let mut msgs = msgs_arc_clone.lock().await.clone();
                            let outcome = mikmik_query::run_query_loop(
                                client_clone.as_ref(),
                                &mut msgs,
                                tools_arc_clone.as_slice(),
                                &ctx_clone,
                                &qcfg,
                                tracker,
                                Some(tx),
                                ct,
                                None,
                            )
                            .await;
                            // Write updated messages (with tool calls + assistant response) back
                            *msgs_arc_clone.lock().await = msgs;
                            outcome
                        });

                        // Store the Arc so we can read messages after task completes
                        current_query = Some((handle, msgs_arc));
                        continue;
                    }
                    if let Some(pr) = app.permission_request.as_mut() {
                        if mikmik_tui::dialogs::handle_permission_key(pr, key) {
                            let tool_use_id = pr.tool_use_id.clone();
                            let selected_option = pr.selected_option;
                            let selected_key = pr.options.get(selected_option).map(|o| o.key);
                            let should_record_bash_prefix = selected_key == Some('P');
                            let bash_prefix = if should_record_bash_prefix {
                                match &pr.kind {
                                    mikmik_tui::dialogs::PermissionDialogKind::Bash {
                                        command,
                                        ..
                                    } => {
                                        let first_word = command
                                            .split_whitespace()
                                            .next()
                                            .unwrap_or("")
                                            .to_string();
                                        if first_word.is_empty() {
                                            None
                                        } else {
                                            Some(first_word)
                                        }
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            app.permission_request = None;

                            if let Some(prefix) = bash_prefix {
                                app.bash_prefix_allowlist.insert(prefix.clone());
                                // "Always allow" must survive restarts: persist
                                // the prefix to settings.json so it is reloaded
                                // into the allowlist on the next launch.
                                if let Ok(mut settings) = mikmik_core::config::Settings::load_sync()
                                {
                                    if !settings.allowed_bash_prefixes.contains(&prefix) {
                                        settings.allowed_bash_prefixes.push(prefix);
                                        let _ = settings.save_sync();
                                    }
                                }
                            }

                            if let Some(settlement) = settle_pending_permission(
                                &pending_permissions,
                                tool_ctx.permission_manager.as_ref(),
                                &tool_use_id,
                                selected_key,
                            ) {
                                if settlement.denied {
                                    mikmik_plugins::run_global_hook(
                                        mikmik_plugins::HookEventKind::PermissionDenied,
                                        Some(&settlement.tool_name),
                                        serde_json::json!({ "tool_name": settlement.tool_name }),
                                    )
                                    .await;
                                }
                            }
                            continue;
                        }
                        continue;
                    }

                    app.handle_key_event(key);
                    cmd_ctx.config = app.config.clone();
                    tool_ctx.config = app.config.clone();
                    if let Some(manager) = tool_ctx.permission_manager.as_ref() {
                        if let Ok(mut manager) = manager.lock() {
                            manager.mode = tool_ctx.config.permission_mode;
                        }
                    }
                    if !app.model_name.is_empty() {
                        session.model = app.model_name.clone();
                    }
                    // The agent mode `Tab` just cycled is applied by the loop
                    // body, which watches `agent_mode_changed` however it was
                    // set.
                    if !app.is_streaming && app.messages.len() < messages.len() {
                        messages = app.messages.clone();
                        session.messages = messages.clone();
                        session.updated_at = chrono::Utc::now();
                    }
                }
                Event::Paste(data) => {
                    // Bracketed paste (Cmd+V on macOS, Ctrl+Shift+V on Linux, any
                    // terminal with bracketed paste). Deliberately NOT gated on
                    // is_streaming: the prompt stays editable during a turn so a
                    // follow-up can be composed/queued — dropping the event here
                    // silently loses the pasted content.
                    if app.permission_request.is_none()
                        && !app.history_search_overlay.visible
                        && app.history_search.is_none()
                    {
                        if app.key_input_dialog.visible {
                            // Paste into API key input dialog
                            for ch in data.chars() {
                                app.key_input_dialog.insert_char(ch);
                            }
                        } else {
                            // Paste into the main prompt through the shared path
                            // so file-path/image pastes and the large-paste
                            // placeholder are handled uniformly.
                            app.handle_paste_data(data);
                            app.refresh_prompt_input();
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse_event(mouse);
                }
                Event::Resize(_, _) => {
                    // Terminal resize - will be handled on next draw
                }
                _ => {}
            }
        }

        if app.permission_request.is_none() {
            loop {
                let next_pending = pending_permissions.lock().queue.pop_front();
                let Some(mut pending) = next_pending else {
                    break;
                };

                let prefix_allowed = pending.request.tool_name == "Bash"
                    && pending
                        .request
                        .path
                        .as_deref()
                        .map(|command| app.bash_command_allowed_by_prefix(command))
                        .unwrap_or(false);

                let reevaluated = if prefix_allowed {
                    Some(mikmik_core::permissions::PermissionDecision::Allow)
                } else {
                    tool_ctx
                        .permission_manager
                        .as_ref()
                        .and_then(|manager| manager.lock().ok())
                        .map(|manager| {
                            manager.evaluate(
                                &pending.request.tool_name,
                                &pending.request.description,
                                pending.request.path.as_deref(),
                                pending.request.working_dir.as_deref(),
                                &pending.request.allowed_roots,
                            )
                        })
                };

                match reevaluated {
                    Some(mikmik_core::permissions::PermissionDecision::Ask { .. }) | None => {
                        let tool_use_id = pending.tool_use_id.clone();
                        let dialog = permission_request_from_core(&pending);
                        // Tell the remote client too, or a remotely-driven
                        // session stalls here with nobody at the keyboard.
                        if let Some(ref runtime) = bridge_runtime {
                            let _ =
                                runtime
                                    .outbound_tx
                                    .try_send(BridgeOutbound::PermissionRequest {
                                        request_id: tool_use_id.clone(),
                                        tool_use_id: tool_use_id.clone(),
                                        tool_name: dialog.tool_name.clone(),
                                        description: dialog.description.clone(),
                                        options: dialog
                                            .options
                                            .iter()
                                            .map(|option| option.label.clone())
                                            .collect(),
                                    });
                        }
                        mikmik_plugins::run_global_hook(
                            mikmik_plugins::HookEventKind::PermissionRequest,
                            Some(&dialog.tool_name),
                            serde_json::json!({
                                "tool_name": dialog.tool_name,
                                "description": dialog.description,
                            }),
                        )
                        .await;
                        // The turn is now blocked on an answer, the same as it
                        // is for a question. Read the settings here rather than
                        // caching them at startup, so a toggle in the settings
                        // screen takes effect on the very next prompt.
                        mikmik_core::desktop_notify::notify(
                            &mikmik_core::config::Settings::load_sync().unwrap_or_default(),
                            mikmik_core::desktop_notify::NotifyEvent::PermissionRequested,
                            &dialog.description,
                        );
                        app.permission_request = Some(dialog);
                        pending_permissions
                            .lock()
                            .waiting
                            .insert(tool_use_id, pending);
                        break;
                    }
                    Some(decision) => {
                        if let Some(tx) = pending.decision_tx.take() {
                            let _ = tx.send(decision);
                        }
                    }
                }
            }
        }

        // Drain the companion's replies. Keeping only the last one means a
        // burst can never queue lines the user has already moved past.
        while let Ok(line) = bubble_rx.try_recv() {
            app.companion_bubble = Some(line);
        }

        // Drain query events — also forward relevant ones to the bridge as outbound.
        while let Ok(evt) = event_rx.try_recv() {
            // Forward to bridge before consuming (clone only what we need).
            if let Some(ref runtime) = bridge_runtime {
                let outbound: Option<BridgeOutbound> = match &evt {
                    QueryEvent::Stream(mikmik_api::AnthropicStreamEvent::ContentBlockDelta {
                        delta: mikmik_api::streaming::ContentDelta::TextDelta { text },
                        index,
                        ..
                    }) => Some(BridgeOutbound::TextDelta {
                        delta: text.clone(),
                        message_id: format!("msg-{}", index),
                    }),
                    QueryEvent::Stream(mikmik_api::AnthropicStreamEvent::ContentBlockDelta {
                        delta: mikmik_api::streaming::ContentDelta::ThinkingDelta { thinking },
                        index,
                        ..
                    }) => Some(BridgeOutbound::ThinkingDelta {
                        delta: thinking.clone(),
                        message_id: format!("think-{}", index),
                    }),
                    QueryEvent::ToolStart {
                        tool_name,
                        tool_id,
                        input_json,
                    } => Some(BridgeOutbound::ToolStart {
                        id: tool_id.clone(),
                        name: tool_name.clone(),
                        input_preview: Some(input_json.clone()),
                    }),
                    QueryEvent::ToolEnd {
                        tool_id,
                        result,
                        is_error,
                        duration_ms,
                        ..
                    } => Some(BridgeOutbound::ToolEnd {
                        id: tool_id.clone(),
                        output: result.clone(),
                        is_error: *is_error,
                        duration_ms: *duration_ms,
                    }),
                    QueryEvent::TurnComplete {
                        stop_reason,
                        turn,
                        usage,
                        model,
                    } => Some(BridgeOutbound::TurnComplete {
                        message_id: format!("turn-{}", turn),
                        stop_reason: stop_reason.clone(),
                        usage: usage
                            .as_ref()
                            .map(|u| bridge_usage(model, u, cost_tracker.total_cost_usd())),
                    }),
                    QueryEvent::Error(msg) => Some(BridgeOutbound::Error {
                        message: msg.clone(),
                    }),
                    QueryEvent::Status(msg) => Some(BridgeOutbound::Status {
                        message: msg.clone(),
                    }),
                    QueryEvent::Compacted {
                        messages_before,
                        messages_after,
                        ..
                    } => {
                        let removed = messages_before.saturating_sub(*messages_after);
                        Some(BridgeOutbound::Status {
                            message: format!(
                                "Compacted {removed} message{} into a summary.",
                                if removed == 1 { "" } else { "s" }
                            ),
                        })
                    }
                    QueryEvent::TokenWarning { state, pct_used } => {
                        use mikmik_query::compact::TokenWarningState;
                        // `Ok` is the absence of a warning, so sending it would
                        // put a reassurance on screen that was never asked for.
                        match state {
                            TokenWarningState::Ok => None,
                            TokenWarningState::Warning => Some(BridgeOutbound::TokenWarning {
                                level: "warning".to_string(),
                                pct_used: *pct_used,
                            }),
                            TokenWarningState::Critical => Some(BridgeOutbound::TokenWarning {
                                level: "critical".to_string(),
                                pct_used: *pct_used,
                            }),
                        }
                    }
                    QueryEvent::Advisory {
                        advisor,
                        severity,
                        note,
                    } => {
                        // A remote client has no advisory frame, so it goes out
                        // as status text. Without this the agent changes
                        // direction on screen with nothing to explain it.
                        let who = advisor.as_deref().unwrap_or("advisor");
                        Some(BridgeOutbound::Status {
                            message: format!("{who} ({severity}): {note}"),
                        })
                    }
                    _ => None,
                };
                if let Some(ob) = outbound {
                    let _ = runtime.outbound_tx.try_send(ob);
                }
            }
            // Also forward to the BridgeSessionInfo relay channel (best-effort).
            // This drives the post_bridge_event relay task spawned on Connected.
            if bridge_session_info.is_some() {
                let relay_payload: Option<String> = match &evt {
                    QueryEvent::Stream(mikmik_api::AnthropicStreamEvent::ContentBlockDelta {
                        delta: mikmik_api::streaming::ContentDelta::TextDelta { text },
                        ..
                    }) => Some(
                        serde_json::json!({
                            "type": "text_chunk",
                            "text": text,
                        })
                        .to_string(),
                    ),
                    QueryEvent::ToolStart {
                        tool_name,
                        tool_id,
                        input_json,
                    } => Some(
                        serde_json::json!({
                            "type": "tool_start",
                            "tool_name": tool_name,
                            "tool_id": tool_id,
                            "input": input_json,
                        })
                        .to_string(),
                    ),
                    QueryEvent::ToolEnd {
                        tool_name,
                        tool_id,
                        result,
                        is_error,
                        duration_ms,
                    } => Some(
                        serde_json::json!({
                            "type": "tool_end",
                            "tool_name": tool_name,
                            "tool_id": tool_id,
                            "result": result,
                            "is_error": is_error,
                            "duration_ms": duration_ms,
                        })
                        .to_string(),
                    ),
                    _ => None,
                };
                if let Some(payload) = relay_payload {
                    let _ = relay_ev_tx.try_send(payload);
                }
            }
            app.handle_query_event(evt);

            // The timeline is built once, by the app, and the rows it just
            // recorded go out as they are. Deriving them a second time from the
            // raw events would give the remote client its own timings, and a
            // long poll can sit on a batch for its whole interval, so those
            // durations would measure the transport rather than the work.
            if let Some(ref runtime) = bridge_runtime {
                for row in app.drain_timeline_outbox() {
                    let _ = runtime
                        .outbound_tx
                        .try_send(BridgeOutbound::TimelineRow(row));
                }
            } else {
                // Nobody is listening, so the queue would only grow.
                app.drain_timeline_outbox();
            }
        }

        // Drain TUI-facing bridge events.
        let mut disconnect_bridge = false;
        if let Some(runtime) = bridge_runtime.as_mut() {
            loop {
                match runtime.tui_rx.try_recv() {
                    Ok(TuiBridgeEvent::Connected {
                        session_url,
                        session_id: conn_sid,
                    }) => {
                        let short = if session_url.len() > 60 {
                            format!("{}…", &session_url[..60])
                        } else {
                            session_url.clone()
                        };
                        app.bridge_state = BridgeConnectionState::Connected {
                            session_url: session_url.clone(),
                            peer_count: 0,
                        };
                        app.remote_session_url = Some(session_url.clone());
                        cmd_ctx.remote_session_url = Some(session_url.clone());
                        app.notifications.push(
                            NotificationKind::Success,
                            format!("Remote control active: {}", short),
                            Some(5),
                        );
                        // Send what has already happened, or a client attaching
                        // to a session in progress sees an empty screen. The
                        // MCP trust queue fills at startup, so a prompt is
                        // usually already on screen by this point too.
                        for event in session_snapshot(
                            &app,
                            pending_question_id.as_deref(),
                            pending_mcp_approval_id.as_deref(),
                            pending_bypass_id.as_deref(),
                            &messages,
                        ) {
                            let _ = runtime.outbound_tx.try_send(event);
                        }

                        // Persist the session URL into the saved session record.
                        session.remote_session_url = Some(session_url.clone());
                        session.updated_at = chrono::Utc::now();
                        if let Err(e) = mikmik_core::history::save_session(&session).await {
                            app.push_notification(
                                mikmik_tui::NotificationKind::Error,
                                format!("Could not save the session: {e}"),
                                None,
                            );
                        }

                        // Wire the BridgeSessionInfo relay so live tool/text events reach
                        // the web UI via /api/bridge/sessions. This runs alongside
                        // run_bridge_loop as a best-effort supplementary delivery path.
                        if let Some(ref token) = bridge_token {
                            let info = std::sync::Arc::new(mikmik_bridge::BridgeSessionInfo {
                                session_id: conn_sid.clone(),
                                session_url: session_url.clone(),
                                token: token.clone(),
                            });
                            bridge_session_info = Some(info.clone());

                            // Relay consumer: moves relay_ev_rx (taken from the Option)
                            // into a background task that calls post_bridge_event per item.
                            if let Some(rx) = relay_ev_rx_opt.take() {
                                let info_relay = info.clone();
                                tokio::spawn(async move {
                                    let mut rx = rx;
                                    while let Some(payload) = rx.recv().await {
                                        let _ =
                                            mikmik_bridge::post_bridge_event(&info_relay, payload)
                                                .await;
                                    }
                                });
                            }

                            // Poll task: periodically calls poll_bridge_messages and
                            // forwards inbound user messages to remote_prompt_tx.
                            let info_poll = info.clone();
                            let poll_tx = remote_prompt_tx.clone();
                            tokio::spawn(async move {
                                let mut since_id: Option<String> = None;
                                loop {
                                    match mikmik_bridge::poll_bridge_messages(
                                        &info_poll,
                                        since_id.as_deref(),
                                    )
                                    .await
                                    {
                                        Ok(msgs) if !msgs.is_empty() => {
                                            for msg in &msgs {
                                                since_id = Some(msg.id.clone());
                                                if msg.role == "user"
                                                    && poll_tx
                                                        .send(msg.content.clone())
                                                        .await
                                                        .is_err()
                                                {
                                                    return;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                }
                            });
                        }
                    }
                    Ok(TuiBridgeEvent::Disconnected { reason }) => {
                        app.bridge_state = BridgeConnectionState::Disconnected;
                        app.remote_session_url = None;
                        cmd_ctx.remote_session_url = None;
                        if let Some(r) = reason {
                            app.notifications.push(
                                NotificationKind::Warning,
                                format!("Bridge disconnected: {}", r),
                                Some(5),
                            );
                        }
                        disconnect_bridge = true;
                        break;
                    }
                    Ok(TuiBridgeEvent::Reconnecting { attempt }) => {
                        app.bridge_state = BridgeConnectionState::Reconnecting { attempt };
                    }
                    Ok(TuiBridgeEvent::InboundPrompt {
                        content,
                        attachments,
                        ..
                    }) => {
                        // Park it. Starting the turn from here is what let a
                        // prompt arriving mid-turn spawn a second query on top
                        // of the running one.
                        deferred_remote_prompts.push_back((content, attachments));
                    }
                    Ok(TuiBridgeEvent::Cancelled) => {
                        if app.is_streaming {
                            if let Some(ref ct) = cancel {
                                ct.cancel();
                            }
                            app.is_streaming = false;
                            app.status_message = Some("Cancelled by remote control.".to_string());
                        }
                    }
                    Ok(TuiBridgeEvent::PermissionResponse {
                        tool_use_id,
                        response,
                    }) => {
                        // A remote answer must take the same route as a keyboard
                        // answer, otherwise the blocked tool never learns the
                        // outcome and the dialog only appears to close.
                        use mikmik_bridge::PermissionResponseKind;
                        let selected_key = match response {
                            PermissionResponseKind::Allow => 'y',
                            PermissionResponseKind::AllowSession => 'Y',
                            PermissionResponseKind::Deny => 'n',
                        };
                        let settlement = settle_pending_permission(
                            &pending_permissions,
                            tool_ctx.permission_manager.as_ref(),
                            &tool_use_id,
                            Some(selected_key),
                        );
                        if let Some(ref settled) = settlement {
                            if settled.denied {
                                mikmik_plugins::run_global_hook(
                                    mikmik_plugins::HookEventKind::PermissionDenied,
                                    Some(&settled.tool_name),
                                    serde_json::json!({ "tool_name": settled.tool_name }),
                                )
                                .await;
                            }
                        }
                        if settlement.is_some()
                            && app
                                .permission_request
                                .as_ref()
                                .is_some_and(|pr| pr.tool_use_id == tool_use_id)
                        {
                            app.permission_request = None;
                        }
                    }
                    Ok(TuiBridgeEvent::QuestionAnswer {
                        question_id,
                        answer,
                    }) => {
                        // Route through the dialog so a remote answer and a
                        // keyboard answer take the same path to the tool.
                        if pending_question_id.as_deref() == Some(question_id.as_str())
                            && app.ask_user_dialog.answer_externally(answer)
                        {
                            pending_question_id = None;
                        }
                    }
                    Ok(TuiBridgeEvent::McpApproval {
                        request_id,
                        decision,
                    }) => {
                        // Route through the dialog's own settle path so a
                        // remote answer and a keyboard answer cannot diverge.
                        if pending_mcp_approval_id.as_deref() == Some(request_id.as_str()) {
                            app.mcp_approval.close();
                            app.handle_mcp_approval_decision(mcp_choice_for(decision));
                            pending_mcp_approval_id = None;
                        }
                    }
                    Ok(TuiBridgeEvent::BypassResponse { request_id, accept }) => {
                        // Through the same two methods the keyboard answer
                        // uses, so a remote decline still restores the previous
                        // mode and a remote decline at startup still exits.
                        if pending_bypass_id.as_deref() == Some(request_id.as_str()) {
                            if accept {
                                app.accept_bypass_permissions();
                            } else {
                                app.decline_bypass_permissions();
                            }
                            pending_bypass_id = None;
                        }
                    }
                    Ok(TuiBridgeEvent::ClientAttached) => {
                        // Whatever the session is waiting on was announced
                        // once, when it happened. A client that was not there
                        // then has no other way to hear about it.
                        for event in session_snapshot(
                            &app,
                            pending_question_id.as_deref(),
                            pending_mcp_approval_id.as_deref(),
                            pending_bypass_id.as_deref(),
                            &messages,
                        ) {
                            let _ = runtime.outbound_tx.try_send(event);
                        }
                    }
                    Ok(TuiBridgeEvent::SessionRename { title }) => {
                        // Same settle path as `/rename`, so the two entries
                        // cannot leave different surfaces stale.
                        let title = title.trim().to_string();
                        if !title.is_empty() {
                            apply_session_rename(
                                title,
                                &mut session,
                                &mut cmd_ctx,
                                &mut app,
                                &mut transcript,
                            )
                            .await;
                        }
                    }
                    Ok(TuiBridgeEvent::Error(msg)) => {
                        app.bridge_state = BridgeConnectionState::Failed {
                            reason: msg.clone(),
                        };
                        app.notifications.push(
                            NotificationKind::Warning,
                            format!("Bridge error: {}", msg),
                            Some(5),
                        );
                        disconnect_bridge = true;
                        break;
                    }
                    Ok(TuiBridgeEvent::Ping) => {
                        // No TUI action needed; pong is handled inside run_bridge_loop.
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        app.bridge_state = BridgeConnectionState::Disconnected;
                        app.remote_session_url = None;
                        cmd_ctx.remote_session_url = None;
                        app.notifications.push(
                            NotificationKind::Warning,
                            "Remote control connection lost.".to_string(),
                            Some(5),
                        );
                        disconnect_bridge = true;
                        break;
                    }
                }
            }
        }
        if disconnect_bridge {
            bridge_runtime = None;
        }

        // Prompts from the supplementary `/api/bridge/sessions` poll task join
        // the same queue as the primary protocol's. This path used to drop a
        // prompt with no trace whenever a turn was already running.
        while let Ok(content) = remote_prompt_rx.try_recv() {
            deferred_remote_prompts.push_back((content, Vec::new()));
        }

        // Say why a prompt is sitting there, once per spell of waiting. Without
        // it the sender sees nothing happen and sends the same thing again.
        if !deferred_remote_prompts.is_empty() && !deferred_notice_sent {
            if let Some(reason) = remote_wait_reason(
                app.is_streaming || current_query.is_some(),
                app.blocking_modal_open(),
                app.prompt_input.text.is_empty(),
            ) {
                if let Some(runtime) = bridge_runtime.as_ref() {
                    let _ = runtime.outbound_tx.try_send(BridgeOutbound::Notice {
                        message: reason.to_string(),
                        is_error: false,
                    });
                }
                deferred_notice_sent = true;
            }
        }

        // The one place a remote prompt becomes a turn.
        if !deferred_remote_prompts.is_empty()
            && remote_turn_can_start(
                app.is_streaming,
                current_query.is_some(),
                app.blocking_modal_open(),
                app.prompt_input.text.is_empty(),
            )
        {
            deferred_notice_sent = false;
            if let Some((content, attachments)) = deferred_remote_prompts.pop_front() {
                if content.trim_start().starts_with('/') {
                    // A slash command has to go through the keyboard submit
                    // path, or it reaches the model as plain text. That path
                    // runs on a synthesised Enter, so hand the text over and
                    // let it fire rather than reimplementing 400 lines of
                    // command handling that would then drift.
                    app.set_prompt_text(content);
                    app.pending_auto_submit = true;
                    remote_submit = true;
                } else {
                    // The prompt box stays empty on purpose. The turn is
                    // already visible in the transcript below, and a turn only
                    // starts while the box is empty: text left here would
                    // queue every later remote prompt for good.
                    let message = remote_user_message(&content, &attachments);
                    messages.push(message.clone());
                    app.push_message(message);
                    session.messages = messages.clone();
                    session.updated_at = chrono::Utc::now();
                    app.is_streaming = true;
                    app.streaming_text.clear();
                    let ct = CancellationToken::new();
                    cancel = Some(ct.clone());
                    let msgs_arc = Arc::new(tokio::sync::Mutex::new(messages.clone()));
                    let msgs_arc_clone = msgs_arc.clone();
                    let tools_arc_clone = tools_arc.clone();
                    let ctx_clone = tool_ctx.clone();
                    let mut qcfg = base_query_config.clone();
                    qcfg.model = session_model_string(&cmd_ctx.config, &model_registry);
                    qcfg.max_tokens = cmd_ctx.config.effective_max_tokens();
                    // Re-read per turn so `/turns` reaches the next run; an agent's
                    // own limit still wins inside the loop.
                    qcfg.max_turns = cmd_ctx
                        .config
                        .max_turns
                        .unwrap_or(mikmik_core::constants::MAX_TURNS_DEFAULT);
                    // A prompt from a phone is the same turn as one typed
                    // here, so it runs at the same effort, and the model is
                    // told about the same companion.
                    if app.effort_explicit {
                        qcfg.effort_level = Some(app.effort_level);
                    }
                    qcfg.companion_addendum = app.companion_addendum();
                    let tracker = cost_tracker.clone();
                    let tx = event_tx.clone();
                    let client_clone = client.clone();
                    let handle = tokio::spawn(async move {
                        let mut msgs = msgs_arc_clone.lock().await.clone();
                        let outcome = mikmik_query::run_query_loop(
                            client_clone.as_ref(),
                            &mut msgs,
                            tools_arc_clone.as_slice(),
                            &ctx_clone,
                            &qcfg,
                            tracker,
                            Some(tx),
                            ct,
                            None,
                        )
                        .await;
                        *msgs_arc_clone.lock().await = msgs;
                        outcome
                    });
                    current_query = Some((handle, msgs_arc));
                }
            }
        }

        // External status line: publish the session state whenever it changed,
        // then take whatever the command last printed. `refreshInterval` adds a
        // timer on top; without it an idle session never runs the command.
        if let Some(ref status_line) = status_line {
            let snapshot = status_line::snapshot(
                &app,
                &tool_ctx.session_id,
                mikmik_core::session_storage::transcript_path(
                    &mikmik_core::session_storage::transcript_root_for(&tool_ctx.working_dir),
                    &tool_ctx.session_id,
                )
                .ok()
                .map(|path| path.display().to_string()),
                &status_line_project_dir,
                messages.len(),
            );
            let due = status_line
                .refresh_interval()
                .is_some_and(|interval| status_line_last_run.elapsed() >= interval);
            if due || status_line_trigger.as_ref() != Some(&snapshot.trigger) {
                status_line_trigger = Some(snapshot.trigger.clone());
                status_line_last_run = std::time::Instant::now();
                let size = terminal.size().unwrap_or_default();
                status_line.request(snapshot.payload(), size.width, size.height);
            }
            while let Ok(text) = status_line_rx.try_recv() {
                app.status_line_override = if text.is_empty() { None } else { Some(text) };
            }
        }

        // Sync cost/token counters and expire transient UI state.
        app.cost_usd = app.cost_tracker.total_cost_usd();
        app.token_count = app.cost_tracker.total_tokens() as u32;
        app.notifications.tick();
        app.memory_update_notification.tick();

        // Drain background model-fetch results (non-blocking).
        if let Some(ref mut rx) = app.model_fetch_rx {
            match rx.try_recv() {
                Ok(Ok(entries)) => {
                    let provider = app
                        .model_picker_provider_id
                        .clone()
                        .or_else(|| app.config.provider.clone())
                        .unwrap_or_else(|| "anthropic".to_string());
                    let provider_prefix = format!("{}/", provider);
                    let current = app
                        .model_name
                        .strip_prefix(&provider_prefix)
                        .unwrap_or(app.model_name.as_str())
                        .to_string();
                    // Additively merge the live-discovery result onto the
                    // catalog projection already loaded when the picker opened,
                    // mirroring opencode's github-copilot models.ts merge-by-id
                    // (models.ts:229-255): keep the catalog metadata for ids in
                    // both, append only live ids not already listed. An empty
                    // result — catalog-backed provider (now the trait default),
                    // an unreachable endpoint, or a missing entitlement — is a
                    // no-op and never wipes the projection. For copilot the id
                    // IS the api.id, so this is the by-api.id merge.
                    //
                    // Anthropic and local runtimes return authoritative lists.
                    // Anthropic keeps the catalog projection when discovery is
                    // empty because that can mean authentication failed. Local
                    // discovery reports failures separately, so empty means no
                    // models are loaded.
                    //
                    // When the picker spans several providers the fetch only
                    // speaks for one of them, so the provider-scoped variants
                    // rewrite that section and leave the others standing.
                    let cross_provider = app.model_picker.is_cross_provider();
                    let authoritative = provider == "anthropic"
                        || mikmik_tui::model_picker::provider_has_authoritative_live_models(
                            &provider,
                        );
                    let keep_projection_when_empty = provider == "anthropic";
                    if authoritative && !(entries.is_empty() && keep_projection_when_empty) {
                        if cross_provider {
                            app.model_picker.replace_provider_models(&provider, entries);
                        } else {
                            app.model_picker.set_models(entries);
                        }
                    } else if !authoritative {
                        if cross_provider {
                            app.model_picker.merge_provider_models(&provider, entries);
                        } else {
                            app.model_picker.merge_models(entries);
                        }
                    }
                    let current = if cross_provider {
                        format!("{}{}", provider_prefix, current)
                    } else {
                        current
                    };
                    for m in &mut app.model_picker.models {
                        m.is_current = m.id == current;
                    }
                    app.model_picker.loading_models = false;
                    app.model_fetch_rx = None;
                }
                Ok(Err(())) | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    app.model_picker.loading_models = false;
                    app.model_fetch_rx = None;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
        }

        // Tell the remote client when a turn starts or stops. Without it the
        // only clue is the delta stream, so a turn that is thinking or running
        // a slow tool looks identical to an idle session.
        if let Some(runtime) = bridge_runtime.as_ref() {
            if app.is_streaming != bridge_busy_sent {
                let _ = runtime.outbound_tx.try_send(BridgeOutbound::SessionBusy {
                    busy: app.is_streaming,
                });
                bridge_busy_sent = app.is_streaming;
            }

            // Facts the session list shows, refreshed only when they move.
            // Cost is rounded first: it creeps with every model call, and
            // comparing the raw float would re-register on each one.
            let info = mikmik_bridge::SessionInfo {
                model: Some(session_model_string(&cmd_ctx.config, &model_registry)),
                permission_mode: Some(
                    format!("{:?}", cmd_ctx.config.permission_mode).to_lowercase(),
                ),
                cost_usd: Some((cost_tracker.total_cost_usd() * 10_000.0).round() / 10_000.0),
                title: session.title.clone(),
            };
            if bridge_info_sent.as_ref() != Some(&info) {
                let _ = runtime
                    .outbound_tx
                    .try_send(BridgeOutbound::SessionInfo(info.clone()));
                bridge_info_sent = Some(info);
            }
        }

        // The dialog also closes on a keyboard answer. Drop the correlation id
        // so a late remote answer cannot match a question already settled.
        if pending_question_id.is_some() && !app.ask_user_dialog.visible {
            pending_question_id = None;
        }

        // Drain ask-user question events (non-blocking).
        // When the AskUserQuestion tool fires, it sends a UserQuestionEvent
        // here.  We open the dialog and the user's answer travels back via
        // the embedded oneshot channel.
        if let Some(ref mut rx) = app.user_question_rx {
            match rx.try_recv() {
                Ok(event) => {
                    // A question blocks the turn on a channel with no timeout,
                    // exactly like a permission request, so the remote client
                    // has to be told or a remotely-driven session stalls here.
                    let question_id = uuid::Uuid::new_v4().to_string();
                    if let Some(ref runtime) = bridge_runtime {
                        let _ = runtime.outbound_tx.try_send(BridgeOutbound::UserQuestion {
                            question_id: question_id.clone(),
                            question: event.question.clone(),
                            options: event.options.clone().unwrap_or_default(),
                        });
                    }
                    pending_question_id = Some(question_id);
                    // The terminal may not be the window in front. Read the
                    // settings here rather than caching them at startup, so a
                    // toggle in the settings screen takes effect on the very
                    // next question.
                    mikmik_core::desktop_notify::notify(
                        &mikmik_core::config::Settings::load_sync().unwrap_or_default(),
                        mikmik_core::desktop_notify::NotifyEvent::QuestionAsked,
                        &event.question,
                    );
                    app.ask_user_dialog
                        .open(event.question, event.options, event.reply_tx);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    app.user_question_rx = None;
                }
            }
        }

        // Drain whatever a running tool has printed since the last frame.
        //
        // Every chunk waiting is taken, not one: a command that prints steadily
        // would otherwise fall a frame further behind on every read and show
        // output long after it was produced.
        if let Some(ref mut rx) = app.tool_output_rx {
            let mut chunks = Vec::new();
            loop {
                match rx.try_recv() {
                    Ok(chunk) => chunks.push(chunk),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        app.tool_output_rx = None;
                        break;
                    }
                }
            }
            if !chunks.is_empty() {
                for chunk in chunks {
                    if let Some(block) = app
                        .tool_use_blocks
                        .iter_mut()
                        .find(|b| b.id == chunk.tool_id)
                    {
                        block.push_live_output(&chunk.text);
                    }
                }
                app.invalidate_transcript();
            }
        }

        // Drain EnterPlanMode requests. The tool does not wait for an answer:
        // it only narrows what the model may do, so it needs no approval.
        if let Some(ref mut rx) = app.plan_mode_rx {
            let mut entered = None;
            loop {
                match rx.try_recv() {
                    Ok(event) => entered = Some(event),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        app.plan_mode_rx = None;
                        break;
                    }
                }
            }
            if let Some(event) = entered {
                app.enter_plan_mode();
                app.status_message = Some(match event.reason {
                    Some(reason) => format!("Plan mode: {}", reason),
                    None => "Plan mode.".to_string(),
                });
            }
        }

        // Carry a mode the model set itself onto the running turn. The key arm
        // below syncs the whole config, but only after a key press, and a
        // model-driven switch has none behind it.
        if sync_permission_mode(
            tool_ctx.permission_manager.as_ref(),
            &mut tool_ctx.config.permission_mode,
            app.config.permission_mode,
        ) {
            cmd_ctx.config.permission_mode = app.config.permission_mode;
        }

        // The tool roster the next turn is built from. Moved out of the key arm
        // for the same reason: `EnterPlanMode` sets `agent_mode_changed` with
        // no key press behind it, and a turn started before the rebuild would
        // otherwise be offered plan mode's tools a turn late.
        if app.agent_mode_changed {
            app.agent_mode_changed = false;
            let mode = app.agent_mode.as_deref().unwrap_or("build");
            let all_agents =
                mikmik_core::resolve_agents(&cmd_ctx.working_dir, &cmd_ctx.config.agents);
            if let Some(def) = all_agents.get(mode) {
                base_query_config.agent_name = Some(mode.to_string());
                base_query_config.agent_definition = Some(def.clone());
                // A command that reports a limit has to know which agent would
                // override it.
                cmd_ctx.active_agent = Some(def.clone());
                // The agent's own `max_turns` is not copied onto the query
                // config: `effective_max_turns` in the loop already prefers it,
                // and the dispatch site re-reads the session limit every turn,
                // so an assignment here would be overwritten and then ignored.
                tools_arc = filter_tools_for_agent(all_tools_arc.clone(), &def.access);
            } else {
                // "build" with no explicit definition = full access, no agent
                base_query_config.agent_name = None;
                base_query_config.agent_definition = None;
                cmd_ctx.active_agent = None;
                tools_arc = all_tools_arc.clone();
            }
        }

        // Drain plan approval events, the same way. ExitPlanMode is blocked on
        // the answer, so a plan that is never answered holds the turn open.
        if let Some(ref mut rx) = app.plan_approval_rx {
            match rx.try_recv() {
                Ok(event) => {
                    mikmik_core::desktop_notify::notify(
                        &mikmik_core::config::Settings::load_sync().unwrap_or_default(),
                        mikmik_core::desktop_notify::NotifyEvent::PlanReady,
                        &event.plan,
                    );
                    // The percentage names what clearing the context would
                    // free, so it comes from the same numbers the footer draws.
                    let context_pct = (app.context_window_size > 0).then(|| {
                        (app.context_used_tokens as f64 / app.context_window_size as f64 * 100.0)
                            as u64
                    });
                    let restore_mode = app.permission_mode_after_plan();
                    app.plan_approval_dialog.open(
                        event.plan,
                        event.plan_path,
                        restore_mode,
                        context_pct,
                        event.reply_tx,
                    );
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    app.plan_approval_rx = None;
                }
            }
        }

        // ctrl+g in the plan dialog. The editor needs the terminal back, and
        // this loop is the only place that can hand it over: drawing an editor
        // over the alternate screen leaves both unusable.
        if let Some(path) = app.plan_approval_dialog.take_edit_request() {
            let (editor, editor_hint) = mikmik_core::paths::preferred_editor();
            mikmik_tui::restore_terminal(&mut terminal).ok();
            let status = std::process::Command::new(&editor).arg(&path).status();
            terminal = mikmik_tui::setup_terminal(app.config.mouse_capture_enabled())?;
            app.kitty_keyboard_active = mikmik_tui::keyboard_enhancement_active();

            app.status_message = match status {
                // The dialog shows what the tool will read back when the user
                // answers, so it has to be refreshed from the file rather than
                // kept as the model wrote it.
                Ok(_) => match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        app.plan_approval_dialog.plan = text.trim().to_string();
                        None
                    }
                    Err(e) => Some(format!("Could not read the plan back: {e}")),
                },
                Err(e) => Some(format!("Could not launch '{editor}': {e}. {editor_hint}")),
            };
        }

        // Spawn async provider model-list fetch when requested.
        if app.model_picker_fetch_pending {
            app.model_picker_fetch_pending = false;
            let provider_id_str = app
                .model_picker_provider_id
                .clone()
                .or_else(|| app.config.provider.clone())
                .unwrap_or_else(|| "anthropic".to_string());
            let is_anthropic = provider_id_str == "anthropic";
            // For Anthropic, live `/v1/models` discovery is the authoritative set
            // the credential can use; intersect it with the rich catalog
            // projection so we keep context/cost metadata for known ids but drop
            // models the subscription/key can't serve (e.g. legacy claude-3.x).
            let anthropic_catalog: Vec<mikmik_tui::model_picker::ModelEntry> = if is_anthropic {
                mikmik_tui::model_picker::models_for_provider_from_registry(
                    "anthropic",
                    model_registry.as_ref(),
                )
            } else {
                Vec::new()
            };
            if let Some(ref registry) = app.provider_registry {
                let pid = mikmik_core::ProviderId::new(&provider_id_str);
                if let Some(provider) = registry.get(&pid) {
                    let provider = provider.clone();
                    // Layer user metadata overrides (issue #309) onto the
                    // live-discovered list too, so self-hosted / openai-compatible
                    // endpoints show the corrected context window in the picker.
                    let overrides = app.config.model_overrides.clone();
                    let provider_key = provider_id_str.clone();
                    let (tx, rx) = tokio::sync::mpsc::channel(1);
                    app.model_fetch_rx = Some(rx);
                    app.model_picker.loading_models = true;
                    tokio::spawn(async move {
                        // Effective context window for a discovered model:
                        // the user override wins, else the discovered value.
                        let ctx_for = |id: &str, discovered: u32| -> u32 {
                            overrides
                                .get(&format!("{}/{}", provider_key, id))
                                .and_then(|o| o.context_window)
                                .unwrap_or(discovered)
                        };
                        let name_for = |id: &str, discovered: &str| -> String {
                            overrides
                                .get(&format!("{}/{}", provider_key, id))
                                .and_then(|o| o.name.clone())
                                .unwrap_or_else(|| discovered.to_string())
                        };
                        match provider.discover_models().await {
                            Ok(models) => {
                                let entries: Vec<mikmik_tui::model_picker::ModelEntry> =
                                    if is_anthropic && !models.is_empty() {
                                        let by_id: std::collections::HashMap<
                                            String,
                                            mikmik_tui::model_picker::ModelEntry,
                                        > = anthropic_catalog
                                            .into_iter()
                                            .map(|e| (e.id.clone(), e))
                                            .collect();
                                        models
                                            .into_iter()
                                            .map(|m| {
                                                let id = m.id.to_string();
                                                by_id.get(&id).cloned().unwrap_or_else(|| {
                                                    mikmik_tui::model_picker::ModelEntry {
                                                        id: id.clone(),
                                                        display_name: name_for(&id, &m.name),
                                                        description:
                                                            mikmik_tui::model_picker::format_context_window(
                                                                ctx_for(&id, m.context_window),
                                                            ),
                                                        is_current: false,
                                                        provider_id: None,
                                                    }
                                                })
                                            })
                                            .collect()
                                    } else {
                                        models
                                            .into_iter()
                                            .map(|m| {
                                                let id = m.id.to_string();
                                                mikmik_tui::model_picker::ModelEntry {
                                                    display_name: name_for(&id, &m.name),
                                                    description:
                                                        mikmik_tui::model_picker::format_context_window(
                                                            ctx_for(&id, m.context_window),
                                                        ),
                                                    id,
                                                    is_current: false,
                                                    provider_id: None,
                                                }
                                            })
                                            .collect()
                                    };
                                let _ = tx.send(Ok(entries)).await;
                            }
                            Err(_) => {
                                let _ = tx.send(Err(())).await;
                            }
                        }
                    });
                }
            }
        }

        // Refresh task list if the overlay is visible.
        if app.tasks_overlay.visible {
            app.tasks_overlay.refresh_tasks(&mikmik_tools::TASK_STORE);
        }

        // Check if the background update task has reported a result.
        if app.update_available.is_none() {
            if let Ok(Some(version)) = update_rx.try_recv() {
                app.update_available = Some(version);
            }
        }

        // ---- Device code / OAuth auth: spawn background task when pending ----
        if let Some(provider_id) = app.device_auth_pending.take() {
            let _tx = device_auth_tx.clone();
            match provider_id.as_str() {
                "github-copilot" => {
                    let tx2 = device_auth_tx.clone();
                    // Use the OpenCode Copilot OAuth app (Ov23li8tweQw6odWQebz)
                    // which is registered and authorised for the Copilot API.
                    // Tokens from an unregistered app get "model not supported"
                    // on every model.
                    const COPILOT_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
                    tokio::spawn(async move {
                        // Step 1: Request device code
                        match mikmik_core::device_code::request_device_code(
                            COPILOT_CLIENT_ID,
                            "read:user",
                            "https://github.com/login/device/code",
                        )
                        .await
                        {
                            Ok(resp) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::GotCode {
                                        user_code: resp.user_code,
                                        verification_uri: resp.verification_uri,
                                        device_code: resp.device_code.clone(),
                                        interval: resp.interval,
                                    })
                                    .await;
                                // Step 2: Poll for access token
                                match mikmik_core::device_code::poll_for_token(
                                    COPILOT_CLIENT_ID,
                                    &resp.device_code,
                                    "https://github.com/login/oauth/access_token",
                                    resp.interval,
                                    300,
                                )
                                .await
                                {
                                    Ok(token) => {
                                        // Name the account after the GitHub
                                        // login so a second Copilot account is
                                        // filed separately instead of
                                        // overwriting the first. The token is
                                        // opaque, so the name has to be asked
                                        // for; when that call fails the flow
                                        // still completes under the provider
                                        // id, which is what it always did.
                                        let event =
                                            match mikmik_core::device_code::github_login(&token)
                                                .await
                                            {
                                                Ok(login) => DeviceAuthEvent::TokenReceivedFor {
                                                    token,
                                                    account_id: login,
                                                },
                                                Err(e) => {
                                                    tracing::warn!(
                                                        "could not read the GitHub login for this \
                                                     token ({e}); filing it under the provider id"
                                                    );
                                                    DeviceAuthEvent::TokenReceived(token)
                                                }
                                            };
                                        let _ = tx2.send(event).await;
                                    }
                                    Err(e) => {
                                        let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                            }
                        }
                    });
                }
                "anthropic-oauth" => {
                    let tx2 = device_auth_tx.clone();
                    // Claude Pro/Max subscription login: claude.ai OAuth (Bearer).
                    // Runs the loopback flow in the background and surfaces the URL
                    // to the dialog; the flow persists the tokens itself
                    // (save_and_register), so the success handler only switches to
                    // the anthropic provider. Usage draws from the account's
                    // extra-usage pool, not subscription quota.
                    tokio::spawn(async move {
                        match oauth_flow::run_oauth_login_flow_tui(tx2.clone(), true, None).await {
                            Ok(_) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::TokenReceived("connected".to_string()))
                                    .await;
                            }
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "Anthropic login failed: {}",
                                        e
                                    )))
                                    .await;
                            }
                        }
                    });
                }
                "codex" | "openai-codex" => {
                    let tx2 = device_auth_tx.clone();
                    // Keep the dialog in WaitingForCode until GotBrowserUrl arrives.
                    // (set_browser_url() transitions it to BrowserAuth with the URL.)
                    tokio::spawn(async move {
                        match crate::codex_oauth_flow::run_oauth_flow(tx2.clone()).await {
                            Ok(tokens) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::TokenReceived(tokens.access_token))
                                    .await;
                            }
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "Codex OAuth failed: {}",
                                        e
                                    )))
                                    .await;
                            }
                        }
                    });
                }
                "kimi-code" => {
                    let tx2 = device_auth_tx.clone();
                    // Kimi Code device authorization grant. On success the task
                    // persists and registers the tokens itself, then reports the
                    // account it filed them under so the success handler can
                    // activate it without re-storing anything.
                    tokio::spawn(async move {
                        match mikmik_core::kimi_oauth::request_device_authorization().await {
                            Ok(device) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::GotCode {
                                        user_code: device.user_code,
                                        verification_uri: device.verification_uri_complete,
                                        device_code: device.device_code.clone(),
                                        interval: device.interval,
                                    })
                                    .await;
                                match mikmik_core::kimi_oauth::poll_for_token(
                                    &device.device_code,
                                    device.interval,
                                    device.expires_in,
                                )
                                .await
                                {
                                    Ok(tokens) => {
                                        let event = match mikmik_core::kimi_oauth::save_kimi_tokens_and_register(
                                            &tokens,
                                        ) {
                                            Ok(account_id) => DeviceAuthEvent::TokenReceivedFor {
                                                token: "connected".to_string(),
                                                account_id,
                                            },
                                            Err(e) => DeviceAuthEvent::Error(format!(
                                                "Kimi login could not be saved: {e}"
                                            )),
                                        };
                                        let _ = tx2.send(event).await;
                                    }
                                    Err(e) => {
                                        let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                            }
                        }
                    });
                }
                "xai-oauth" => {
                    let tx2 = device_auth_tx.clone();
                    // xAI Grok device authorization grant. The token endpoint is
                    // discovered from xAI's OIDC document before polling. On
                    // success the task persists and registers the tokens itself.
                    tokio::spawn(async move {
                        let discovery = mikmik_core::xai_oauth::discover_token_endpoint().await;
                        let token_endpoint = match discovery {
                            Ok(endpoint) => endpoint,
                            Err(e) => {
                                let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                                return;
                            }
                        };
                        match mikmik_core::xai_oauth::request_device_authorization().await {
                            Ok(device) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::GotCode {
                                        user_code: device.user_code,
                                        verification_uri: device.verification_uri_complete,
                                        device_code: device.device_code.clone(),
                                        interval: device.interval,
                                    })
                                    .await;
                                match mikmik_core::xai_oauth::poll_for_token(
                                    &token_endpoint,
                                    &device.device_code,
                                    device.interval,
                                    device.expires_in,
                                )
                                .await
                                {
                                    Ok(tokens) => {
                                        let event = match mikmik_core::xai_oauth::save_xai_tokens_and_register(
                                            &tokens,
                                        ) {
                                            Ok(account_id) => DeviceAuthEvent::TokenReceivedFor {
                                                token: "connected".to_string(),
                                                account_id,
                                            },
                                            Err(e) => DeviceAuthEvent::Error(format!(
                                                "xAI login could not be saved: {e}"
                                            )),
                                        };
                                        let _ = tx2.send(event).await;
                                    }
                                    Err(e) => {
                                        let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                            }
                        }
                    });
                }
                "gitlab-duo" => {
                    let tx2 = device_auth_tx.clone();
                    // GitLab Duo PKCE loopback flow: bind the fixed callback
                    // port the client id is registered with, open the browser,
                    // capture the code, exchange it, and register the account.
                    tokio::spawn(async move {
                        let verifier = match mikmik_core::oauth::generate_code_verifier() {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "GitLab PKCE setup failed: {e}"
                                    )))
                                    .await;
                                return;
                            }
                        };
                        let challenge = mikmik_core::oauth::generate_code_challenge(&verifier);
                        let state = uuid::Uuid::new_v4().to_string();
                        let port = mikmik_core::gitlab_duo::callback_port();
                        let listener =
                            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                                Ok(l) => l,
                                Err(e) => {
                                    let _ = tx2
                                        .send(DeviceAuthEvent::Error(format!(
                                            "GitLab callback server could not bind port {port}: {e}"
                                        )))
                                        .await;
                                    return;
                                }
                            };
                        let auth_url =
                            mikmik_core::gitlab_duo::build_authorize_url(&challenge, &state);
                        let _ = tx2
                            .send(DeviceAuthEvent::GotBrowserUrl { url: auth_url })
                            .await;

                        let code = match oauth_flow::run_callback_server(listener, &state).await {
                            Ok(code) => code,
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "GitLab callback failed: {e}"
                                    )))
                                    .await;
                                return;
                            }
                        };

                        match mikmik_core::gitlab_duo::exchange_code(&code, &verifier).await {
                            Ok(tokens) => {
                                let event =
                                    match mikmik_core::gitlab_duo::save_gitlab_tokens_and_register(
                                        &tokens,
                                    ) {
                                        Ok(account_id) => DeviceAuthEvent::TokenReceivedFor {
                                            token: "connected".to_string(),
                                            account_id,
                                        },
                                        Err(e) => DeviceAuthEvent::Error(format!(
                                            "GitLab login could not be saved: {e}"
                                        )),
                                    };
                                let _ = tx2.send(event).await;
                            }
                            Err(e) => {
                                let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                            }
                        }
                    });
                }
                "google-antigravity" => {
                    let tx2 = device_auth_tx.clone();
                    // Google OAuth loopback flow: bind the fixed callback port
                    // the desktop client's credentials are registered with, open
                    // the browser, capture the code, exchange it, resolve the
                    // Cloud Code project, and register the account.
                    tokio::spawn(async move {
                        let state = uuid::Uuid::new_v4().to_string();
                        let port = mikmik_core::antigravity_oauth::CALLBACK_PORT;
                        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port))
                            .await
                        {
                            Ok(l) => l,
                            Err(e) => {
                                let _ = tx2
                                        .send(DeviceAuthEvent::Error(format!(
                                            "Antigravity callback server could not bind port {port}: {e}"
                                        )))
                                        .await;
                                return;
                            }
                        };
                        let auth_url = mikmik_core::antigravity_oauth::authorize_url(&state);
                        let _ = tx2
                            .send(DeviceAuthEvent::GotBrowserUrl { url: auth_url })
                            .await;

                        let code = match oauth_flow::run_callback_server(listener, &state).await {
                            Ok(code) => code,
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "Antigravity callback failed: {e}"
                                    )))
                                    .await;
                                return;
                            }
                        };

                        let mut tokens =
                            match mikmik_core::antigravity_oauth::exchange_code(&code).await {
                                Ok(t) => t,
                                Err(e) => {
                                    let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                                    return;
                                }
                            };

                        match mikmik_core::antigravity_oauth::discover_project(&tokens.access_token)
                            .await
                        {
                            Ok(project) => tokens.project_id = Some(project),
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "Antigravity project provisioning failed: {e}"
                                    )))
                                    .await;
                                return;
                            }
                        }

                        let event =
                            match mikmik_core::antigravity_oauth::save_antigravity_tokens_and_register(
                                &tokens,
                            ) {
                                Ok(account_id) => DeviceAuthEvent::TokenReceivedFor {
                                    token: "connected".to_string(),
                                    account_id,
                                },
                                Err(e) => DeviceAuthEvent::Error(format!(
                                    "Antigravity login could not be saved: {e}"
                                )),
                            };
                        let _ = tx2.send(event).await;
                    });
                }
                "devin" => {
                    let tx2 = device_auth_tx.clone();
                    // Devin PKCE loopback flow: bind the fixed callback port the
                    // CLI sign-in page redirects to, open the browser, capture the
                    // code, exchange it for a session token, and register it.
                    tokio::spawn(async move {
                        let verifier = match mikmik_core::oauth::generate_code_verifier() {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "Devin PKCE setup failed: {e}"
                                    )))
                                    .await;
                                return;
                            }
                        };
                        let challenge = mikmik_core::oauth::generate_code_challenge(&verifier);
                        let state = uuid::Uuid::new_v4().to_string();
                        let port = mikmik_core::devin_oauth::CALLBACK_PORT;
                        let listener =
                            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                                Ok(l) => l,
                                Err(e) => {
                                    let _ = tx2
                                        .send(DeviceAuthEvent::Error(format!(
                                            "Devin callback server could not bind port {port}: {e}"
                                        )))
                                        .await;
                                    return;
                                }
                            };
                        let auth_url = mikmik_core::devin_oauth::authorize_url(&challenge, &state);
                        let _ = tx2
                            .send(DeviceAuthEvent::GotBrowserUrl { url: auth_url })
                            .await;

                        let code = match oauth_flow::run_callback_server(listener, &state).await {
                            Ok(code) => code,
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "Devin callback failed: {e}"
                                    )))
                                    .await;
                                return;
                            }
                        };

                        match mikmik_core::devin_oauth::exchange_code(&code, &verifier).await {
                            Ok(tokens) => {
                                let event =
                                    match mikmik_core::devin_oauth::save_devin_tokens_and_register(
                                        &tokens,
                                    ) {
                                        Ok(account_id) => DeviceAuthEvent::TokenReceivedFor {
                                            token: "connected".to_string(),
                                            account_id,
                                        },
                                        Err(e) => DeviceAuthEvent::Error(format!(
                                            "Devin login could not be saved: {e}"
                                        )),
                                    };
                                let _ = tx2.send(event).await;
                            }
                            Err(e) => {
                                let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                            }
                        }
                    });
                }
                "zai-oauth" => {
                    let tx2 = device_auth_tx.clone();
                    // Z.AI browser flow: bind the fixed callback port Z.AI's OAuth
                    // allowlist accepts (a conflict fails here rather than opening
                    // the browser with a redirect_uri Z.AI would reject), open the
                    // browser, capture the code, exchange it, and mint a durable
                    // API key that is stored as an ordinary zai account.
                    tokio::spawn(async move {
                        let state = uuid::Uuid::new_v4().to_string();
                        let port = mikmik_core::zai_oauth::CALLBACK_PORT;
                        let listener =
                            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                                Ok(l) => l,
                                Err(e) => {
                                    let _ = tx2
                                        .send(DeviceAuthEvent::Error(format!(
                                            "Z.AI callback server could not bind port {port}: {e}"
                                        )))
                                        .await;
                                    return;
                                }
                            };
                        let auth_url = mikmik_core::zai_oauth::authorize_url(&state);
                        let _ = tx2
                            .send(DeviceAuthEvent::GotBrowserUrl { url: auth_url })
                            .await;

                        let code = match oauth_flow::run_callback_server(listener, &state).await {
                            Ok(code) => code,
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "Z.AI callback failed: {e}"
                                    )))
                                    .await;
                                return;
                            }
                        };

                        match mikmik_core::zai_oauth::login(&code, &state).await {
                            Ok(login) => {
                                let event =
                                    match mikmik_core::zai_oauth::save_zai_key_and_register(&login)
                                    {
                                        Ok(account_id) => DeviceAuthEvent::TokenReceivedFor {
                                            token: "connected".to_string(),
                                            account_id,
                                        },
                                        Err(e) => DeviceAuthEvent::Error(format!(
                                            "Z.AI login could not be saved: {e}"
                                        )),
                                    };
                                let _ = tx2.send(event).await;
                            }
                            Err(e) => {
                                let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                            }
                        }
                    });
                }
                "cursor" => {
                    let tx2 = device_auth_tx.clone();
                    // Cursor PKCE poll flow: open the browser sign-in page, then
                    // poll api2.cursor.sh until the sign-in completes. No loopback
                    // callback — the token is delivered by the poll endpoint.
                    tokio::spawn(async move {
                        let params = match mikmik_core::cursor_oauth::generate_auth_params() {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = tx2
                                    .send(DeviceAuthEvent::Error(format!(
                                        "Cursor PKCE setup failed: {e}"
                                    )))
                                    .await;
                                return;
                            }
                        };
                        let _ = tx2
                            .send(DeviceAuthEvent::GotBrowserUrl {
                                url: params.login_url.clone(),
                            })
                            .await;

                        match mikmik_core::cursor_oauth::poll_for_token(
                            &params.uuid,
                            &params.verifier,
                        )
                        .await
                        {
                            Ok(tokens) => {
                                let event =
                                    match mikmik_core::cursor_oauth::save_cursor_tokens_and_register(
                                        &tokens,
                                    ) {
                                        Ok(account_id) => DeviceAuthEvent::TokenReceivedFor {
                                            token: "connected".to_string(),
                                            account_id,
                                        },
                                        Err(e) => DeviceAuthEvent::Error(format!(
                                            "Cursor login could not be saved: {e}"
                                        )),
                                    };
                                let _ = tx2.send(event).await;
                            }
                            Err(e) => {
                                let _ = tx2.send(DeviceAuthEvent::Error(e)).await;
                            }
                        }
                    });
                }
                _ => {
                    // Unknown provider for device auth — should not happen
                    app.device_auth_dialog
                        .set_error(format!("Unsupported auth flow for {}", provider_id));
                }
            }
        }

        // ---- Drain device auth events from the background task ----
        while let Ok(evt) = device_auth_rx.try_recv() {
            match evt {
                DeviceAuthEvent::GotCode {
                    user_code,
                    verification_uri,
                    device_code,
                    interval,
                } => {
                    // Auto-copy the user code to clipboard
                    let _ = mikmik_tui::try_copy_to_clipboard(&user_code);

                    // Auto-open the verification URL in the browser
                    let _ = open::that(&verification_uri);

                    app.device_auth_dialog.set_code(
                        user_code,
                        verification_uri,
                        device_code,
                        interval,
                    );

                    app.notifications.push(
                        mikmik_tui::NotificationKind::Info,
                        "Code copied to clipboard & browser opened.".to_string(),
                        Some(4),
                    );
                }
                DeviceAuthEvent::GotBrowserUrl { url } => {
                    // Copy the URL to clipboard so the user can paste it even
                    // when the automatic browser launch silently fails (headless
                    // terminals, tty2, Wayland-without-xdg-open, etc.).
                    let _ = mikmik_tui::try_copy_to_clipboard(&url);
                    app.device_auth_dialog.set_browser_url(url);
                    app.notifications.push(
                        mikmik_tui::NotificationKind::Info,
                        "Login URL copied to clipboard.".to_string(),
                        Some(5),
                    );
                }
                DeviceAuthEvent::TokenReceived(token) => {
                    app.device_auth_dialog.set_success(token);
                }
                DeviceAuthEvent::TokenReceivedFor { token, account_id } => {
                    app.device_auth_dialog
                        .set_success_for(token, Some(account_id));
                }
                DeviceAuthEvent::Error(msg) => {
                    app.device_auth_dialog.set_error(msg);
                }
            }
        }

        while let Ok(evt) = mcp_auth_rx.try_recv() {
            match evt {
                McpAuthEvent::Completed(result) => {
                    // Schedule a runtime rebuild so the newly persisted token is
                    // picked up by the next MCP manager instance.
                    app.pending_mcp_reconnect = true;
                    app.status_message = Some(format!(
                        "MCP OAuth — '{}' authentication completed; token saved to: {}",
                        result.server_name,
                        result.token_path.display()
                    ));
                }
                McpAuthEvent::Failed(error) => {
                    app.status_message = Some(format!("MCP OAuth failed: {}", error));
                }
            }
        }
        // Check if query task is done; sync messages from the task
        let task_finished = current_query
            .as_ref()
            .map(|(h, _)| h.is_finished())
            .unwrap_or(false);

        if task_finished {
            if let Some((handle, msgs_arc)) = current_query.take() {
                // Get the outcome and handle errors
                if let Ok(QueryOutcome::Error(err)) = handle.await {
                    while app.notifications.current_is_error() {
                        app.notifications.dismiss_current();
                    }
                    app.notifications.push(
                        mikmik_tui::notifications::NotificationKind::Error,
                        err.to_string(),
                        None,
                    );
                }
                // Sync the updated conversation back to our local vector
                messages = msgs_arc.lock().await.clone();
                // Before the session record is rebuilt from `messages`: the
                // recorder stamps a uuid onto any message that lacks one, and
                // the saved session has to carry the same values.
                if let Err(e) = transcript
                    .record_turn(&mut messages, &tool_ctx.working_dir)
                    .await
                {
                    app.notifications.push(
                        mikmik_tui::notifications::NotificationKind::Error,
                        format!("Could not write the session transcript: {e}"),
                        None,
                    );
                }
                session.messages = messages.clone();
                session.updated_at = chrono::Utc::now();
                session.model = session_model_string(&cmd_ctx.config, &model_registry);
                session.working_dir = Some(tool_ctx.working_dir.display().to_string());
                // A point to come back to, recorded once the turn is whole.
                mikmik_core::history::create_checkpoint(&mut session, None);
                // The whole query loop is done here, tool round-trips included.
                // `QueryEvent::TurnComplete` is the wrong hook: it fires once
                // per model turn, so a single prompt that calls five tools
                // would send five notifications.
                mikmik_core::desktop_notify::notify(
                    &mikmik_core::config::Settings::load_sync().unwrap_or_default(),
                    mikmik_core::desktop_notify::NotifyEvent::TurnComplete,
                    &last_assistant_text(&messages).unwrap_or_default(),
                );
                app.is_streaming = false;
                app.status_message = None;

                // The user approved a plan and asked for a clean slate first.
                // It happens here rather than when they answered, because a
                // summary written mid-turn would swallow the pending tool call
                // and leave the request with a tool_use nothing answers.
                if let Some(plan) = app.take_pending_plan_compaction() {
                    let replaced = compact_conversation(
                        Some(
                            "The user approved a plan and is about to have it \
                             implemented. Summarise what has been established \
                             so far, keeping every decision the plan depends on.",
                        ),
                        &mut messages,
                        &mut app,
                        &mut session,
                        &mut transcript,
                        &cmd_ctx.config,
                        client.as_ref(),
                        base_query_config.provider_registry.as_ref(),
                        &model_registry,
                        &tool_ctx.session_id,
                    )
                    .await;
                    if replaced {
                        if let Some(runtime) = bridge_runtime.as_ref() {
                            let (entries, omitted) = history_for_bridge(&messages);
                            let _ = runtime
                                .outbound_tx
                                .try_send(BridgeOutbound::History { entries, omitted });
                        }
                    }
                    // Queued rather than dispatched: the drain below is the one
                    // place a follow-up turn starts, and it already handles the
                    // prompt and the auto-submit.
                    app.queued_messages
                        .push_front(format!("Implement the approved plan:\n\n{plan}"));
                }

                // Drain one queued message into the prompt and request an
                // auto-submit on the next loop iteration (issue #149).
                if let Some(next) = app.queued_messages.pop_front() {
                    app.prompt_input.text = next;
                    app.prompt_input.cursor = app.prompt_input.text.len();
                    app.pending_auto_submit = true;
                }
                if let Err(e) = persist_session(&session).await {
                    app.notifications.push(
                        mikmik_tui::notifications::NotificationKind::Error,
                        format!("Could not save the session: {e}"),
                        None,
                    );
                }

                // --- Goal continuation (issue #230 / MI-3) ---
                // Continuation toward an active goal is now decided *inside*
                // run_query_loop by the goal continuation policy, so the REPL no
                // longer re-dispatches a follow-up turn here. All that remains
                // post-loop is to refresh the footer badge from the store: once
                // the loop returns the goal is paused / complete / budget-limited
                // (or absent), so this clears the badge in the common case. The
                // paused / budget / runaway notes are surfaced live from within
                // the loop via QueryEvent::Status.
                // One store read feeds both surfaces: the footer badge only
                // fills while the goal is still running, and the transcript
                // badge goes muted once it is complete.
                if mikmik_core::goals_enabled() {
                    let goal = mikmik_core::GoalStore::open_default()
                        .and_then(|s| s.get_goal(&session.id));
                    let (badge, completed) = goal_display_state(goal.as_ref());
                    app.active_goal_badge = badge;
                    app.goal_completed = completed;
                }
            }
        }

        if !app.is_streaming && current_query.is_none() {
            if let Some(server_name) = app.take_pending_mcp_panel_auth() {
                let server_config = cmd_ctx
                    .config
                    .mcp_servers
                    .iter()
                    .find(|server| server.name == server_name);
                let supports_panel_auth = server_config.is_some_and(|server| {
                    matches!(server.server_type.as_str(), "http" | "sse")
                        && server.url.as_deref().is_some()
                });

                if !supports_panel_auth {
                    app.status_message = Some(format!(
                        "Selected MCP server '{}' does not support panel auth.",
                        server_name
                    ));
                } else if let Some(manager) = app.mcp_manager.clone() {
                    match manager.begin_auth(&server_name).await {
                        Ok(session) => {
                            let auth_url = session.auth_url.clone();
                            let redirect_uri = session.redirect_uri.clone();
                            mcp_auth_runner(session);
                            app.status_message = Some(format!(
                                "MCP auth — '{}' started. Complete authentication in your browser.\nURL: {}\nCallback URL: {}",
                                server_name, auth_url, redirect_uri
                            ));
                        }
                        Err(error) => {
                            app.status_message =
                                Some(format!("MCP auth failed for '{}': {}", server_name, error));
                        }
                    }
                } else {
                    app.status_message = Some(
                        "MCP auth is unavailable because the MCP runtime is not connected."
                            .to_string(),
                    );
                }
            }
        }

        if !app.is_streaming && current_query.is_none() && app.take_pending_provider_reload() {
            // A provider was just connected in-session (e.g. a Claude Pro/Max
            // OAuth login). Re-resolve credentials and swap in a fresh client +
            // provider registry so the current session can use them immediately,
            // without a restart. The client built at startup had no credential.
            // `activate_provider` updated `app.config` (not `cmd_ctx.config`), so
            // snapshot it as the resolution source — and snapshot up-front so we
            // don't hold a borrow of `app` across the await.
            let reload_source = app.config.clone();
            match reload_provider_runtime_state(&reload_source).await {
                Ok(refreshed) => {
                    cmd_ctx.config = refreshed.config.clone();
                    tool_ctx.config = refreshed.config.clone();
                    base_query_config.provider_registry = Some(refreshed.provider_registry.clone());
                    base_query_config.model_registry = Some(refreshed.model_registry.clone());
                    base_query_config.model =
                        session_model_string(&cmd_ctx.config, refreshed.model_registry.as_ref());
                    client = refreshed.client;
                    model_registry = refreshed.model_registry;
                    session.model = session_model_string(&cmd_ctx.config, model_registry.as_ref());
                    app.provider_registry = Some(refreshed.provider_registry);
                    app.has_credentials = true;
                }
                Err(err) => {
                    app.status_message = Some(format!("Could not activate credentials: {}", err));
                }
            }
        }

        // Fill queued accounts' model lists from the accounts themselves.
        // Runs after the reload above, because discovery needs a provider that
        // can already reach the endpoint.
        for request in app.take_pending_model_sync() {
            let mikmik_tui::app::ModelSyncRequest {
                account: account_id,
                force,
            } = request;
            let provider = app.provider_registry.as_ref().and_then(|registry| {
                registry
                    .get(&mikmik_core::ProviderId::new(&account_id))
                    .cloned()
            });

            match provider {
                Some(provider) => match provider.discover_models().await {
                    Ok(models) if !models.is_empty() => {
                        match mikmik_api::model_sync::persist_account_models(
                            &account_id,
                            &models,
                            force,
                        ) {
                            Ok(outcome) => {
                                // Apply to every live config, or the session
                                // keeps refusing a model the endpoint just
                                // confirmed and the picker keeps the old list.
                                mikmik_api::model_sync::apply_model_sync(
                                    &mut app.config,
                                    &account_id,
                                    &outcome,
                                );
                                mikmik_api::model_sync::apply_model_sync(
                                    &mut cmd_ctx.config,
                                    &account_id,
                                    &outcome,
                                );
                                mikmik_api::model_sync::apply_model_sync(
                                    &mut tool_ctx.config,
                                    &account_id,
                                    &outcome,
                                );
                                app.status_message =
                                    Some(mikmik_api::model_sync::describe_model_sync(
                                        &account_id,
                                        &outcome,
                                    ))
                            }
                            Err(err) => {
                                app.status_message =
                                    Some(format!("{account_id}: could not save models: {err}"));
                            }
                        }
                    }
                    // An endpoint that lists nothing is not an error, but the
                    // account stays permissive rather than silently locked to
                    // an empty list.
                    Ok(_) => {
                        app.status_message = Some(format!(
                            "{account_id}: endpoint listed no models; \
                             set them by hand in settings.json."
                        ));
                    }
                    Err(err) => {
                        app.status_message = Some(format!(
                            "{account_id}: model discovery failed ({err}); \
                             set them by hand in settings.json."
                        ));
                    }
                },
                None => {
                    app.status_message = Some(format!(
                        "{account_id}: not reachable for model discovery; \
                         set its models by hand in settings.json."
                    ));
                }
            }
        }

        if !app.is_streaming && current_query.is_none() && app.take_pending_mcp_reconnect() {
            // Re-apply the project-MCP trust gate on reconnect: only user
            // servers plus project servers approved this session, persisted, or
            // globally trusted are launched (issue #123).
            let store = mikmik_core::mcp_trust::McpTrustStore::load();
            let decision = mikmik_core::mcp_trust::partition_mcp_servers(
                &cmd_ctx.config.mcp_servers,
                app.mcp_project_root.as_deref(),
                settings.trust_project_mcp_servers,
                &app.mcp_session_trusted,
                &store,
            );
            let new_mcp_manager = connect_mcp_manager_arc(&decision.allowed).await;
            tool_ctx.mcp_manager = new_mcp_manager.clone();
            app.mcp_manager = new_mcp_manager.clone();
            tools_arc = mikmik_query::build_tool_roster(
                new_mcp_manager.clone(),
                &tool_ctx.config,
                &tool_ctx.working_dir,
            );
            if app.mcp_view.visible {
                app.refresh_mcp_view();
            }

            let connected = new_mcp_manager
                .as_ref()
                .map(|manager| manager.server_count())
                .unwrap_or(0);
            app.status_message = Some(if cmd_ctx.config.mcp_servers.is_empty() {
                "No MCP servers configured.".to_string()
            } else {
                format!(
                    "Reconnected MCP runtime ({} connected server{}).",
                    connected,
                    if connected == 1 { "" } else { "s" }
                )
            });
        }

        // Ask whether the checkout's settings file may run what it declares
        // (#389), and install it if the user says so. Waits behind the same
        // startup dialog as the MCP prompt below.
        if !app.is_streaming && current_query.is_none() && !app.bypass_permissions_dialog.visible {
            app.maybe_prompt_project_trust();
        }
        if app.take_project_trust_granted() {
            if let Some(gated) = project_gated.take() {
                // Re-running the merge would also undo everything the session
                // changed since it started, so the approved set is folded into
                // the live configs instead. All three carry a copy.
                gated.install_into(&mut cmd_ctx.config);
                gated.install_into(&mut tool_ctx.config);
                gated.install_into(&mut app.config);
                let (session_commands, skill_count) =
                    session_slash_commands(&tool_ctx.working_dir, &cmd_ctx.config);
                app.set_extra_slash_commands(session_commands);
                app.skill_count = skill_count;
                tools_arc = mikmik_query::build_tool_roster(
                    app.mcp_manager.clone(),
                    &tool_ctx.config,
                    &tool_ctx.working_dir,
                );
            }
        }

        // Prompt for any project-defined MCP servers awaiting approval (#123).
        // Hold off while the startup bypass-permissions dialog is up so the two
        // modals don't fight over the screen.
        if !app.is_streaming
            && current_query.is_none()
            && !app.bypass_permissions_dialog.visible
            && app.maybe_prompt_next_mcp_server()
        {
            // Approving one of these launches a command on this machine, and
            // the dialog blocks the queue until it is answered, so a remote
            // client has to be able to answer it too.
            let request_id = uuid::Uuid::new_v4().to_string();
            if let (Some(runtime), Some(server)) =
                (bridge_runtime.as_ref(), app.mcp_prompting.as_ref())
            {
                let _ = runtime
                    .outbound_tx
                    .try_send(mcp_approval_request(&request_id, server));
            }
            pending_mcp_approval_id = Some(request_id);
        }

        // The dialog also closes on a keyboard answer. Drop the correlation id
        // so a late remote answer cannot settle a prompt already dealt with.
        if pending_mcp_approval_id.is_some() && !app.mcp_approval.visible {
            pending_mcp_approval_id = None;
        }

        // Report the notices raised this pass, and a settings write, to the
        // plugins. Both surfaces are synchronous, so this is where the async
        // side happens.
        for (kind, message) in app.drain_notification_outbox() {
            let hook_ctx = mikmik_core::hooks::HookContext {
                event: "Notification".to_string(),
                tool_name: None,
                tool_input: None,
                tool_output: Some(message.clone()),
                is_error: Some(kind == "error"),
                session_id: Some(tool_ctx.session_id.clone()),
            };
            mikmik_core::hooks::run_hooks(
                &cmd_ctx.config.hooks,
                mikmik_core::config::HookEvent::Notification,
                &hook_ctx,
                &tool_ctx.working_dir,
            )
            .await;
            mikmik_plugins::run_global_hook(
                mikmik_plugins::HookEventKind::Notification,
                None,
                serde_json::json!({ "kind": kind, "message": message }),
            )
            .await;
        }

        if app.settings_screen.saves() != settings_saves_seen {
            settings_saves_seen = app.settings_screen.saves();
            mikmik_plugins::run_global_hook(
                mikmik_plugins::HookEventKind::ConfigChange,
                None,
                serde_json::json!({ "source": "settings_screen" }),
            )
            .await;
        }

        if app.should_exit {
            break 'main;
        }
    }

    if let Some(runtime) = bridge_runtime.take() {
        runtime.cancel.cancel();
    }
    if let Some(status_line) = status_line {
        status_line.shutdown();
    }

    // Interpreters started by the REPL tool are kept alive between calls on
    // purpose; this is where that purpose ends.
    mikmik_tools::repl_tool::shutdown_session(&tool_ctx.session_id).await;
    mikmik_tools::computer_script::shutdown_session(&tool_ctx.session_id).await;
    // A language server holds the whole project in memory and outlives the
    // session otherwise: its manager is a global, so nothing else stops it.
    // `shutdown` asks first and kills the process tree after, so a server that
    // spawned a compiler does not leave it behind.
    mikmik_core::lsp::global_lsp_manager()
        .lock()
        .await
        .shutdown_all()
        .await;
    // Which problems this session already reported is session state too.
    mikmik_tools::lsp_after_write::forget_session(&tool_ctx.session_id).await;
    // Which conditional rules already spoke is session state too.
    mikmik_core::rules::forget_session(&tool_ctx.session_id);
    // The auto-compact circuit breaker is keyed by session, so it has to be
    // dropped here or a long-lived process keeps one entry per session it ran.
    mikmik_query::compact::forget_compact_state(&tool_ctx.session_id);

    mikmik_plugins::run_global_hook(
        mikmik_plugins::HookEventKind::SessionEnd,
        None,
        serde_json::json!({ "session_id": tool_ctx.session_id }),
    )
    .await;

    restore_terminal(&mut terminal)?;

    // `/restart`: relaunch this binary with the original flags plus
    // `--resume <session_id>`. The terminal is already restored and the
    // SessionEnd hook has run, so the child inherits a clean terminal.
    if app.restart_requested {
        let args = restart_argv(std::env::args_os(), &app.session_id);
        let exe = std::env::current_exe()?;
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // exec() replaces this process image and only returns on failure.
            let err = std::process::Command::new(&exe).args(&args).exec();
            return Err(anyhow::anyhow!("restart exec failed: {err}"));
        }
        #[cfg(not(unix))]
        {
            std::process::Command::new(&exe).args(&args).spawn()?;
            std::process::exit(0);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// `claude auth` subcommand handler
// ---------------------------------------------------------------------------
// Mirrors TypeScript cli.tsx `if (args[0] === 'auth') { ... }` fast-path.
// Called before Cli::parse() so it doesn't conflict with positional `prompt`.
//
// Usage:
//   claude auth login [--console]   — OAuth PKCE login (claude.ai by default)
//   claude auth logout              — Clear stored credentials
//   claude auth status [--json]     — Show authentication status

async fn handle_auth_command(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("login") => {
            // --console flag selects the Console OAuth flow (creates an API key)
            // Default (no flag) uses the Claude.ai flow (Bearer token)
            let login_with_claude_ai = !args.iter().any(|a| a == "--console");
            let label = extract_label_flag(args);
            println!("Starting authentication...");
            match oauth_flow::run_oauth_login_flow_with_label(
                login_with_claude_ai,
                label.as_deref(),
            )
            .await
            {
                Ok(result) => {
                    println!("Successfully logged in!");
                    if let Some(email) = &result.tokens.email {
                        println!("  Account: {}", email);
                    }
                    if result.use_bearer_auth {
                        println!("  Auth method: claude.ai");
                    } else {
                        println!("  Auth method: console (API key)");
                    }
                    if let Some(active) = active_account() {
                        println!("  Account: {}", active);
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Login failed: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Some("logout") => {
            auth_logout().await;
        }

        Some("status") => {
            let json_output = args.iter().any(|a| a == "--json");
            auth_status(json_output).await;
        }

        Some("list") | Some("ls") | Some("accounts") => {
            print_account_list(mikmik_core::ProviderId::ANTHROPIC, "Anthropic");
            std::process::exit(0);
        }

        Some("switch") | Some("use") => {
            let id = args.get(1).map(|s| s.as_str());
            switch_account(mikmik_core::ProviderId::ANTHROPIC, "Anthropic", id);
        }

        Some("remove") | Some("rm") => {
            let id = args.get(1).map(|s| s.as_str()).unwrap_or_else(|| {
                eprintln!("Usage: mikmik auth remove <profile-id>");
                std::process::exit(1);
            });
            remove_account(mikmik_core::ProviderId::ANTHROPIC, "Anthropic", id);
        }

        Some(unknown) => {
            eprintln!("Unknown auth subcommand: '{}'", unknown);
            eprintln!();
            print_auth_usage();
            std::process::exit(1);
        }

        None => {
            print_auth_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_auth_usage() {
    eprintln!("Usage: mikmik auth <subcommand>");
    eprintln!("  login [--console] [--label <name>]   Authenticate (claude.ai by default)");
    eprintln!("  logout                                Remove the active account's credentials");
    eprintln!("  status [--json]                       Show authentication status");
    eprintln!("  list                                  List all stored Anthropic accounts");
    eprintln!("  switch <profile-id>                   Make a stored account active");
    eprintln!("  remove <profile-id>                   Delete a stored account");
}

fn extract_label_flag(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--label" || a == "-l" {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix("--label=") {
            return Some(rest.to_string());
        }
    }
    None
}

/// The account the session is pointed at, whatever protocol it speaks.
fn active_account() -> Option<String> {
    mikmik_core::config::Settings::load_sync()
        .ok()
        .and_then(|settings| settings.provider)
}

fn print_account_list(provider: &str, display_name: &str) {
    let store = mikmik_core::AuthStore::load();
    let accounts = store.accounts_for_protocol(provider);
    let active = active_account();
    if accounts.is_empty() {
        println!("No {} accounts stored.", display_name);
        println!(
            "Use `mikmik {} login` to add one.",
            if provider == "anthropic" {
                "auth"
            } else {
                provider
            }
        );
        return;
    }
    println!("{} accounts:", display_name);
    for id in accounts {
        let marker = if active.as_deref() == Some(id.as_str()) {
            "*"
        } else {
            " "
        };
        // Identity comes from the credential, which is the only place it is
        // recorded now that there is no separate registry.
        let detail = match store.get(&id) {
            Some(mikmik_core::StoredCredential::AnthropicOAuth(tokens)) => {
                let tier = tokens
                    .subscription_type
                    .as_deref()
                    .map(|t| format!(" [{}]", t))
                    .unwrap_or_default();
                format!("{}  {}", tier, tokens.email.as_deref().unwrap_or(""))
            }
            Some(mikmik_core::StoredCredential::CodexOAuth(tokens)) => {
                format!("  {}", tokens.account_id.as_deref().unwrap_or(""))
            }
            Some(mikmik_core::StoredCredential::KimiOAuth(tokens)) => {
                format!("  {}", tokens.account_id.as_deref().unwrap_or(""))
            }
            Some(mikmik_core::StoredCredential::XaiOAuth(tokens)) => {
                format!("  {}", tokens.account_id.as_deref().unwrap_or(""))
            }
            Some(mikmik_core::StoredCredential::AntigravityOAuth(tokens)) => {
                format!("  {}", tokens.account_id.as_deref().unwrap_or(""))
            }
            Some(mikmik_core::StoredCredential::DevinOAuth(tokens)) => {
                format!("  {}", tokens.account_id.as_deref().unwrap_or(""))
            }
            Some(mikmik_core::StoredCredential::CursorOAuth(tokens)) => {
                format!("  {}", tokens.account_id.as_deref().unwrap_or(""))
            }
            // GitLab Duo tokens carry no readable identity; the account name is
            // all the listing shows, so they fall through to the empty detail.
            _ => String::new(),
        };
        println!("  {} {}{}", marker, id, detail.trim_end());
    }
}

fn switch_account(provider: &str, display_name: &str, id: Option<&str>) -> ! {
    let store = mikmik_core::AuthStore::load();
    let accounts = store.accounts_for_protocol(provider);

    let target = match id {
        Some(id) => id.to_string(),
        None => {
            if accounts.is_empty() {
                eprintln!("No {} accounts stored.", display_name);
                std::process::exit(1);
            }
            // No id: print the picker and exit with usage.
            eprintln!(
                "Usage: mikmik {} switch <account>",
                if provider == "anthropic" {
                    "auth"
                } else {
                    provider
                }
            );
            eprintln!();
            print_account_list(provider, display_name);
            std::process::exit(1);
        }
    };

    if !accounts.contains(&target) {
        eprintln!("No {} account '{}'.", display_name, target);
        eprintln!();
        print_account_list(provider, display_name);
        std::process::exit(1);
    }

    match mikmik_core::config::register_account(&target, provider, true) {
        Ok(()) => {
            println!("Switched to '{}'.", target);
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Failed to switch account: {}", e);
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// `mikmik codex` subcommand handler (account-level CLI)
// ---------------------------------------------------------------------------

async fn handle_codex_account_command(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("login") => {
            let label = extract_label_flag(args);
            // The Codex flow expects a TUI DeviceAuth dialog. For headless CLI
            // login we still spin up the OAuth listener but route the URL
            // through a no-op channel; the user opens the URL in their browser
            // either way.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<mikmik_tui::DeviceAuthEvent>(8);
            tokio::spawn(async move {
                while let Some(evt) = rx.recv().await {
                    if let mikmik_tui::DeviceAuthEvent::GotBrowserUrl { url } = evt {
                        println!("Opening browser for Codex authentication...");
                        println!("If the browser did not open, visit:\n\n  {}\n", url);
                    }
                }
            });
            match crate::codex_oauth_flow::run_oauth_flow_with_label(tx, label.as_deref()).await {
                Ok(_) => {
                    println!("Successfully logged in to Codex!");
                    if let Some(active) = active_account() {
                        println!("  Account: {}", active);
                    }
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Codex login failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some("logout") => match mikmik_core::oauth_config::clear_codex_tokens() {
            Ok(_) => {
                println!("Logged out of the active Codex account.");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Logout failed: {}", e);
                std::process::exit(1);
            }
        },
        Some("list") | Some("ls") | Some("accounts") => {
            print_account_list(mikmik_core::ProviderId::CODEX, "Codex");
            std::process::exit(0);
        }
        Some("switch") | Some("use") => {
            let id = args.get(1).map(|s| s.as_str());
            switch_account(mikmik_core::ProviderId::CODEX, "Codex", id);
        }
        Some("remove") | Some("rm") => {
            let id = args.get(1).map(|s| s.as_str()).unwrap_or_else(|| {
                eprintln!("Usage: mikmik codex remove <profile-id>");
                std::process::exit(1);
            });
            remove_account(mikmik_core::ProviderId::CODEX, "Codex", id);
        }
        Some("status") => {
            let store = mikmik_core::AuthStore::load();
            let accounts = store.accounts_for_protocol(mikmik_core::ProviderId::CODEX);
            if accounts.is_empty() {
                println!("Not logged in to Codex.");
                std::process::exit(1);
            }
            println!("Logged in to Codex.");
            let active = active_account();
            for id in accounts {
                let marker = if active.as_deref() == Some(id.as_str()) {
                    "*"
                } else {
                    " "
                };
                println!("  {} {}", marker, id);
            }
            std::process::exit(0);
        }
        Some(unknown) => {
            eprintln!("Unknown codex subcommand: '{}'", unknown);
            eprintln!();
            print_codex_usage();
            std::process::exit(1);
        }
        None => {
            print_codex_usage();
            std::process::exit(1);
        }
    }
}

fn print_codex_usage() {
    eprintln!("Usage: mikmik codex <subcommand>");
    eprintln!("  login [--label <name>]   Authenticate with ChatGPT/Codex");
    eprintln!("  logout                   Remove the active Codex credentials");
    eprintln!("  status                   Show Codex auth status");
    eprintln!("  list                     List all stored Codex accounts");
    eprintln!("  switch <profile-id>      Make a stored Codex account active");
    eprintln!("  remove <profile-id>      Delete a stored Codex account");
}

// ---------------------------------------------------------------------------
// `mikmik accounts` — unified read-only list across providers
// ---------------------------------------------------------------------------

/// The accounts `mikmik accounts --json` reports.
///
/// Names only: the values are live credentials, and printing them to stdout
/// would leak them into shell history and CI logs.
fn accounts_json(store: &mikmik_core::AuthStore, active: Option<&str>) -> serde_json::Value {
    let accounts: Vec<serde_json::Value> = store
        .credentials
        .iter()
        // The workspace session is not a model account: it authenticates
        // against the organisation's own server, which serves no completions,
        // and nothing here could select it. `/accounts` skips it for the same
        // reason, and `mikmik workspace status` is where it belongs.
        .filter(|(_, credential)| {
            !matches!(
                credential,
                mikmik_core::StoredCredential::WorkspaceSession { .. }
            )
        })
        .map(|(id, _)| {
            serde_json::json!({
                "account": id,
                "active": active == Some(id.as_str()),
            })
        })
        .collect();
    serde_json::json!({ "accounts": accounts })
}

fn handle_accounts_command(args: &[String]) {
    if args.iter().any(|a| a == "--json") {
        let store = mikmik_core::AuthStore::load();
        let active = active_account();
        println!(
            "{}",
            serde_json::to_string_pretty(&accounts_json(&store, active.as_deref()))
                .unwrap_or_else(|_| "{}".into())
        );
        return;
    }

    print_account_list(mikmik_core::ProviderId::ANTHROPIC, "Anthropic");
    println!();
    print_account_list(mikmik_core::ProviderId::CODEX, "Codex");
}

fn remove_account(provider: &str, display_name: &str, id: &str) -> ! {
    let mut store = mikmik_core::AuthStore::load();
    if !store
        .accounts_for_protocol(provider)
        .iter()
        .any(|a| a == id)
    {
        eprintln!("No {} account '{}' to remove.", display_name, id);
        std::process::exit(1);
    }
    store.remove(id);
    match mikmik_core::config::forget_account(id) {
        Ok(()) => {
            println!("Removed {} account '{}'.", display_name, id);
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!(
                "Removed the credential, but could not update settings.json: {}",
                e
            );
            std::process::exit(1);
        }
    }
}

fn provider_status_lookup_keys(provider_id: &str) -> Vec<&str> {
    match provider_id {
        "togetherai" | "together-ai" => vec!["togetherai", "together-ai"],
        "lmstudio" | "lm-studio" => vec!["lmstudio", "lm-studio"],
        "llamacpp" | "llama-cpp" | "llama-server" => vec!["llamacpp", "llama-cpp", "llama-server"],
        "moonshot" | "moonshotai" => vec!["moonshot", "moonshotai"],
        "zhipu" | "zhipuai" => vec!["zhipu", "zhipuai"],
        "vultr" | "vultr-ai" => vec!["vultr", "vultr-ai"],
        "google" | "google-vertex" => vec!["google", "google-vertex"],
        _ => vec![provider_id],
    }
}

fn format_provider_name(provider_id: &str) -> String {
    match provider_id {
        "anthropic" => "Anthropic".to_string(),
        "openai" => "OpenAI".to_string(),
        "google" => "Google".to_string(),
        "google-vertex" => "Google Vertex".to_string(),
        "github-copilot" => "GitHub Copilot".to_string(),
        "xai" => "xAI".to_string(),
        "lmstudio" | "lm-studio" => "LM Studio".to_string(),
        "llamacpp" | "llama-cpp" | "llama-server" => "llama.cpp".to_string(),
        other => other
            .split('-')
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Print current auth status, then exit with code 0 (logged in) or 1 (not logged in).
async fn auth_status(json_output: bool) {
    let settings = Settings::load().await.unwrap_or_default();
    let config = &settings.config;
    let active_provider = config.selected_provider_id();
    let provider_cfg = config
        .provider_configs
        .get(active_provider)
        .filter(|provider| provider.enabled);
    let auth_store = mikmik_core::AuthStore::load();
    let oauth_tokens = if active_provider == "anthropic" {
        mikmik_core::oauth::OAuthTokens::load().await
    } else {
        None
    };

    let env_api_key_source = mikmik_core::config::api_key_env_vars_for_provider(active_provider)
        .iter()
        .find_map(|env_var| {
            std::env::var(env_var)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|_| (*env_var).to_string())
        });
    let stored_api_key_source = provider_status_lookup_keys(active_provider)
        .into_iter()
        .find_map(|provider_id| match auth_store.get(provider_id) {
            Some(mikmik_core::StoredCredential::ApiKey { key, .. }) if !key.is_empty() => {
                Some("stored credential".to_string())
            }
            Some(mikmik_core::StoredCredential::OAuthToken {
                access, refresh, ..
            }) if active_provider == "github-copilot"
                && (!access.is_empty() || !refresh.is_empty()) =>
            {
                Some("stored token".to_string())
            }
            Some(mikmik_core::StoredCredential::KimiOAuth(tokens))
                if active_provider == mikmik_core::provider_id::ProviderId::KIMI_CODE
                    && !tokens.access_token.is_empty() =>
            {
                Some("stored token".to_string())
            }
            Some(mikmik_core::StoredCredential::XaiOAuth(tokens))
                if active_provider == mikmik_core::provider_id::ProviderId::XAI_OAUTH
                    && !tokens.access_token.is_empty() =>
            {
                Some("stored token".to_string())
            }
            Some(mikmik_core::StoredCredential::GitlabDuoOAuth(tokens))
                if active_provider == mikmik_core::provider_id::ProviderId::GITLAB_DUO
                    && !tokens.access_token.is_empty() =>
            {
                Some("stored token".to_string())
            }
            Some(mikmik_core::StoredCredential::AntigravityOAuth(tokens))
                if active_provider == mikmik_core::provider_id::ProviderId::GOOGLE_ANTIGRAVITY
                    && !tokens.access_token.is_empty() =>
            {
                Some("stored token".to_string())
            }
            Some(mikmik_core::StoredCredential::DevinOAuth(tokens))
                if active_provider == mikmik_core::provider_id::ProviderId::DEVIN
                    && !tokens.session_token.is_empty() =>
            {
                Some("stored token".to_string())
            }
            Some(mikmik_core::StoredCredential::CursorOAuth(tokens))
                if active_provider == mikmik_core::provider_id::ProviderId::CURSOR
                    && !tokens.access_token.is_empty() =>
            {
                Some("stored token".to_string())
            }
            _ => None,
        });

    let api_provider = format_provider_name(active_provider);
    let api_key_source = config
        .api_key
        .as_ref()
        .filter(|key| !key.is_empty())
        .map(|_| "settings.api_key".to_string())
        .or_else(|| {
            provider_cfg
                .and_then(|provider| provider.api_key.as_ref())
                .filter(|key| !key.is_empty())
                .map(|_| format!("settings.provider_configs.{active_provider}.api_key"))
        })
        .or(stored_api_key_source)
        .or(env_api_key_source)
        .or_else(|| {
            oauth_tokens
                .as_ref()
                .filter(|tokens| !tokens.uses_bearer_auth() && tokens.api_key.is_some())
                .map(|_| "/login managed key".to_string())
        });
    let token_source = oauth_tokens.as_ref().map(|tokens| {
        if tokens.uses_bearer_auth() {
            "claude.ai".to_string()
        } else {
            "console_oauth".to_string()
        }
    });
    let login_method = oauth_tokens
        .as_ref()
        .and_then(|tokens| subscription_label(tokens.subscription_type.as_deref()))
        .or_else(|| {
            oauth_tokens.as_ref().map(|tokens| {
                if tokens.uses_bearer_auth() {
                    "MikMik Account".to_string()
                } else {
                    "Console Account".to_string()
                }
            })
        })
        .or_else(|| api_key_source.as_ref().map(|_| "API Key".to_string()));
    let billing_mode = oauth_tokens.as_ref().map_or_else(
        || {
            if api_key_source.is_some() {
                "API".to_string()
            } else {
                "None".to_string()
            }
        },
        |tokens| {
            if tokens.uses_bearer_auth() {
                "Subscription".to_string()
            } else {
                "API".to_string()
            }
        },
    );

    let (auth_method, logged_in) = if let Some(ref tokens) = oauth_tokens {
        let method = if tokens.uses_bearer_auth() {
            "claude.ai"
        } else {
            "oauth_token"
        };
        (method.to_string(), true)
    } else if api_key_source.is_some() {
        ("api_key".to_string(), true)
    } else {
        ("none".to_string(), false)
    };

    if json_output {
        let mut obj = serde_json::json!({
            "loggedIn": logged_in,
            "authMethod": auth_method,
            "apiProvider": api_provider,
            "billing": billing_mode,
        });

        if let Some(ref source) = api_key_source {
            obj["apiKeySource"] = serde_json::Value::String(source.clone());
        }
        if let Some(ref source) = token_source {
            obj["tokenSource"] = serde_json::Value::String(source.clone());
        }
        if let Some(ref method) = login_method {
            obj["loginMethod"] = serde_json::Value::String(method.clone());
        }

        if let Some(ref tokens) = oauth_tokens {
            obj["email"] = json_null_or_string(&tokens.email);
            obj["orgId"] = json_null_or_string(&tokens.organization_uuid);
            obj["subscriptionType"] = json_null_or_string(&tokens.subscription_type);
        }

        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
    } else {
        if !logged_in {
            let hint = if active_provider == "anthropic" {
                "Run `mikmik auth login` or set ANTHROPIC_API_KEY.".to_string()
            } else if let Some(env_var) =
                mikmik_core::config::primary_api_key_env_var_for_provider(active_provider)
            {
                format!(
                    "Set {} or store a credential for {}.",
                    env_var, api_provider
                )
            } else {
                format!("Configure credentials for {}.", api_provider)
            };
            println!("Not logged in for {}. {}", api_provider, hint);
        } else {
            println!("Logged in.");
            println!("  API provider: {}", api_provider);
            println!("  Billing: {}", billing_mode);
            if let Some(ref method) = login_method {
                println!("  Login method: {}", method);
            }
            if let Some(ref source) = token_source {
                println!("  Auth token: {}", source);
            }
            if let Some(ref source) = api_key_source {
                println!("  API key: {}", source);
            }
            match auth_method.as_str() {
                "claude.ai" | "oauth_token" => {
                    if let Some(ref tokens) = oauth_tokens {
                        if let Some(ref email) = tokens.email {
                            println!("  Email: {}", email);
                        }
                        if let Some(ref org) = tokens.organization_uuid {
                            println!("  Organization ID: {}", org);
                        } else {
                            println!("  Organization ID: unavailable");
                        }
                        if let Some(ref sub) = tokens.subscription_type {
                            println!("  Subscription: {}", sub);
                        }
                    }
                }
                "api_key" => {
                    println!("  Organization ID: unavailable for direct API key auth");
                }
                _ => {}
            }
        }
    }

    std::process::exit(if logged_in { 0 } else { 1 });
}

/// Clear all stored credentials and exit.
async fn auth_logout() {
    let mut had_error = false;

    // Clear OAuth tokens
    if let Err(e) = mikmik_core::oauth::OAuthTokens::clear().await {
        eprintln!("Warning: failed to clear OAuth tokens: {}", e);
        had_error = true;
    }

    // Also clear any API key stored in settings.json
    match Settings::load().await {
        Ok(mut settings) => {
            if settings.config.api_key.is_some() {
                settings.config.api_key = None;
                if let Err(e) = settings.save().await {
                    eprintln!("Warning: failed to update settings.json: {}", e);
                    had_error = true;
                }
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to load settings.json: {}", e);
        }
    }

    if had_error {
        eprintln!("Logout completed with warnings.");
        std::process::exit(1);
    } else {
        println!("Successfully logged out from your Anthropic account.");
        std::process::exit(0);
    }
}

/// Helper: convert `Option<String>` to a JSON string or null.
fn subscription_label(subscription_type: Option<&str>) -> Option<String> {
    match subscription_type? {
        "enterprise" => Some("Claude Enterprise Account".to_string()),
        "team" => Some("Claude Team Account".to_string()),
        "max" => Some("Claude Max Account".to_string()),
        "pro" => Some("Claude Pro Account".to_string()),
        other if !other.is_empty() => Some(format!("{} Account", other)),
        _ => None,
    }
}

/// Helper: convert `Option<String>` to a JSON string or null.
fn json_null_or_string(opt: &Option<String>) -> serde_json::Value {
    match opt {
        Some(s) => serde_json::Value::String(s.clone()),
        None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod bare_mode_tests {
    //! Tests for issue #208: `--bare` must disable hooks, plugins, and
    //! AGENTS.md. The wiring lives inline in `main()`, so these tests exercise
    //! the underlying decisions/primitives that wiring relies on.
    use super::*;

    #[test]
    fn bare_flag_parses_and_implies_no_claude_md() {
        let cli = Cli::parse_from(["mikmik", "--bare"]);
        assert!(cli.bare, "--bare should set cli.bare");
        // main() computes `config.disable_claude_mds = cli.no_claude_md || cli.bare`,
        // so --bare must disable AGENTS.md even without --no-claude-md.
        assert!(
            cli.no_claude_md || cli.bare,
            "--bare must imply disable_claude_mds"
        );

        let normal = Cli::parse_from(["mikmik"]);
        assert!(!normal.bare, "bare defaults to false");
        assert!(
            !(normal.no_claude_md || normal.bare),
            "AGENTS.md stays enabled without --bare/--no-claude-md"
        );
    }

    /// Write a plugin directory whose manifest declares one MCP server and one
    /// language server, both named after the plugin.
    #[cfg(test)]
    fn write_contributing_plugin(root: &std::path::Path, name: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("plugin dir");
        std::fs::write(
            dir.join("plugin.json"),
            format!(
                r#"{{
                  "name": "{name}",
                  "version": "0.1.0",
                  "mcpServers": {{ "{name}": {{ "command": "true" }} }},
                  "lspServers": [ {{ "name": "{name}", "command": "true" }} ]
                }}"#
            ),
        )
        .expect("manifest");
    }

    #[cfg(test)]
    async fn registry_from(dir: &std::path::Path) -> mikmik_plugins::PluginRegistry {
        let mut registry = mikmik_plugins::PluginRegistry::new();
        let (plugins, errors) = mikmik_plugins::discover_plugins(
            &[dir.to_path_buf()],
            mikmik_plugins::PluginSource::User,
        )
        .await;
        registry.extend(plugins, errors);
        registry
    }

    #[tokio::test]
    async fn a_reload_drops_what_a_removed_plugin_contributed() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_contributing_plugin(tmp.path(), "alpha");
        write_contributing_plugin(tmp.path(), "beta");

        let before = registry_from(tmp.path()).await;
        let mut config = mikmik_core::Config::default();
        apply_plugin_contributions(&before, None, &mut config);
        assert_eq!(
            config.mcp_servers.len(),
            2,
            "both plugin servers registered"
        );
        assert_eq!(config.lsp_servers.len(), 2);

        // A server that came from the settings file, not from a plugin.
        config
            .mcp_servers
            .push(mikmik_core::config::McpServerConfig {
                name: "from-settings".to_string(),
                command: Some("true".to_string()),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
                url: None,
                headers: std::collections::HashMap::new(),
                server_type: "stdio".to_string(),
                origin: mikmik_core::config::McpServerOrigin::User,
            });

        std::fs::remove_dir_all(tmp.path().join("beta")).expect("remove plugin");
        let after = registry_from(tmp.path()).await;
        apply_plugin_contributions(&after, Some(&before), &mut config);

        let mcp_names: Vec<&str> = config.mcp_servers.iter().map(|s| s.name.as_str()).collect();
        assert!(
            mcp_names.contains(&"alpha"),
            "surviving plugin keeps its server"
        );
        assert!(
            !mcp_names.contains(&"beta"),
            "removed plugin loses its server"
        );
        assert!(
            mcp_names.contains(&"from-settings"),
            "a reload must not touch a server the settings file declared"
        );

        let lsp_names: Vec<&str> = config.lsp_servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(lsp_names, vec!["alpha"]);
    }

    #[tokio::test]
    async fn applying_the_same_registry_twice_keeps_one_copy() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_contributing_plugin(tmp.path(), "alpha");

        let registry = registry_from(tmp.path()).await;
        let mut config = mikmik_core::Config::default();
        apply_plugin_contributions(&registry, None, &mut config);
        apply_plugin_contributions(&registry, None, &mut config);

        assert_eq!(config.mcp_servers.len(), 1, "no duplicate MCP server");
        assert_eq!(config.lsp_servers.len(), 1, "no duplicate language server");
    }

    #[test]
    fn bare_mode_uses_empty_plugin_registry() {
        // In bare mode main() substitutes `PluginRegistry::new()` for
        // `load_plugins()`. Assert it contributes no plugins, commands, hooks,
        // or MCP servers downstream.
        let registry = mikmik_plugins::PluginRegistry::new();
        assert_eq!(registry.enabled_count(), 0, "no plugins enabled");
        assert!(registry.all_command_defs().is_empty(), "no plugin commands");
        let hook_count: usize = registry
            .build_hook_registry()
            .values()
            .map(|v| v.len())
            .sum();
        assert_eq!(hook_count, 0, "no plugin hooks");
        assert!(
            registry.all_mcp_servers().is_empty(),
            "no plugin MCP servers"
        );
    }

    #[test]
    fn bare_mode_clears_hooks() {
        use mikmik_core::config::{HookEntry, HookEvent};
        // Simulate settings-derived hooks, then apply the bare-mode clear that
        // main() performs. Every `run_hooks` call site guards on
        // `config.hooks.is_empty()`, so an empty map means nothing executes.
        let mut hooks: std::collections::HashMap<HookEvent, Vec<HookEntry>> =
            std::collections::HashMap::new();
        hooks.insert(
            HookEvent::PreToolUse,
            vec![HookEntry {
                command: "echo untrusted".to_string(),
                ..Default::default()
            }],
        );
        assert!(!hooks.is_empty(), "precondition: hooks are present");

        hooks.clear(); // mirrors `config.hooks.clear()` in bare mode

        assert!(hooks.is_empty(), "bare mode leaves no hooks to run");
    }
}

#[cfg(test)]
mod goal_display_state_tests {
    //! The footer badge and the transcript's muted goal block come from one
    //! store read. Before this, only the badge was fed and the muted variant
    //! could never trigger.
    use super::*;
    use mikmik_core::{Goal, GoalStatus};

    fn goal(status: GoalStatus) -> Goal {
        Goal {
            id: "g1".to_string(),
            session_id: "s1".to_string(),
            objective: "Migrate to React".to_string(),
            status,
            token_budget: None,
            tokens_used: 0,
            time_used_secs: 90,
            turns_used: 3,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn active_goal_fills_the_badge_and_is_not_completed() {
        let (badge, completed) = goal_display_state(Some(&goal(GoalStatus::Active)));
        let badge = badge.expect("an active goal must show a footer badge");
        assert!(badge.starts_with("active · "), "{badge:?}");
        assert!(badge.ends_with("3 turns"), "{badge:?}");
        assert!(!completed);
    }

    #[test]
    fn complete_goal_clears_the_badge_and_sets_the_flag() {
        let (badge, completed) = goal_display_state(Some(&goal(GoalStatus::Complete)));
        assert_eq!(badge, None);
        assert!(completed);
    }

    #[test]
    fn paused_and_budget_limited_goals_are_neither_shown_nor_completed() {
        for status in [GoalStatus::Paused, GoalStatus::BudgetLimited] {
            let goal = goal(status);
            let (badge, completed) = goal_display_state(Some(&goal));
            assert_eq!(badge, None, "{:?} must not fill the badge", goal.status);
            assert!(!completed, "{:?} must not read as complete", goal.status);
        }
    }

    #[test]
    fn no_goal_leaves_both_surfaces_empty() {
        let (badge, completed) = goal_display_state(None);
        assert_eq!(badge, None);
        assert!(!completed);
    }
}

#[cfg(test)]
mod dump_system_prompt_tests {
    //! `--dump-system-prompt` must print what a run actually sends. It used to
    //! print only the context attachments, so tool guidelines never appeared.
    use super::*;

    fn rendered_prompt(advisor_model: Option<&str>) -> String {
        let config = mikmik_core::config::Config {
            advisor_model: advisor_model.map(str::to_string),
            ..Default::default()
        };
        let model_registry = mikmik_api::ModelRegistry::new();
        let mut dump_config =
            mikmik_query::QueryConfig::from_config_with_registry(&config, &model_registry);
        dump_config.enabled_tools = Some(
            mikmik_query::build_tool_roster(None, &config, std::path::Path::new("."))
                .iter()
                .map(|tool| tool.name().to_string())
                .collect(),
        );
        match mikmik_query::build_system_prompt(&dump_config) {
            mikmik_api::SystemPrompt::Text(text) => text,
            mikmik_api::SystemPrompt::Blocks(blocks) => blocks
                .into_iter()
                .map(|block| block.text)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// `--system-prompt-file` was declared in the clap struct and never read,
    /// so it appeared in `--help` and silently did nothing.
    #[test]
    fn a_system_prompt_file_replaces_the_base_prompt() {
        let dir = std::env::temp_dir().join("mikmik-system-prompt-file-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("sp.txt");
        std::fs::write(&path, "You are a haiku bot.").expect("write");

        let contents = std::fs::read_to_string(&path).expect("read back");
        let config = Config {
            custom_system_prompt: Some(contents),
            ..Default::default()
        };
        let model_registry = mikmik_api::ModelRegistry::new();
        let mut dump_config =
            mikmik_query::QueryConfig::from_config_with_registry(&config, &model_registry);
        dump_config.system_prompt = config.custom_system_prompt.clone();

        let rendered = match mikmik_query::build_system_prompt(&dump_config) {
            mikmik_api::SystemPrompt::Text(text) => text,
            mikmik_api::SystemPrompt::Blocks(blocks) => blocks
                .into_iter()
                .map(|block| block.text)
                .collect::<Vec<_>>()
                .join("\n"),
        };

        std::fs::remove_dir_all(&dir).ok();
        assert!(
            rendered.contains("You are a haiku bot."),
            "the file's contents must reach the prompt, got: {rendered}"
        );
    }

    #[test]
    fn the_dump_carries_tool_guidelines() {
        let prompt = rendered_prompt(None);
        assert!(
            prompt.contains("Read a file with the Read tool"),
            "the dump must include per-tool guidance, not just context"
        );
    }

    #[test]
    fn the_dump_reflects_the_advisor_setting() {
        assert!(!rendered_prompt(None).contains("Call Advisor"));
        assert!(rendered_prompt(Some("claude-opus-4-6")).contains("Call Advisor"));
    }
}

#[cfg(test)]
mod remote_control_config_tests {
    //! The relay token is what stops an outsider from running tools on this
    //! machine, so a half-configured relay must not start the bridge.
    use super::*;
    use mikmik_core::config::{RemoteControlSettings, MIN_REMOTE_TOKEN_LEN};
    use std::sync::Mutex;

    // `resolve_bridge_config` reads process-global env.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let keys = [
                "MIKMIK_BRIDGE_URL",
                "CLAUDE_BRIDGE_BASE_URL",
                "MIKMIK_BRIDGE_TOKEN",
                "CLAUDE_BRIDGE_OAUTH_TOKEN",
            ];
            let saved = keys
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(value) => std::env::set_var(k, value),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn settings_with(remote: Option<RemoteControlSettings>) -> Settings {
        Settings {
            remote_control: remote,
            ..Default::default()
        }
    }

    fn configured() -> RemoteControlSettings {
        RemoteControlSettings {
            url: "https://relay.example/".to_string(),
            token: "a".repeat(MIN_REMOTE_TOKEN_LEN),
            label: Some("workstation".to_string()),
        }
    }

    #[test]
    fn a_configured_relay_activates_the_bridge() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _env = EnvGuard::new();

        let config = resolve_bridge_config(&settings_with(Some(configured())), "", false, false)
            .expect("bridge is active");

        assert_eq!(
            config.server_url, "https://relay.example",
            "the trailing slash must be trimmed or every path gains a double slash"
        );
        assert_eq!(
            config.session_token.as_deref(),
            Some("a".repeat(MIN_REMOTE_TOKEN_LEN).as_str())
        );
        assert!(config.enabled);
    }

    #[test]
    fn a_short_token_refuses_to_start_the_bridge() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _env = EnvGuard::new();

        let weak = RemoteControlSettings {
            token: "hunter2".to_string(),
            ..configured()
        };

        assert!(
            resolve_bridge_config(&settings_with(Some(weak)), "", false, false).is_none(),
            "a weak secret must not reach the network"
        );
    }

    #[test]
    fn a_half_configured_relay_does_not_fall_back_to_the_anthropic_credential() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _env = EnvGuard::new();

        let no_url = RemoteControlSettings {
            url: String::new(),
            ..configured()
        };

        assert!(
            resolve_bridge_config(&settings_with(Some(no_url)), "oauth-token", true, false)
                .is_none(),
            "falling back would point the session credential at an unknown host"
        );
    }

    #[test]
    fn the_environment_overrides_the_configured_url() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _env = EnvGuard::new();
        std::env::set_var("MIKMIK_BRIDGE_URL", "http://localhost:8350");

        let config = resolve_bridge_config(&settings_with(Some(configured())), "", false, false)
            .expect("bridge is active");

        assert_eq!(
            config.server_url, "http://localhost:8350",
            "a temporary redirect during development must not need a settings edit"
        );
    }

    #[test]
    fn headless_never_starts_the_bridge() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _env = EnvGuard::new();

        assert!(
            resolve_bridge_config(&settings_with(Some(configured())), "", false, true).is_none()
        );
    }

    #[test]
    fn no_remote_section_leaves_the_old_behaviour_alone() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _env = EnvGuard::new();

        assert!(resolve_bridge_config(&settings_with(None), "", false, false).is_none());
    }
}

#[cfg(test)]
mod remote_turn_gate_tests {
    use super::remote_turn_can_start;

    #[test]
    fn an_idle_session_with_an_empty_prompt_box_starts_the_turn() {
        assert!(remote_turn_can_start(false, false, false, true));
    }

    #[test]
    fn the_dialog_is_reported_ahead_of_the_running_turn() {
        use super::remote_wait_reason;
        // A turn blocked on a permission prompt is both busy and blocked. The
        // operator can only act on the dialog, so that is what to say.
        assert_eq!(
            remote_wait_reason(true, true, true),
            Some("Queued: the terminal is waiting on a dialog.")
        );
        assert_eq!(
            remote_wait_reason(true, false, true),
            Some("Queued: a turn is already running.")
        );
        assert_eq!(
            remote_wait_reason(false, false, false),
            Some("Queued: someone is typing at the terminal.")
        );
        assert_eq!(remote_wait_reason(false, false, true), None);
    }

    #[test]
    fn each_condition_alone_is_enough_to_hold_the_prompt() {
        // Streaming: a second query would run alongside the first and both
        // would write to the same event channel.
        assert!(!remote_turn_can_start(true, false, false, true));
        // Still joining the previous task: starting now leaks its handle.
        assert!(!remote_turn_can_start(false, true, false, true));
        // Something on screen is waiting for a decision.
        assert!(!remote_turn_can_start(false, false, true, true));
        // The local user is mid-sentence; submitting would discard their text.
        assert!(!remote_turn_can_start(false, false, false, false));
    }
}

#[cfg(test)]
mod remote_slash_routing_tests {
    /// A remote prompt has to be recognised as a slash command by exactly the
    /// same rule the keyboard uses, or the two paths diverge on the edges.
    fn is_slash(content: &str) -> bool {
        content.trim_start().starts_with('/')
    }

    #[test]
    fn a_command_is_recognised_with_or_without_leading_space() {
        assert!(is_slash("/compact"));
        assert!(is_slash("  /model claude-haiku"));
        assert!(is_slash("\n/clear"));
    }

    #[test]
    fn a_prompt_that_merely_mentions_a_path_is_not_a_command() {
        assert!(!is_slash("look at src/main.rs"));
        assert!(!is_slash("what does / mean here"));
        assert!(!is_slash(""));
    }

    /// The refusal has to carry the way out, or it is only a "no".
    ///
    /// `/model` is the case that matters: the picker it would have opened
    /// exists to set a model, and the usage line is how to do that without it.
    #[test]
    fn a_terminal_only_answer_says_what_to_send_instead() {
        let notice = super::terminal_only_notice("model");
        assert!(notice.starts_with("/model answers with a view on the terminal."));
        assert!(notice.contains("/model"));
        assert!(notice.lines().count() > 1, "no usage came with it");
    }

    /// A view with no command behind it still gets a straight answer rather
    /// than the "Unknown command" it would otherwise collect.
    #[test]
    fn a_view_with_no_command_still_answers() {
        assert!(mikmik_commands::find_command("survey").is_none());
        assert_eq!(
            super::terminal_only_notice("survey"),
            "/survey answers with a view on the terminal."
        );
    }
}

#[cfg(test)]
mod bridge_usage_tests {
    use super::bridge_usage;
    use mikmik_core::types::UsageInfo;

    fn sample() -> UsageInfo {
        UsageInfo {
            input_tokens: 100_000,
            output_tokens: 20_000,
            cache_creation_input_tokens: 5_000,
            cache_read_input_tokens: 40_000,
        }
    }

    #[test]
    fn a_turn_is_priced_at_the_model_it_names() {
        // The regression: the turn was priced at the session model, so a turn
        // an agent definition or a fallback switch moved elsewhere reported a
        // cost that did not belong to it.
        let haiku = bridge_usage("claude-haiku-4-5", &sample(), 0.0);
        let opus = bridge_usage("claude-opus-4-6", &sample(), 0.0);
        assert!(
            haiku.cost_usd < opus.cost_usd,
            "haiku {:?} against opus {:?}",
            haiku.cost_usd,
            opus.cost_usd
        );
    }

    #[test]
    fn one_turn_costs_what_the_session_costs() {
        // On the first turn the two figures sit side by side on a phone. They
        // have to agree, or neither is believable.
        let tracker = mikmik_core::cost::CostTracker::new();
        let usage = sample();
        tracker.add_usage(
            "claude-haiku-4-5",
            mikmik_core::cost::ModelPricing::for_model("claude-haiku-4-5"),
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );

        let reported = bridge_usage("claude-haiku-4-5", &usage, tracker.total_cost_usd());
        assert_eq!(reported.cost_usd, reported.session_cost_usd);
    }

    #[test]
    fn the_token_counts_pass_through_unchanged() {
        let reported = bridge_usage("claude-sonnet-4-5", &sample(), 0.0);
        assert_eq!(reported.input_tokens, 100_000);
        assert_eq!(reported.output_tokens, 20_000);
        assert_eq!(reported.cache_creation_tokens, 5_000);
        assert_eq!(reported.cache_read_tokens, 40_000);
    }
}

#[cfg(test)]
mod session_snapshot_tests {
    use super::*;
    use mikmik_bridge::BridgeOutbound;
    use mikmik_core::types::Message;

    fn app() -> mikmik_tui::App {
        mikmik_tui::App::new(
            mikmik_core::config::Config::default(),
            mikmik_core::cost::CostTracker::new(),
        )
    }

    #[test]
    fn a_waiting_permission_is_announced_again() {
        // The one that matters most: a permission blocks its tool on a channel
        // with no timeout, so a client that cannot see the card cannot get the
        // session moving again.
        let mut app = app();
        app.permission_request = Some(mikmik_tui::dialogs::PermissionRequest::standard(
            "tool-1".into(),
            "Bash".into(),
            "rm -rf build".into(),
        ));

        let snapshot = session_snapshot(&app, None, None, None, &[]);
        match &snapshot[..] {
            [BridgeOutbound::PermissionRequest {
                request_id,
                tool_name,
                options,
                ..
            }] => {
                assert_eq!(request_id, "tool-1");
                assert_eq!(tool_name, "Bash");
                assert!(!options.is_empty(), "an answerable card needs options");
            }
            other => panic!("expected one permission, got {other:?}"),
        }
    }

    #[test]
    fn a_waiting_bypass_warning_is_announced_again() {
        // Nothing runs while this is up. A client that never saw it watches a
        // session that looks idle and stays idle.
        let mut app = app();
        app.bypass_permissions_dialog.show(false);

        let snapshot = session_snapshot(&app, None, None, Some("gate-1"), &[]);
        match &snapshot[..] {
            [BridgeOutbound::BypassWarning {
                request_id,
                message,
                options,
            }] => {
                assert_eq!(request_id, "gate-1");
                assert!(
                    message.contains("NOT ask for your approval"),
                    "the card says nothing about what is being granted: {message}"
                );
                assert_eq!(options.len(), 2, "a two-answer gate needs both answers");
                assert_eq!(options[1], "No, keep asking");
            }
            other => panic!("expected one bypass warning, got {other:?}"),
        }
    }

    #[test]
    fn the_startup_gate_says_that_declining_exits() {
        // The same warning means something different at startup, and answering
        // it from a browser has to know which one it is looking at.
        let mut app = app();
        app.bypass_permissions_dialog.show(true);

        let snapshot = session_snapshot(&app, None, None, Some("gate-1"), &[]);
        match &snapshot[..] {
            [BridgeOutbound::BypassWarning { options, .. }] => {
                assert_eq!(options[1], "No, exit");
            }
            other => panic!("expected one bypass warning, got {other:?}"),
        }
    }

    #[test]
    fn a_bypass_warning_with_no_id_is_not_announced() {
        // Without a correlation id an answer could not be matched back, so a
        // card would be drawn that nothing could settle.
        let mut app = app();
        app.bypass_permissions_dialog.show(false);

        assert!(session_snapshot(&app, None, None, None, &[]).is_empty());
    }

    #[test]
    fn a_waiting_question_is_announced_again_with_its_id() {
        let mut app = app();
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        app.ask_user_dialog.open(
            "Which branch?".into(),
            Some(vec!["main".into(), "dev".into()]),
            reply_tx,
        );

        let snapshot = session_snapshot(&app, Some("q-1"), None, None, &[]);
        match &snapshot[..] {
            [BridgeOutbound::UserQuestion {
                question_id,
                question,
                options,
            }] => {
                assert_eq!(question_id, "q-1");
                assert_eq!(question, "Which branch?");
                assert_eq!(options.len(), 2);
            }
            other => panic!("expected one question, got {other:?}"),
        }
    }

    #[test]
    fn a_question_with_no_id_is_left_out() {
        // The id correlates the answer. Announcing a question the runner
        // cannot match an answer to would offer a card that settles nothing.
        let mut app = app();
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        app.ask_user_dialog
            .open("Which branch?".into(), None, reply_tx);

        assert!(session_snapshot(&app, None, None, None, &[]).is_empty());
    }

    #[test]
    fn a_streaming_turn_holds_the_transcript_back() {
        // History replaces the transcript wholesale. Sent mid-turn it wipes
        // the bubble the deltas are still filling, and the deltas that follow
        // have nowhere to land.
        let mut app = app();
        app.is_streaming = true;

        let messages = vec![Message::user("hello"), Message::assistant("hi")];
        assert!(session_snapshot(&app, None, None, None, &messages).is_empty());

        app.is_streaming = false;
        let idle = session_snapshot(&app, None, None, None, &messages);
        assert!(matches!(idle[..], [BridgeOutbound::History { .. }]));
    }

    #[test]
    fn the_transcript_comes_before_the_prompt_it_would_wipe() {
        let mut app = app();
        app.permission_request = Some(mikmik_tui::dialogs::PermissionRequest::standard(
            "tool-1".into(),
            "Bash".into(),
            "ls".into(),
        ));

        let snapshot = session_snapshot(&app, None, None, None, &[Message::user("hello")]);
        assert!(
            matches!(
                snapshot[..],
                [
                    BridgeOutbound::History { .. },
                    BridgeOutbound::PermissionRequest { .. }
                ]
            ),
            "history must lead, or it replaces the card behind it"
        );
    }

    #[test]
    fn the_timeline_is_rebuilt_after_the_history_that_clears_it() {
        let mut app = app();
        app.timeline
            .add_running_tool("tool-1", "Reading", 10, "", "");
        app.timeline
            .add_running_tool("tool-2", "Editing", 20, "", "");

        let snapshot = session_snapshot(&app, None, None, None, &[Message::user("hello")]);
        assert!(
            matches!(
                snapshot[..],
                [
                    BridgeOutbound::History { .. },
                    BridgeOutbound::TimelineRow(_),
                    BridgeOutbound::TimelineRow(_),
                ]
            ),
            "a client clears its timeline on history, so the rows have to follow it"
        );
    }

    #[test]
    fn a_long_timeline_is_trimmed_before_it_is_replayed() {
        let mut app = app();
        for idx in 0..BRIDGE_TIMELINE_ROWS + 15 {
            app.timeline
                .add_running_tool(format!("tool-{idx}"), "Reading", 10, "", "");
        }

        let rows: Vec<_> = session_snapshot(&app, None, None, None, &[])
            .into_iter()
            .filter_map(|event| match event {
                BridgeOutbound::TimelineRow(row) => Some(row),
                _ => None,
            })
            .collect();

        assert_eq!(rows.len(), BRIDGE_TIMELINE_ROWS);
        assert_eq!(
            rows.first().map(|row| row.id.as_str()),
            Some("tool-15"),
            "the oldest rows are the ones dropped"
        );
    }
}

#[cfg(test)]
mod remote_attachment_tests {
    use super::*;
    use mikmik_bridge::BridgeAttachment;
    use mikmik_core::types::{ContentBlock, MessageContent};

    fn attachment(name: &str, mime: &str, content: &str) -> BridgeAttachment {
        BridgeAttachment {
            name: name.to_string(),
            content: content.to_string(),
            mime_type: Some(mime.to_string()),
        }
    }

    fn blocks(message: &mikmik_core::types::Message) -> Vec<ContentBlock> {
        match &message.content {
            MessageContent::Blocks(blocks) => blocks.clone(),
            MessageContent::Text(text) => vec![ContentBlock::Text { text: text.clone() }],
        }
    }

    #[test]
    fn a_prompt_without_attachments_stays_plain_text() {
        let message = remote_user_message("just a prompt", &[]);

        assert!(matches!(message.content, MessageContent::Text(ref t) if t == "just a prompt"));
    }

    #[test]
    fn an_image_becomes_a_block_the_model_can_see() {
        let message = remote_user_message(
            "what is wrong here",
            &[attachment("shot.png", "image/png", "aGVsbG8=")],
        );

        let blocks = blocks(&message);
        assert_eq!(blocks.len(), 2, "text plus image");
        match &blocks[1] {
            ContentBlock::Image { source } => {
                assert_eq!(source.source_type, "base64");
                assert_eq!(source.media_type.as_deref(), Some("image/png"));
                assert_eq!(source.data.as_deref(), Some("aGVsbG8="));
            }
            other => panic!("expected an image block, got {other:?}"),
        }
    }

    #[test]
    fn a_text_file_is_folded_into_the_prompt_under_its_name() {
        let message = remote_user_message(
            "review this",
            &[attachment("a.rs", "text/plain", "fn main() {}")],
        );

        let blocks = blocks(&message);
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("review this"));
                assert!(text.contains("--- a.rs ---"));
                assert!(text.contains("fn main() {}"));
            }
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    /// Base64 of a binary pushed through as text would be noise to the model
    /// and would hide that the file never really arrived.
    #[test]
    fn an_unsupported_type_is_named_rather_than_smuggled_through() {
        let message = remote_user_message(
            "open this",
            &[attachment("a.zip", "application/zip", "UEsDBA==")],
        );

        let blocks = blocks(&message);
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("a.zip"));
                assert!(text.contains("not sent"));
                assert!(!text.contains("UEsDBA=="));
            }
            other => panic!("expected a text block, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod notification_body_tests {
    use super::*;
    use mikmik_core::types::{ContentBlock, Message, MessageContent};

    #[test]
    fn the_body_is_the_last_thing_the_model_said() {
        let messages = vec![
            Message::assistant("an older answer"),
            Message::user("and then?"),
            Message::assistant("  the newest answer  "),
        ];

        assert_eq!(
            last_assistant_text(&messages).as_deref(),
            Some("the newest answer")
        );
    }

    #[test]
    fn tool_calls_are_not_read_as_text() {
        let mut message = Message::assistant(String::new());
        message.content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "Reading the file.".to_string(),
            },
            ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
        ]);

        assert_eq!(
            last_assistant_text(&[message]).as_deref(),
            Some("Reading the file.")
        );
    }

    #[test]
    fn a_turn_with_nothing_to_say_has_no_body() {
        // A turn that ended on a tool call alone: the notification still
        // fires, it just carries its title.
        let mut message = Message::assistant(String::new());
        message.content = MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: "t1".to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({}),
            thought_signature: None,
        }]);

        assert_eq!(last_assistant_text(&[message]), None);
        assert_eq!(last_assistant_text(&[Message::user("hello")]), None);
        assert_eq!(last_assistant_text(&[]), None);
    }
}

#[cfg(test)]
mod bridge_history_tests {
    use super::*;
    use mikmik_core::types::{ContentBlock, Message, MessageContent};

    fn assistant_with_tool(text: &str, tool: &str) -> Message {
        let mut message = Message::assistant(String::new());
        message.content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: text.to_string(),
            },
            ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: tool.to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
        ]);
        message
    }

    #[test]
    fn a_short_conversation_survives_whole() {
        let messages = vec![
            Message::user("add a test"),
            assistant_with_tool("On it.", "Edit"),
        ];

        let (entries, omitted) = history_for_bridge(&messages);

        assert_eq!(omitted, 0);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].text, "add a test");
        assert_eq!(entries[1].role, "assistant");
        assert_eq!(entries[1].tools, vec!["Edit".to_string()]);
    }

    /// A long run must not push the live events out of the relay's ring
    /// buffer, and the client has to be told the transcript is partial.
    #[test]
    fn a_long_conversation_is_bounded_and_reports_what_it_dropped() {
        let messages: Vec<Message> = (0..BRIDGE_HISTORY_TURNS + 7)
            .map(|i| Message::user(format!("turn {i}")))
            .collect();

        let (entries, omitted) = history_for_bridge(&messages);

        assert_eq!(entries.len(), BRIDGE_HISTORY_TURNS);
        assert_eq!(omitted, 7);
        assert_eq!(
            entries[0].text, "turn 7",
            "the newest turns are the kept ones"
        );
    }

    #[test]
    fn an_overlong_turn_is_truncated_visibly() {
        let messages = vec![Message::user("x".repeat(BRIDGE_HISTORY_CHARS + 500))];

        let (entries, _) = history_for_bridge(&messages);

        assert!(entries[0].text.ends_with("… (truncated)"));
        assert!(entries[0].text.chars().count() < BRIDGE_HISTORY_CHARS + 100);
    }

    /// A turn that carries only a tool result would render as an empty bubble.
    #[test]
    fn a_turn_with_nothing_to_show_is_skipped() {
        let mut message = Message::user(String::new());
        message.content = MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: mikmik_core::types::ToolResultContent::Text("ok".to_string()),
            is_error: None,
        }]);
        let messages = vec![message];

        assert!(history_for_bridge(&messages).0.is_empty());
    }
}

#[cfg(test)]
mod remote_permission_tests {
    use super::*;

    fn pending(
        tool_use_id: &str,
    ) -> (
        mikmik_tools::PendingPermissionRequest,
        tokio::sync::oneshot::Receiver<mikmik_core::permissions::PermissionDecision>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = mikmik_core::permissions::PermissionRequest {
            tool_name: "Bash".to_string(),
            description: "run a command".to_string(),
            details: None,
            is_read_only: false,
            path: Some("ls -la".to_string()),
            working_dir: None,
            allowed_roots: Vec::new(),
            context_description: None,
            input: None,
        };
        (
            mikmik_tools::PendingPermissionRequest {
                tool_use_id: tool_use_id.to_string(),
                request,
                reason: "needs approval".to_string(),
                decision_tx: Some(tx),
            },
            rx,
        )
    }

    fn store_with(
        id: &str,
    ) -> (
        ParkingMutex<mikmik_tools::PendingPermissionStore>,
        tokio::sync::oneshot::Receiver<mikmik_core::permissions::PermissionDecision>,
    ) {
        let (entry, rx) = pending(id);
        let store = mikmik_tools::PendingPermissionStore {
            waiting: std::collections::HashMap::from([(id.to_string(), entry)]),
            ..Default::default()
        };
        (ParkingMutex::new(store), rx)
    }

    #[test]
    fn a_remote_allow_releases_the_blocked_tool() {
        let (store, rx) = store_with("tool-1");

        assert!(settle_pending_permission(&store, None, "tool-1", Some('y')).is_some());

        assert_eq!(
            rx.blocking_recv().ok(),
            Some(mikmik_core::permissions::PermissionDecision::Allow)
        );
        assert!(store.lock().waiting.is_empty());
    }

    #[test]
    fn a_remote_deny_reaches_the_blocked_tool() {
        let (store, rx) = store_with("tool-2");

        let settlement = settle_pending_permission(&store, None, "tool-2", Some('n'))
            .expect("the prompt was waiting");
        assert!(settlement.denied, "a deny has to report itself as one");

        assert_eq!(
            rx.blocking_recv().ok(),
            Some(mikmik_core::permissions::PermissionDecision::Deny)
        );
    }

    #[test]
    fn answering_twice_is_reported_rather_than_silently_ignored() {
        let (store, _rx) = store_with("tool-3");

        assert!(settle_pending_permission(&store, None, "tool-3", Some('y')).is_some());
        assert!(settle_pending_permission(&store, None, "tool-3", Some('n')).is_none());
    }

    #[test]
    fn a_session_allow_is_recorded_on_the_manager() {
        let (store, _rx) = store_with("tool-4");
        let manager = Arc::new(std::sync::Mutex::new(
            mikmik_core::permissions::PermissionManager::new(
                mikmik_core::config::PermissionMode::Default,
                &mikmik_core::config::Settings::default(),
            ),
        ));

        assert!(settle_pending_permission(&store, Some(&manager), "tool-4", Some('Y')).is_some());

        let decision = manager
            .lock()
            .map(|m| m.evaluate("Bash", "run a command", Some("ls -la"), None, &[]))
            .ok();
        assert_eq!(
            decision,
            Some(mikmik_core::permissions::PermissionDecision::Allow)
        );
    }
}

#[cfg(test)]
mod plan_badge_tests {
    use super::*;
    use mikmik_core::config::PermissionMode;

    #[test]
    fn only_plan_mode_raises_the_badge() {
        assert!(plan_badge_for(PermissionMode::Plan));
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
        ] {
            assert!(!plan_badge_for(mode), "{mode:?}");
        }
    }

    #[test]
    fn leaving_plan_mode_for_bypass_drops_the_badge() {
        // `/yolo` is the first command to reach `permission_mode` through the
        // `ConfigChangeMessage` arm. Without the sync there, the badge would
        // stay on screen while nothing was in plan mode any more.
        let mut plan_mode = plan_badge_for(PermissionMode::Plan);
        assert!(plan_mode);
        plan_mode = plan_badge_for(PermissionMode::BypassPermissions);
        assert!(!plan_mode);
    }
}

#[cfg(all(test, unix))]
mod bang_command_tests {
    use super::*;

    /// Permission handler that refuses everything, to prove the bang path does
    /// not consult it.
    struct DenyAll;

    impl mikmik_core::permissions::PermissionHandler for DenyAll {
        fn check_permission(
            &self,
            _request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            mikmik_core::permissions::PermissionDecision::Deny
        }

        fn request_permission(
            &self,
            _request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            mikmik_core::permissions::PermissionDecision::Deny
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: std::sync::Arc::new(DenyAll),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "bang-command-test".to_string(),
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

    #[tokio::test]
    async fn the_block_carries_the_command_above_its_output() {
        let (text, style) = run_bang_command("echo hello", &ctx()).await;

        assert!(text.starts_with("$ echo hello\n"), "{text:?}");
        assert!(text.contains("hello"), "{text:?}");
        assert_eq!(style, mikmik_tui::app::SystemMessageStyle::Info);
    }

    #[tokio::test]
    async fn a_failing_command_is_drawn_as_a_warning() {
        let (text, style) = run_bang_command("exit 3", &ctx()).await;

        assert_eq!(style, mikmik_tui::app::SystemMessageStyle::Warning);
        assert!(text.contains("exit 3"), "{text:?}");
    }

    #[tokio::test]
    async fn the_session_shell_carries_cd_from_one_command_to_the_next() {
        // The point of running through the tool rather than a fresh process:
        // the typed command and the model's share one shell. The directory is
        // freshly made so no default working directory can satisfy the check.
        let dir = tempfile::tempdir().expect("temp dir");
        let marker = dir
            .path()
            .file_name()
            .expect("temp dir name")
            .to_string_lossy()
            .into_owned();

        run_bang_command(&format!("cd {}", dir.path().display()), &ctx()).await;
        let (text, _) = run_bang_command("pwd", &ctx()).await;

        assert!(text.contains(&marker), "{text:?}");
    }
}

#[cfg(test)]
mod permission_mode_tests {
    use super::*;

    /// The base prompt as a run would send it.
    const BASE_SYSTEM_PROMPT: &str = include_str!("system_prompt.txt");

    #[test]
    fn only_the_conditional_note_claims_gh_is_installed() {
        // `system_prompt.txt` reaches every run, including one on a machine
        // with no `gh`. A mention there would promise a binary that is not
        // present, and the model would spend a turn finding that out.
        assert!(
            !BASE_SYSTEM_PROMPT.contains("gh "),
            "the base prompt names gh, which is only true on some machines"
        );
        for needle in ["gh pr view", "gh pr diff", "gh issue view", "gh api"] {
            assert!(
                GH_SYSTEM_PROMPT_NOTE.contains(needle),
                "the note never mentions {needle}"
            );
        }
    }

    #[test]
    fn the_note_steers_the_model_off_the_rendered_page() {
        // Without this sentence the model still reaches for WebFetch, which is
        // the behaviour the note exists to replace.
        assert!(GH_SYSTEM_PROMPT_NOTE.contains("Do not fetch a github.com page"));
    }

    #[test]
    fn the_base_prompt_says_when_to_plan_and_how_to_finish() {
        // Nothing else tells the model when to plan. The tool description is
        // read once the model is already reaching for the tool, which is too
        // late to decide whether to reach for it.
        for needle in [
            "EnterPlanMode",
            "ExitPlanMode",
            "AskUserQuestion",
            "Do not plan when",
            // An approved plan must be driven through the task tools.
            "task list with TaskCreate",
        ] {
            assert!(
                BASE_SYSTEM_PROMPT.contains(needle),
                "the base prompt never mentions {needle}"
            );
        }
    }

    #[test]
    fn the_plan_agent_is_told_to_submit_its_plan() {
        // The plan agent prompt replaces the general guidance once plan mode is
        // on, so a model that entered plan mode reads this and nothing else
        // about how planning ends.
        let agents = mikmik_core::default_agents();
        let plan = agents.get("plan").expect("the plan agent is a default");
        let prompt = plan.prompt.as_deref().unwrap_or_default();

        assert!(
            prompt.contains("ExitPlanMode"),
            "nothing tells the plan agent to submit its plan: {prompt}"
        );
        assert!(
            prompt.contains("AskUserQuestion"),
            "nothing tells the plan agent to ask about what the request leaves open: {prompt}"
        );
    }

    fn manager(mode: PermissionMode) -> Arc<std::sync::Mutex<PermissionManager>> {
        Arc::new(std::sync::Mutex::new(PermissionManager::new(
            mode,
            &Settings::default(),
        )))
    }

    #[test]
    fn a_mode_the_model_set_reaches_the_running_turn() {
        // The turn decides by the shared manager. `EnterPlanMode` has no key
        // press behind it, so without this the turn kept allowing writes while
        // the model believed it was planning.
        let manager = manager(PermissionMode::BypassPermissions);
        let mut observed = PermissionMode::BypassPermissions;

        assert!(sync_permission_mode(
            Some(&manager),
            &mut observed,
            PermissionMode::Plan
        ));

        assert_eq!(observed, PermissionMode::Plan);
        assert_eq!(
            manager.lock().expect("manager").mode,
            PermissionMode::Plan,
            "the running turn still decides by the mode it started in"
        );
    }

    #[test]
    fn an_unchanged_mode_touches_nothing() {
        // Called every frame, so it has to be free when nothing moved.
        let manager = manager(PermissionMode::Default);
        let mut observed = PermissionMode::Default;

        assert!(!sync_permission_mode(
            Some(&manager),
            &mut observed,
            PermissionMode::Default
        ));
    }

    #[test]
    fn an_absent_flag_leaves_the_saved_mode_alone() {
        // The flag used to carry a default, so clap answered `Default`
        // whether or not it was passed and startup wrote that over the
        // settings file. A mode saved by `/yolo` was then gone by the next
        // launch.
        let cli = Cli::parse_from(["mikmik"]);
        assert!(cli.permission_mode.is_none());
    }

    #[test]
    fn a_given_flag_still_wins() {
        let cli = Cli::parse_from(["mikmik", "--permission-mode", "plan"]);
        assert_eq!(
            cli.permission_mode.map(PermissionMode::from),
            Some(PermissionMode::Plan)
        );
    }

    #[test]
    fn three_sources_reach_the_same_bypass_mode() {
        // The warning gate reads the resolved mode, so all three arrive at one
        // place rather than each needing to be found and wired separately.
        assert_eq!(
            startup_permission_mode(PermissionMode::Default, true, None),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            startup_permission_mode(
                PermissionMode::Default,
                false,
                Some(CliPermissionMode::BypassPermissions)
            ),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            startup_permission_mode(PermissionMode::BypassPermissions, false, None),
            PermissionMode::BypassPermissions
        );
    }

    #[test]
    fn the_flags_outrank_the_settings_file_and_each_other() {
        assert_eq!(
            startup_permission_mode(
                PermissionMode::BypassPermissions,
                true,
                Some(CliPermissionMode::Plan)
            ),
            PermissionMode::BypassPermissions,
            "--dangerously-skip-permissions must win over --permission-mode"
        );
        assert_eq!(
            startup_permission_mode(
                PermissionMode::BypassPermissions,
                false,
                Some(CliPermissionMode::Default)
            ),
            PermissionMode::Default,
            "--permission-mode must win over the settings file"
        );
        assert_eq!(
            startup_permission_mode(PermissionMode::AcceptEdits, false, None),
            PermissionMode::AcceptEdits,
            "with no flag the settings file decides"
        );
    }

    #[test]
    fn a_switch_into_bypass_is_warned_about_however_it_was_made() {
        // The gate reads the mode, so shift+tab, /yolo, /permissions set and
        // the settings file are all one case here.
        assert_eq!(
            bypass_gate_for(PermissionMode::BypassPermissions, false, false),
            BypassGate::Warn
        );
    }

    #[test]
    fn a_mode_that_is_not_bypass_is_recorded_to_go_back_to() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
        ] {
            assert_eq!(
                bypass_gate_for(mode, false, false),
                BypassGate::RememberMode,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn the_warning_is_not_repeated_once_it_has_been_answered() {
        assert_eq!(
            bypass_gate_for(PermissionMode::BypassPermissions, true, false),
            BypassGate::Nothing,
            "an accepted gate must not ask again"
        );
        assert_eq!(
            bypass_gate_for(PermissionMode::BypassPermissions, false, true),
            BypassGate::Nothing,
            "the dialog is already on screen"
        );
    }
}

#[cfg(test)]
mod accounts_listing_tests {
    use super::*;
    use mikmik_core::{AuthStore, StoredCredential};

    fn store() -> AuthStore {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "kendi-openrouter".to_string(),
            StoredCredential::api_key("a-key"),
        );
        store.credentials.insert(
            mikmik_core::auth_store::WORKSPACE_ACCOUNT.to_string(),
            StoredCredential::WorkspaceSession {
                url: "https://mikmik.firma.com".to_string(),
                token: "a-session-token".to_string(),
                expires: u64::MAX,
            },
        );
        store
    }

    #[test]
    fn the_workspace_session_is_not_listed_as_an_account() {
        // It authenticates against the organisation's own server, which serves
        // no completions, so nothing could select it. `/accounts` skips it for
        // the same reason, and the two surfaces read one store.
        let listed = accounts_json(&store(), None);
        let names: Vec<&str> = listed["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .filter_map(|entry| entry["account"].as_str())
            .collect();
        assert_eq!(names, vec!["kendi-openrouter"]);
    }

    #[test]
    fn the_listing_never_carries_the_credential_itself() {
        let text = serde_json::to_string(&accounts_json(&store(), Some("kendi-openrouter")))
            .expect("serialise");
        assert!(!text.contains("a-key"), "{text}");
        assert!(!text.contains("a-session-token"), "{text}");
        assert!(text.contains("\"active\":true"), "{text}");
    }
}
