// mikmik-tools: All tool implementations for MikMik.
//
// Each tool maps to a capability the LLM can invoke: running shell commands,
// reading/writing/editing files, searching codebases, fetching web pages, etc.

// type_complexity: the REPL tool holds a boxed session-callback map whose full
// type is unwieldy; a type alias would not meaningfully improve readability.
#![allow(clippy::type_complexity)]

use async_trait::async_trait;
use mikmik_core::cost::CostTracker;
use mikmik_core::permissions::{PermissionDecision, PermissionHandler, PermissionRequest};
use mikmik_core::types::ToolDefinition;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// Sub-modules – each contains a full tool implementation.
pub mod acp_agent;
pub mod advise;
pub mod advisor;
pub mod apply_patch;
pub mod ask_user;
pub mod batch_edit;
pub mod brief;
pub mod browser;
pub(crate) mod brush_background;
pub(crate) mod brush_bash;
pub mod bundled_skills;
pub mod computer_script;
pub mod computer_use;
pub mod config_tool;
pub mod cron;
pub mod edit_guard;
pub mod editor_host;
pub mod enter_plan_mode;
pub mod exit_plan_mode;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod formatter;
pub mod glob_tool;
pub mod goal_tool;
pub mod grep_tool;
pub mod hub;
pub mod image_tools;
pub mod learn_tool;
pub mod line_endings;
pub mod lsp_after_write;
pub mod lsp_tool;
pub mod mcp_auth_tool;
pub mod mcp_resources;
pub mod mcp_tool;
pub(crate) mod memory_append;
pub mod memory_backend;
pub mod memory_guard;
pub mod memory_tool;
pub mod monitor_tool;
pub mod notebook_edit;
pub mod output_filter;
pub mod powershell;
pub(crate) mod powershell_session;
pub mod pty_bash;
pub mod repl_tool;
pub mod retain_tool;
pub mod send_message;
pub mod skill_tool;
pub mod sleep;
pub mod synthetic_output;
pub mod tasks;
pub mod team_tool;
#[cfg(test)]
pub(crate) mod test_support;
pub mod todo_write;
pub mod tool_search;
pub mod web;
pub mod web_fetch;
pub mod web_search;
pub mod worktree;

// Re-exports for convenience.
pub use acp_agent::AcpAgentTool;
pub use advise::AdviseTool;
pub use advisor::AdvisorTool;
pub use apply_patch::ApplyPatchTool;
pub use ask_user::AskUserQuestionTool;
pub use batch_edit::BatchEditTool;
pub use brief::BriefTool;
pub use browser::BrowserTool;
pub use computer_script::ComputerScriptTool;
pub use computer_use::ComputerUseTool;
pub use config_tool::ConfigTool;
pub use cron::{CronCreateTool, CronDeleteTool, CronListTool};
pub use editor_host::{
    EditorCapabilities, EditorHost, TerminalId, TerminalOutput, TerminalRequest,
};
pub use enter_plan_mode::EnterPlanModeTool;
pub use exit_plan_mode::ExitPlanModeTool;
pub use file_edit::FileEditTool;
pub use file_read::FileReadTool;
pub use file_write::FileWriteTool;
pub use formatter::try_format_file;
pub use glob_tool::GlobTool;
pub use goal_tool::GoalTool;
pub use grep_tool::GrepTool;
pub use hub::HubTool;
pub use image_tools::{GenerateImageTool, InspectImageTool};
pub use learn_tool::LearnTool;
pub use lsp_tool::LspTool;
pub use mcp_auth_tool::McpAuthTool;
pub use mcp_resources::{ListMcpResourcesTool, ReadMcpResourceTool};
pub use mcp_tool::mcp_tools;
pub use memory_tool::MemoryTool;
pub use monitor_tool::MonitorTool;
pub use notebook_edit::NotebookEditTool;
pub use powershell::PowerShellTool;
pub use pty_bash::PtyBashTool;
pub use repl_tool::ReplTool;
pub use retain_tool::RetainTool;
pub use send_message::{
    drain_inbox, peek_inbox, register_main, register_named, AgentAddress, AgentMessage, InboxGuard,
    SendMessageTool, MAIN_NAME,
};
pub use skill_tool::SkillTool;
pub use sleep::SleepTool;
pub use synthetic_output::SyntheticOutputTool;
pub use tasks::{
    Task, TaskCreateTool, TaskGetTool, TaskListTool, TaskOutputTool, TaskStatus, TaskStopTool,
    TaskUpdateTool, TASK_STORE,
};
pub use team_tool::{register_agent_runner, AgentRunFn, TeamCreateTool, TeamDeleteTool};
pub use todo_write::TodoWriteTool;
pub use tool_search::ToolSearchTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use worktree::{EnterWorktreeTool, ExitWorktreeTool};

// ---------------------------------------------------------------------------
// AskUser question channel
// ---------------------------------------------------------------------------

/// Event sent through the TUI side-channel when the `AskUserQuestion` tool
/// needs to pause the query loop and collect a response from the user.
pub struct UserQuestionEvent {
    /// The question text to display.
    pub question: String,
    /// Optional predefined choices (for multiple-choice questions).
    pub options: Option<Vec<String>>,
    /// Send the user's answer back through this channel to resume execution.
    pub reply_tx: tokio::sync::oneshot::Sender<String>,
}

// ---------------------------------------------------------------------------
// Plan approval channel
// ---------------------------------------------------------------------------

/// What the user decided to do with a plan.
///
/// The two plain approvals do not name a permission mode, because the mode
/// they restore is the one plan mode was entered from and only the front end
/// knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanChoice {
    /// Summarise the conversation, then implement it.
    ApproveAndClearContext,
    /// Implement it now.
    Approve,
    /// Implement it, but ask before each edit.
    ApproveWithManualEdits,
    /// Do not implement it yet; the plan needs more work.
    KeepPlanning,
}

impl PlanChoice {
    /// Whether this answer means the work may start.
    pub fn is_approval(self) -> bool {
        !matches!(self, Self::KeepPlanning)
    }
}

/// The user's answer to a plan.
#[derive(Debug, Clone)]
pub struct PlanDecision {
    /// The option the user picked.
    pub choice: PlanChoice,
    /// Anything the user typed alongside it. Carried into the tool result on
    /// every choice, so a rejection reaches the model with its reason.
    pub note: Option<String>,
}

/// Sent when `EnterPlanMode` asks the session to stop being able to act.
///
/// No reply channel: the switch takes writing and command execution away, so
/// there is nothing to approve. The tool reports what it asked for and the
/// turn carries on under the new mode.
#[derive(Debug, Clone)]
pub struct EnterPlanModeEvent {
    /// Why the model wants to plan, when it said.
    pub reason: Option<String>,
}

/// A piece of output a tool produced while it was still running.
///
/// Sent through a side-channel rather than a `QueryEvent`, because the turn
/// loop has two dispatch arms and a new event variant would have to be added to
/// both to reach every provider. Only the frontend that owns the channel sees
/// these; the finished result still travels the normal way.
#[derive(Debug, Clone)]
pub struct ToolOutputChunk {
    /// The `tool_use` id of the call that produced it, so the frontend knows
    /// which block on screen the text belongs to.
    pub tool_id: String,
    /// The bytes as they arrived, undecorated.
    pub text: String,
}

/// Event sent through the TUI side-channel when `ExitPlanMode` needs the user
/// to approve a plan before the session leaves planning.
pub struct PlanApprovalEvent {
    /// The plan to show, as the model wrote it.
    pub plan: String,
    /// Where the plan was written, so the user can open it in an editor.
    /// `None` when it could not be written, and the dialog then offers no
    /// way to edit it.
    pub plan_path: Option<PathBuf>,
    /// Send the decision back through this channel to resume execution.
    pub reply_tx: tokio::sync::oneshot::Sender<PlanDecision>,
}

// ---------------------------------------------------------------------------
// Core trait & types
// ---------------------------------------------------------------------------

/// The result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// Content to send back to the model as the tool result.
    pub content: String,
    /// Whether this invocation was an error.
    pub is_error: bool,
    /// Optional structured metadata (for the TUI to render diffs, etc.).
    pub metadata: Option<Value>,
    /// How long the tool's own work took, in milliseconds.
    ///
    /// Stamped by `execute_tool` in `mikmik-query` rather than by the tool, so
    /// every tool is measured the same way and none has to remember to. It
    /// covers the `execute` call alone: a tool that waits for the user to
    /// approve it would otherwise report how long the user took to answer.
    ///
    /// `None` when nothing ran, which is what a cancelled call answers.
    pub duration_ms: Option<u64>,
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: None,
            duration_ms: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: None,
            duration_ms: None,
        }
    }

    pub fn with_metadata(mut self, meta: Value) -> Self {
        self.metadata = Some(meta);
        self
    }
}

/// Permission level required by a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLevel {
    /// No permission needed (read-only, purely informational).
    None,
    /// Read-only access to the filesystem or network.
    ReadOnly,
    /// Write access to the filesystem.
    Write,
    /// Arbitrary command execution.
    Execute,
    /// Potentially dangerous (e.g., bypass sandbox).
    Dangerous,
    /// Unconditionally forbidden — the action must never be executed regardless
    /// of permission mode.  Used by the bash tool (`PtyBashTool`) when the
    /// classifier identifies a `Critical`-risk command (e.g. `rm -rf /`,
    /// fork-bomb, `dd if=…`).
    Forbidden,
}

/// Walk `root` the way the Glob and Grep tools agree to walk it.
///
/// Reads `.gitignore`, `.ignore`, `.git/info/exclude` and the global git rules,
/// and never descends into a directory they exclude, so a build tree costs
/// nothing rather than filling the result window.
///
/// Two deliberate departures from the crate defaults:
///
/// - Hidden entries are kept. Being hidden is not an ignore rule, and dropping
///   them would hide `.github/workflows/` from both tools.
/// - `.git` is dropped anyway, because keeping hidden entries would otherwise
///   walk straight into it and there is nothing to find there.
///
/// `include_ignored` turns every standard filter off, which is what
/// `Config::include_ignored_files` is for.
pub(crate) fn ignore_aware_walk(root: &std::path::Path, include_ignored: bool) -> ignore::Walk {
    ignore_aware_walk_builder(root, include_ignored).build()
}

/// The same walk, run across every core.
///
/// For the Grep tool, which reads each file it reaches. One thread spends most
/// of its time waiting on the disk, and the search itself is the work; the
/// Glob tool has no such work and stays on the sequential walk.
pub(crate) fn ignore_aware_walk_parallel(
    root: &std::path::Path,
    include_ignored: bool,
) -> ignore::WalkParallel {
    ignore_aware_walk_builder(root, include_ignored).build_parallel()
}

fn ignore_aware_walk_builder(root: &std::path::Path, include_ignored: bool) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    if include_ignored {
        builder.standard_filters(false);
    }
    builder
        .hidden(false)
        .follow_links(true)
        .filter_entry(|entry| entry.file_name() != ".git");
    builder
}

#[derive(Debug)]
pub struct PendingPermissionRequest {
    pub tool_use_id: String,
    pub request: mikmik_core::permissions::PermissionRequest,
    pub reason: String,
    pub decision_tx: Option<tokio::sync::oneshot::Sender<PermissionDecision>>,
}

#[derive(Default)]
pub struct PendingPermissionStore {
    pub queue: VecDeque<PendingPermissionRequest>,
    pub waiting: HashMap<String, PendingPermissionRequest>,
}

/// Persistent shell state shared across Bash tool invocations within one session.
///
/// The bash tool (`PtyBashTool`) reads and writes this state on every call so
/// that `cd` and `export` commands persist across separate tool invocations, matching the
/// mental model described in the tool description ("the working directory
/// persists between commands").
#[derive(Debug, Clone, Default)]
pub struct ShellState {
    /// Current working directory as tracked by the shell state.
    /// Starts as the session's `working_dir`; updated after each `cd` command.
    pub cwd: Option<PathBuf>,
    /// Environment variable overrides exported by previous commands.
    pub env_vars: HashMap<String, String>,
}

impl ShellState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Process-global registry of shell states keyed by session_id.
/// This lets us persist cwd/env across Bash invocations without changing
/// the `ToolContext` struct (which is constructed in places we cannot modify).
static SHELL_STATE_REGISTRY: once_cell::sync::Lazy<
    dashmap::DashMap<String, Arc<parking_lot::Mutex<ShellState>>>,
> = once_cell::sync::Lazy::new(dashmap::DashMap::new);

/// Return the persistent `ShellState` for the given session, creating one if needed.
pub fn session_shell_state(session_id: &str) -> Arc<parking_lot::Mutex<ShellState>> {
    SHELL_STATE_REGISTRY
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(parking_lot::Mutex::new(ShellState::new())))
        .clone()
}

/// Process-global registry of embedded shells keyed by session id.
///
/// Held behind a `tokio::sync::Mutex` rather than the `parking_lot` one beside
/// it, because running a command is `async` and a `parking_lot` guard cannot be
/// held across an await.
static BRUSH_SESSIONS: once_cell::sync::Lazy<
    dashmap::DashMap<String, Arc<tokio::sync::Mutex<mikmik_shell::ShellSession>>>,
> = once_cell::sync::Lazy::new(dashmap::DashMap::new);

/// Return the embedded shell for this session, opening one on first use.
///
/// The shell is what makes `cd`, `export`, an alias and a shell function
/// outlive the command that set them, so it must be the same one every call.
pub async fn session_brush_shell(
    session_id: &str,
    working_dir: &std::path::Path,
    bundled: mikmik_core::config::BundledUtilities,
) -> anyhow::Result<Arc<tokio::sync::Mutex<mikmik_shell::ShellSession>>> {
    if let Some(existing) = BRUSH_SESSIONS.get(session_id) {
        return Ok(existing.clone());
    }
    let opened = mikmik_shell::ShellSession::new(
        &mikmik_shell::usable_working_dir(working_dir),
        bundled_policy(bundled),
    )
    .await?;
    // Another call may have opened one while this was awaiting. Whichever
    // landed first is the session's shell; the loser is dropped here, which is
    // correct because nothing has run in it yet.
    Ok(BRUSH_SESSIONS
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(opened)))
        .clone())
}

/// The shell's own name for a choice made in `settings.json`.
///
/// `mikmik-shell` does not depend on `mikmik-core`, so the two enums are
/// separate and this is where they meet.
fn bundled_policy(choice: mikmik_core::config::BundledUtilities) -> mikmik_shell::BundledUtilities {
    match choice {
        mikmik_core::config::BundledUtilities::Prefer => mikmik_shell::BundledUtilities::Prefer,
        mikmik_core::config::BundledUtilities::Fallback => mikmik_shell::BundledUtilities::Fallback,
    }
}

/// Process-global registry of PowerShell interpreters keyed by session id.
///
/// Beside [`BRUSH_SESSIONS`] and for the same reason: a variable, a `cd` and
/// an imported module have to outlive the command that made them, so the
/// interpreter has to be the same one every call.
static POWERSHELL_SESSIONS: once_cell::sync::Lazy<
    dashmap::DashMap<String, Arc<tokio::sync::Mutex<powershell_session::PowerShellSession>>>,
> = once_cell::sync::Lazy::new(dashmap::DashMap::new);

/// Return the PowerShell interpreter for this session, starting one on first
/// use.
pub(crate) fn session_powershell(
    session_id: &str,
    working_dir: &std::path::Path,
) -> anyhow::Result<Arc<tokio::sync::Mutex<powershell_session::PowerShellSession>>> {
    if let Some(existing) = POWERSHELL_SESSIONS.get(session_id) {
        return Ok(existing.clone());
    }
    let opened = powershell_session::PowerShellSession::open(working_dir)?;
    Ok(POWERSHELL_SESSIONS
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(opened)))
        .clone())
}

/// Forget this session's PowerShell interpreter.
///
/// A command that ran too long left the interpreter killed, and a killed one
/// answers nothing; the next call starts a new one.
pub(crate) fn drop_session_powershell(session_id: &str) {
    POWERSHELL_SESSIONS.remove(session_id);
}

/// Remove the shell state for a session (e.g. when the session ends).
///
/// Both stores: the embedded shell holds open descriptors and a child process
/// tree, so leaving it behind leaks more than a `HashMap` entry.
pub fn clear_session_shell_state(session_id: &str) {
    SHELL_STATE_REGISTRY.remove(session_id);
    BRUSH_SESSIONS.remove(session_id);
    POWERSHELL_SESSIONS.remove(session_id);
}

/// Return the `ShadowSnapshot` for `working_dir`, creating it on first call.
/// Returns `None` when git is unavailable or the directory is not in a git repo.
pub fn session_shadow(
    working_dir: &std::path::Path,
) -> Option<Arc<mikmik_core::snapshot::ShadowSnapshot>> {
    mikmik_core::snapshot::get_or_create(working_dir)
}

/// Drop the cached shadow snapshot for `working_dir` (e.g. when a session ends).
pub fn clear_session_shadow(working_dir: &std::path::Path) {
    mikmik_core::snapshot::remove(working_dir);
}

/// Write `contents` to `path` atomically: write to a temp file in the same
/// directory, then rename over the destination. A crash or disk-full mid-write
/// can never leave the destination truncated or half-written.
pub(crate) async fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(".{}.mikmik-tmp-{}", file_name, std::process::id()));

    tokio::fs::write(&tmp, contents).await?;
    // Preserve the original file's permissions (e.g. the executable bit on
    // Unix), which a fresh temp file would otherwise reset.
    if let Ok(meta) = tokio::fs::metadata(path).await {
        let _ = tokio::fs::set_permissions(&tmp, meta.permissions()).await;
    }
    match tokio::fs::rename(&tmp, path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// A cloneable handle for injecting notification messages into the next agent turn.
/// Used by background tasks with `notify_on_complete` to signal completion without polling.
#[derive(Clone)]
pub struct CompletionNotifier(Arc<dyn Fn(String) + Send + Sync>);

impl CompletionNotifier {
    pub fn new(f: impl Fn(String) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
    pub fn notify(&self, msg: String) {
        (self.0)(msg);
    }
}

/// The tool call a context was cloned for.
///
/// A tool is handed its own arguments, but not the id the model gave the call
/// nor a value it can pass on to something that needs the arguments as data
/// (a permission prompt that wants to show what it is approving, an editor
/// that wants to attach a terminal to the right call).
#[derive(Debug, Clone)]
pub struct ActiveToolCall {
    /// The `tool_use` id from the model's turn.
    pub id: String,
    /// The arguments the model supplied, as they arrived.
    pub input: serde_json::Value,
    /// Milliseconds this call spent waiting for the user to answer a permission
    /// prompt it raised from inside its own `execute` (a self-gating tool).
    ///
    /// The runner subtracts it from the call's measured duration, so a tool's
    /// reported time is its own work and not how long the user took to click.
    /// Shared behind an `Arc` so `request_permission` can add to the same
    /// counter the runner later reads. Zero for a tool that never prompts.
    pub permission_wait_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Shared context passed to every tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub permission_handler: Arc<dyn PermissionHandler>,
    pub cost_tracker: Arc<CostTracker>,
    pub session_id: String,
    pub file_history: Arc<parking_lot::Mutex<mikmik_core::file_history::FileHistory>>,
    /// What this session has read from each file, and what keeps failing
    /// against it. The reading tools fill it and the editing tools check it;
    /// see `mikmik_core::file_snapshot`. A context built with a fresh store
    /// has read nothing, so every guard correctly stays silent.
    pub file_snapshots: Arc<parking_lot::Mutex<mikmik_core::file_snapshot::FileSnapshotStore>>,
    pub current_turn: Arc<AtomicUsize>,
    /// If true, suppress interactive prompts (batch / CI mode).
    pub non_interactive: bool,
    /// Optional MCP manager for ListMcpResources / ReadMcpResource tools.
    pub mcp_manager: Option<Arc<mikmik_mcp::McpManager>>,
    /// Configured event hooks (PreToolUse, PostToolUse, etc.).
    pub config: mikmik_core::config::Config,
    /// Managed agent (manager-executor) configuration, if active.
    pub managed_agent_config: Option<mikmik_core::config::ManagedAgentConfig>,
    /// Optional notifier for injecting completion messages into the next agent turn.
    /// Set when the query loop has a command queue wired up.
    pub completion_notifier: Option<CompletionNotifier>,
    /// Queue used by interactive mode to surface permission dialogs to the TUI.
    pub pending_permissions: Option<Arc<parking_lot::Mutex<PendingPermissionStore>>>,
    /// Shared permission manager so the interactive loop can record session/persistent approvals.
    pub permission_manager:
        Option<Arc<std::sync::Mutex<mikmik_core::permissions::PermissionManager>>>,
    /// Channel for the `AskUserQuestion` tool to send questions to the TUI and
    /// receive the user's typed answer.  `None` in headless / non-interactive mode.
    pub user_question_tx: Option<tokio::sync::mpsc::UnboundedSender<UserQuestionEvent>>,
    /// Channel for `ExitPlanMode` to put a plan in front of the user and wait
    /// for a decision. `None` in headless / non-interactive mode, where the
    /// tool reports the plan and returns without blocking.
    pub plan_approval_tx: Option<tokio::sync::mpsc::UnboundedSender<PlanApprovalEvent>>,
    /// Channel a long-running tool writes its output to while it is still
    /// running. `None` in headless / non-interactive mode, and whenever
    /// `config.live_tool_output` is off, so nothing is produced that nothing
    /// would draw.
    pub tool_output_tx: Option<tokio::sync::mpsc::UnboundedSender<ToolOutputChunk>>,
    /// Channel `EnterPlanMode` uses to put the session into plan mode. `None`
    /// in headless / non-interactive mode, where nothing owns the mode, and the
    /// tool then says the switch did not happen rather than claiming it did.
    pub plan_mode_tx: Option<tokio::sync::mpsc::UnboundedSender<EnterPlanModeEvent>>,
    /// Channel the `Advise` tool puts a note on. Set only on a watching
    /// advisor's own context; `None` everywhere else, and `Advise` is then not
    /// registered at all, so no primary agent can advise itself.
    pub advisor_note_tx:
        Option<tokio::sync::mpsc::UnboundedSender<mikmik_core::advisor::AdvisorNote>>,
    /// The roster entry this context belongs to, when it belongs to a watcher.
    /// It rides on each note so the primary can say which watcher spoke.
    pub advisor_name: Option<String>,
    /// Cancellation token for the owning query loop (issue #218). The parallel
    /// tool executor selects on this to abandon in-flight tools when the user
    /// cancels, and long-running tools may observe it to bail out early. Defaults
    /// to a fresh disconnected token; `run_query_loop` rebinds it to the loop's
    /// actual token so cancellation propagates into tools and sub-agents.
    pub cancel_token: tokio_util::sync::CancellationToken,
    /// The call this context was cloned for, set by the tool dispatcher.
    /// `None` on the per-turn context that every call is cloned from, and on
    /// any context built outside the dispatcher.
    pub current_call: Option<Arc<ActiveToolCall>>,
    /// The client hosting this session's files and shell, when one does.
    /// `None` in a terminal, where the agent owns both.
    pub editor: Option<Arc<dyn editor_host::EditorHost>>,
    /// Where `SendMessage` reaches this agent, and who it may reach.
    /// Default-built contexts are unaddressable; `run_query_loop` fills this
    /// in for a top-level session and `AgentTool` for a sub-agent.
    pub inbox: send_message::AgentAddress,
}

impl ToolContext {
    /// A sink for output produced while the current call is still running.
    ///
    /// `None` unless a frontend asked for live output, the setting is on, and
    /// the dispatcher told this context which call it belongs to. Returning
    /// the closure rather than the channel keeps the tool from having to know
    /// its own id or repeat the three conditions.
    pub fn live_output_sink(&self) -> Option<impl Fn(&str) + Send + Sync + 'static> {
        if !self.config.live_tool_output {
            return None;
        }
        let tx = self.tool_output_tx.clone()?;
        let tool_id = self.current_call.as_ref()?.id.clone();
        Some(move |text: &str| {
            let _ = tx.send(ToolOutputChunk {
                tool_id: tool_id.clone(),
                text: text.to_string(),
            });
        })
    }

    /// Every directory this session can reach, by name.
    ///
    /// Derived from the working directory and the configured extra directories
    /// rather than stored, so it cannot drift from either.
    pub fn workspace_roots(&self) -> std::collections::BTreeMap<String, PathBuf> {
        mikmik_core::workspace::generate_root_names(
            &self.working_dir,
            &self.config.additional_dirs,
            &self.config.workspace_paths,
        )
    }

    /// Resolve a tool path argument.
    ///
    /// An absolute path is taken as-is, `&name` and `&name/relative` resolve
    /// against the named workspace root, and anything else resolves against
    /// the working directory.
    ///
    /// # Errors
    /// Returns a message naming the known roots when the path asks for a root
    /// that does not exist, so a mistyped root does not silently turn into a
    /// file the session was never pointed at.
    pub fn resolve_path(&self, path: &str) -> Result<PathBuf, String> {
        use mikmik_core::workspace::RootRef;

        if PathBuf::from(path).is_absolute() {
            return Ok(PathBuf::from(path));
        }

        let roots = self.workspace_roots();
        let unknown = |name: &str| {
            format!(
                "unknown workspace root \"&{name}\"; known roots: {}",
                roots
                    .keys()
                    .map(|known| format!("&{known}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        match mikmik_core::workspace::parse_root_ref(path, &roots) {
            RootRef::Root { name, relative } => match roots.get(name) {
                Some(root) if relative.is_empty() => Ok(root.clone()),
                Some(root) => Ok(root.join(relative)),
                None => Err(unknown(name)),
            },
            RootRef::Unknown(name) => Err(unknown(name)),
            RootRef::Plain => Ok(self.working_dir.join(path)),
        }
    }

    /// Read a file the user is working on.
    ///
    /// Through the client when one is hosting the files, so an unsaved buffer
    /// is what the tool sees rather than the older text on disk. Otherwise
    /// straight from disk.
    pub async fn read_text(&self, path: &std::path::Path) -> std::io::Result<String> {
        match &self.editor {
            Some(editor) if editor.capabilities().read_text_file => {
                editor.read_text_file(path).await
            }
            _ => tokio::fs::read_to_string(path).await,
        }
    }

    /// Write a file the user is working on.
    ///
    /// Through the client when one is hosting the files, so the change lands
    /// in the editor's undo stack instead of appearing underneath it.
    /// Otherwise atomically to disk.
    ///
    /// Binary content is always written to disk: the client's write carries
    /// text, and there is no lossless way to hand it bytes.
    pub async fn write_text(&self, path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
        if let Some(editor) = &self.editor {
            if editor.capabilities().write_text_file {
                if let Ok(text) = std::str::from_utf8(contents) {
                    return editor.write_text_file(path, text).await;
                }
            }
        }
        write_atomic(path, contents).await
    }

    fn permission_allowed_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.config.workspace_paths.clone();
        roots.extend(self.config.additional_dirs.clone());
        roots
    }

    fn build_permission_request(
        &self,
        tool_name: &str,
        description: &str,
        details: Option<String>,
        is_read_only: bool,
        path: Option<PathBuf>,
    ) -> PermissionRequest {
        PermissionRequest {
            tool_name: tool_name.to_string(),
            description: description.to_string(),
            details,
            is_read_only,
            path: path.map(|p| p.display().to_string()),
            working_dir: Some(self.working_dir.clone()),
            allowed_roots: self.permission_allowed_roots(),
            context_description: None,
            // Whatever the running call was given, so a prompt can show what
            // it is approving rather than only which tool asked.
            input: self.current_call.as_ref().map(|call| call.input.clone()),
        }
    }

    fn request_permission_inner(
        &self,
        request: PermissionRequest,
    ) -> Result<(), mikmik_core::error::ClaudeError> {
        let interactive_reason = request.details.clone();
        let decision = self.permission_handler.request_permission(&request);
        match decision {
            PermissionDecision::Allow | PermissionDecision::AllowPermanently => Ok(()),
            PermissionDecision::Ask { reason } if self.non_interactive => {
                Err(mikmik_core::error::ClaudeError::PermissionDenied(format!(
                    "Permission denied for tool '{}': {}",
                    request.tool_name,
                    interactive_reason.unwrap_or(reason)
                )))
            }
            PermissionDecision::Ask { reason } => {
                let Some(queue) = &self.pending_permissions else {
                    return Err(mikmik_core::error::ClaudeError::PermissionDenied(format!(
                        "Permission denied for tool '{}'",
                        request.tool_name
                    )));
                };

                let (tx, rx) = tokio::sync::oneshot::channel();
                queue.lock().queue.push_back(PendingPermissionRequest {
                    tool_use_id: format!(
                        "perm-{}-{}",
                        self.session_id,
                        self.current_turn.fetch_add(1, Ordering::Relaxed)
                    ),
                    request,
                    reason: interactive_reason.unwrap_or(reason),
                    decision_tx: Some(tx),
                });

                // Time the wait for the user's answer and attribute it to this
                // call, so the runner can subtract it: a self-gating tool that
                // prompts here would otherwise report the user's think time as
                // its own work.
                let waited_at = std::time::Instant::now();
                let decision = tokio::task::block_in_place(|| rx.blocking_recv());
                if let Some(call) = &self.current_call {
                    call.permission_wait_ms.fetch_add(
                        waited_at.elapsed().as_millis() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                match decision {
                    Ok(PermissionDecision::Allow | PermissionDecision::AllowPermanently) => Ok(()),
                    _ => Err(mikmik_core::error::ClaudeError::PermissionDenied(
                        "Permission denied by user".to_string(),
                    )),
                }
            }
            _ => Err(mikmik_core::error::ClaudeError::PermissionDenied(format!(
                "Permission denied for tool '{}'",
                request.tool_name
            ))),
        }
    }

    /// Check permissions for a tool invocation.
    pub fn check_permission(
        &self,
        tool_name: &str,
        description: &str,
        is_read_only: bool,
    ) -> Result<(), mikmik_core::error::ClaudeError> {
        let request =
            self.build_permission_request(tool_name, description, None, is_read_only, None);
        self.request_permission_inner(request)
    }

    pub fn check_permission_for_path(
        &self,
        tool_name: &str,
        description: &str,
        path: PathBuf,
        is_read_only: bool,
    ) -> Result<(), mikmik_core::error::ClaudeError> {
        let request =
            self.build_permission_request(tool_name, description, None, is_read_only, Some(path));
        self.request_permission_inner(request)
    }

    /// Like `check_permission` but also passes structured `details` text
    /// (e.g. a risk explanation) that the TUI permission dialog can display.
    pub fn check_permission_with_details(
        &self,
        tool_name: &str,
        description: &str,
        details: &str,
        is_read_only: bool,
    ) -> Result<(), mikmik_core::error::ClaudeError> {
        let request = self.build_permission_request(
            tool_name,
            description,
            Some(details.to_string()),
            is_read_only,
            None,
        );
        self.request_permission_inner(request).map_err(|_| {
            mikmik_core::error::ClaudeError::PermissionDenied(format!(
                "Permission denied for tool '{}': {}",
                tool_name, details
            ))
        })
    }

    pub fn path_is_within_workspace(&self, path: &std::path::Path) -> bool {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut roots =
            vec![std::fs::canonicalize(&self.working_dir)
                .unwrap_or_else(|_| self.working_dir.clone())];
        roots.extend(
            self.permission_allowed_roots()
                .into_iter()
                .map(|root| std::fs::canonicalize(&root).unwrap_or(root)),
        );
        roots.iter().any(|root| resolved.starts_with(root))
    }

    pub fn check_permission_with_details_and_path(
        &self,
        tool_name: &str,
        description: &str,
        details: &str,
        path: PathBuf,
        is_read_only: bool,
    ) -> Result<(), mikmik_core::error::ClaudeError> {
        let request = self.build_permission_request(
            tool_name,
            description,
            Some(details.to_string()),
            is_read_only,
            Some(path),
        );
        self.request_permission_inner(request).map_err(|_| {
            mikmik_core::error::ClaudeError::PermissionDenied(format!(
                "Permission denied for tool '{}': {}",
                tool_name, details
            ))
        })
    }

    pub fn current_turn_index(&self) -> usize {
        self.current_turn.load(Ordering::Relaxed)
    }

    pub fn record_file_change(
        &self,
        path: PathBuf,
        before_content: &[u8],
        after_content: &[u8],
        tool_name: &str,
    ) {
        self.file_history.lock().record_modification(
            path,
            before_content,
            after_content,
            self.current_turn_index(),
            tool_name,
        );
    }
}

/// The trait every tool must implement.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Human-readable name (matches the constant in mikmik_core::constants).
    fn name(&self) -> &str;

    /// One-line description shown to the LLM.
    fn description(&self) -> &str;

    /// The permission level the tool requires.
    fn permission_level(&self) -> PermissionLevel;

    /// Whether this tool performs its own permission gating inside `execute()`.
    ///
    /// - `false` (default): the central backstop in `execute_tool` is
    ///   responsible for gating this tool. Secure by default — a tool that
    ///   forgets to call `ctx.check_permission*` is still caught by the backstop
    ///   whenever its `permission_level()` is a gated level.
    /// - `true`: the tool already prompts for permission internally (it calls
    ///   `ctx.check_permission*` in `execute()`), so the central gate must NOT
    ///   also prompt — this prevents double-prompting.
    fn self_gates(&self) -> bool {
        false
    }

    /// Whether this tool is "advanced" / rarely used and a candidate for
    /// deferred (on-demand) disclosure rather than being sent in every request.
    ///
    /// Defaults to `false` (always disclosed). This is purely a metadata hint
    /// today — the prompt/tool-definition layer can read it to decide which
    /// tools to omit from the initial request and surface only via `ToolSearch`.
    ///
    // TODO(#233): wire this into the request assembly so `advanced()` tools are
    // omitted from the initial `tools` array and loaded on demand. That requires
    // a mutable *active tool set* tracked across turns inside `run_query_loop`,
    // which is under active refactor on other branches and intentionally left
    // untouched here. Land the tracking + gated re-injection there, then have
    // `all_tools()` callers filter on `advanced()` for the first request.
    fn advanced(&self) -> bool {
        false
    }

    /// JSON Schema describing the tool's input parameters.
    fn input_schema(&self) -> Value;

    /// Execute the tool with the given JSON input.
    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult;

    /// Produce a `ToolDefinition` suitable for sending to the API.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Whether `PowerShell` belongs in the roster on this machine.
///
/// Offering a tool the machine cannot run costs a turn: the model calls it,
/// the call fails, and the model works out what happened. Windows always has
/// it. Elsewhere it is there only if someone installed `pwsh`.
fn powershell_is_available() -> bool {
    powershell_session::available()
}

/// Return all built-in tools (excluding AgentTool, which lives in cc-query).
pub fn all_tools() -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(PtyBashTool),
        Box::new(FileReadTool),
        Box::new(FileEditTool),
        Box::new(FileWriteTool),
        Box::new(BatchEditTool),
        Box::new(ApplyPatchTool),
        Box::new(GlobTool),
        Box::new(GrepTool),
        Box::new(WebFetchTool),
        Box::new(WebSearchTool),
        Box::new(NotebookEditTool),
        Box::new(TaskCreateTool),
        Box::new(TaskGetTool),
        Box::new(TaskUpdateTool),
        Box::new(TaskListTool),
        Box::new(TaskStopTool),
        Box::new(TaskOutputTool),
        Box::new(TodoWriteTool),
        Box::new(AskUserQuestionTool),
        Box::new(EnterPlanModeTool),
        Box::new(ExitPlanModeTool),
        Box::new(SleepTool),
        Box::new(CronCreateTool),
        Box::new(CronDeleteTool),
        Box::new(CronListTool),
        Box::new(EnterWorktreeTool),
        Box::new(ExitWorktreeTool),
        Box::new(ListMcpResourcesTool),
        Box::new(ReadMcpResourceTool),
        Box::new(ToolSearchTool),
        Box::new(BriefTool),
        Box::new(ConfigTool),
        Box::new(SendMessageTool),
        Box::new(SkillTool),
        Box::new(LspTool),
        Box::new(ReplTool),
        Box::new(TeamCreateTool),
        Box::new(TeamDeleteTool),
        Box::new(SyntheticOutputTool),
        Box::new(McpAuthTool),
        Box::new(MonitorTool),
        Box::new(GoalTool),
        Box::new(BrowserTool),
        Box::new(GenerateImageTool),
        Box::new(InspectImageTool),
        Box::new(HubTool),
        // Both desktop tools need the feature; the roster then decides which
        // of them a session actually offers.
        #[cfg(feature = "computer-use")]
        Box::new(computer_use::ComputerUseTool),
        #[cfg(feature = "computer-use")]
        Box::new(computer_script::ComputerScriptTool),
    ];
    if powershell_is_available() {
        tools.push(Box::new(PowerShellTool));
    }
    tools
}

/// Find a tool by name (case-sensitive).
pub fn find_tool(name: &str) -> Option<Box<dyn Tool>> {
    all_tools().into_iter().find(|t| t.name() == name)
}

/// The tool names an agent `access` level allows, or `None` for unrestricted.
///
/// `full` returns `None` (every tool). `read-only` returns the names of tools
/// whose [`PermissionLevel`] is `ReadOnly`/`None`, plus `AskUserQuestion`.
/// `search-only` returns a fixed search set. This is the canonical
/// access-to-allowlist mapping; a sub-agent spawn feeds the result to its tool
/// allowlist and the `/agent` session persona filters its tools the same way.
pub fn access_tool_names(access: &str) -> Option<Vec<String>> {
    match access {
        "read-only" => Some(
            all_tools()
                .into_iter()
                .filter(|t| {
                    matches!(
                        t.permission_level(),
                        PermissionLevel::ReadOnly | PermissionLevel::None
                    ) || t.name() == "AskUserQuestion"
                })
                .map(|t| t.name().to_string())
                .collect(),
        ),
        "search-only" => Some(
            ["Grep", "Glob", "Read", "WebSearch", "WebFetch"]
                .iter()
                .map(|n| n.to_string())
                .collect(),
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct AskPermissionHandler {
        reason: String,
    }

    impl mikmik_core::permissions::PermissionHandler for AskPermissionHandler {
        fn check_permission(
            &self,
            _request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            mikmik_core::permissions::PermissionDecision::Ask {
                reason: self.reason.clone(),
            }
        }

        fn request_permission(
            &self,
            request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            self.check_permission(request)
        }
    }

    fn test_tool_context(
        handler: Arc<dyn mikmik_core::permissions::PermissionHandler>,
    ) -> ToolContext {
        use mikmik_core::config::Config;

        ToolContext {
            working_dir: PathBuf::from("/workspace"),
            permission_handler: handler,
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "test".to_string(),
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

    // ---- Tool registry tests ------------------------------------------------

    #[test]
    fn test_all_tools_non_empty() {
        let tools = all_tools();
        assert!(
            !tools.is_empty(),
            "all_tools() must return at least one tool"
        );
    }

    #[test]
    fn test_all_tools_have_unique_names() {
        let tools = all_tools();
        let mut names = std::collections::HashSet::new();
        for tool in &tools {
            assert!(
                names.insert(tool.name().to_string()),
                "Duplicate tool name: {}",
                tool.name()
            );
        }
    }

    #[test]
    fn test_all_tools_have_non_empty_descriptions() {
        for tool in all_tools() {
            assert!(
                !tool.description().is_empty(),
                "Tool '{}' has empty description",
                tool.name()
            );
        }
    }

    #[test]
    fn test_all_tools_have_valid_input_schema() {
        for tool in all_tools() {
            let schema = tool.input_schema();
            assert!(
                schema.is_object(),
                "Tool '{}' input_schema must be a JSON object",
                tool.name()
            );
            assert!(
                schema.get("type").is_some() || schema.get("properties").is_some(),
                "Tool '{}' schema missing type or properties",
                tool.name()
            );
        }
    }

    #[test]
    fn test_find_tool_found() {
        let tool = find_tool("Bash");
        assert!(tool.is_some(), "Should find the Bash tool");
        assert_eq!(tool.unwrap().name(), "Bash");
    }

    #[test]
    fn test_find_tool_not_found() {
        assert!(find_tool("NonExistentTool12345").is_none());
    }

    #[test]
    fn test_find_tool_case_sensitive() {
        // Tool names are case-sensitive — "bash" should not match "Bash"
        assert!(find_tool("bash").is_none());
        assert!(find_tool("Bash").is_some());
    }

    #[test]
    fn test_core_tools_present() {
        let expected = [
            "Bash",
            "Read",
            "Edit",
            "Write",
            "Glob",
            "Grep",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
            "Skill",
        ];
        for name in &expected {
            assert!(
                find_tool(name).is_some(),
                "Expected tool '{}' not found in all_tools()",
                name
            );
        }
    }

    #[test]
    fn powershell_is_offered_only_where_it_can_run() {
        // Offering a tool the machine cannot run costs a turn: the model calls
        // it, the call fails, and the model works out what happened.
        let offered = find_tool("PowerShell").is_some();
        assert_eq!(offered, powershell_is_available());
        if cfg!(windows) {
            assert!(offered, "Windows always has PowerShell");
        } else {
            assert_eq!(offered, which::which("pwsh").is_ok());
        }
    }

    #[test]
    fn bash_is_offered_on_every_platform() {
        // The embedded shell runs on Windows too, so the tool is no longer a
        // Unix-only promise.
        assert!(find_tool("Bash").is_some());
    }

    // ---- ToolResult tests ---------------------------------------------------

    #[test]
    fn test_tool_result_success() {
        let r = ToolResult::success("done");
        assert!(!r.is_error);
        assert_eq!(r.content, "done");
        assert!(r.metadata.is_none());
    }

    #[test]
    fn test_tool_result_error() {
        let r = ToolResult::error("something went wrong");
        assert!(r.is_error);
        assert_eq!(r.content, "something went wrong");
    }

    #[test]
    fn test_tool_result_with_metadata() {
        let r = ToolResult::success("ok")
            .with_metadata(serde_json::json!({"file": "foo.rs", "lines": 10}));
        assert!(r.metadata.is_some());
        let meta = r.metadata.unwrap();
        assert_eq!(meta["file"], "foo.rs");
    }

    // ---- ToolContext::resolve_path tests ------------------------------------

    #[test]
    fn test_resolve_path_absolute() {
        use mikmik_core::permissions::AutoPermissionHandler;

        let handler = Arc::new(AutoPermissionHandler {
            mode: mikmik_core::config::PermissionMode::Default,
        });
        let ctx = test_tool_context(handler);

        // Absolute paths pass through unchanged
        let resolved = ctx.resolve_path("/absolute/path/file.rs");
        assert_eq!(resolved, Ok(PathBuf::from("/absolute/path/file.rs")));
    }

    #[test]
    fn test_resolve_path_relative() {
        use mikmik_core::permissions::AutoPermissionHandler;

        let handler = Arc::new(AutoPermissionHandler {
            mode: mikmik_core::config::PermissionMode::Default,
        });
        let ctx = test_tool_context(handler);

        // Relative paths get joined with working_dir
        let resolved = ctx.resolve_path("src/main.rs");
        assert_eq!(resolved, Ok(PathBuf::from("/workspace/src/main.rs")));
    }

    fn context_with_extra_dir() -> ToolContext {
        use mikmik_core::permissions::AutoPermissionHandler;

        let handler = Arc::new(AutoPermissionHandler {
            mode: mikmik_core::config::PermissionMode::Default,
        });
        let mut ctx = test_tool_context(handler);
        ctx.config.additional_dirs = vec![PathBuf::from("/elsewhere/docs")];
        ctx
    }

    #[test]
    fn an_extra_directory_becomes_a_named_root() {
        let ctx = context_with_extra_dir();
        let roots = ctx.workspace_roots();

        assert_eq!(roots.get("main"), Some(&PathBuf::from("/workspace")));
        assert_eq!(roots.get("docs"), Some(&PathBuf::from("/elsewhere/docs")));
    }

    #[test]
    fn a_root_path_resolves_against_its_root() {
        let ctx = context_with_extra_dir();

        assert_eq!(
            ctx.resolve_path("&docs/spec.md"),
            Ok(PathBuf::from("/elsewhere/docs/spec.md"))
        );
        assert_eq!(
            ctx.resolve_path("&docs"),
            Ok(PathBuf::from("/elsewhere/docs"))
        );
        assert_eq!(
            ctx.resolve_path("&main/src/main.rs"),
            Ok(PathBuf::from("/workspace/src/main.rs"))
        );
    }

    #[test]
    fn a_mistyped_root_is_rejected_rather_than_joined() {
        let ctx = context_with_extra_dir();

        let error = ctx
            .resolve_path("&doc/spec.md")
            .expect_err("an unknown root must not resolve");
        assert!(error.contains("&doc"), "{error}");
        assert!(error.contains("&main"), "{error}");
        assert!(error.contains("&docs"), "{error}");
    }

    #[test]
    fn without_extra_directories_only_main_exists() {
        use mikmik_core::permissions::AutoPermissionHandler;

        let handler = Arc::new(AutoPermissionHandler {
            mode: mikmik_core::config::PermissionMode::Default,
        });
        let ctx = test_tool_context(handler);

        assert_eq!(ctx.workspace_roots().len(), 1);
        assert_eq!(
            ctx.resolve_path("src/main.rs"),
            Ok(PathBuf::from("/workspace/src/main.rs"))
        );
    }

    #[test]
    fn test_request_permission_uses_details_for_non_interactive_errors() {
        let ctx = test_tool_context(Arc::new(AskPermissionHandler {
            reason: "generic reason".to_string(),
        }));
        let request = ctx.build_permission_request(
            "PowerShell",
            "[High risk] set execution policy",
            Some("[High risk] This may modify system-wide security policy.".to_string()),
            false,
            Some(PathBuf::from("Set-ExecutionPolicy RemoteSigned")),
        );

        let error = ctx
            .request_permission_inner(request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("[High risk] This may modify system-wide security policy."));
        assert!(!error.contains("generic reason"));
    }

    #[test]
    fn test_request_permission_falls_back_to_handler_reason_without_details() {
        let ctx = test_tool_context(Arc::new(AskPermissionHandler {
            reason: "generic reason".to_string(),
        }));
        let request = ctx.build_permission_request(
            "Bash",
            "run ls",
            None,
            false,
            Some(PathBuf::from("ls -la")),
        );

        let error = ctx
            .request_permission_inner(request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("generic reason"));
    }

    // ---- PermissionLevel tests ---------------------------------------------

    #[test]
    fn test_permission_level_order() {
        // Just verify the variants exist and are distinct
        assert_ne!(PermissionLevel::None, PermissionLevel::ReadOnly);
        assert_ne!(PermissionLevel::Write, PermissionLevel::Execute);
        assert_ne!(PermissionLevel::Execute, PermissionLevel::Dangerous);
    }

    #[test]
    fn test_bash_tool_permission_level() {
        assert_eq!(PtyBashTool.permission_level(), PermissionLevel::Execute);
    }

    #[test]
    fn test_file_read_permission_level() {
        assert_eq!(FileReadTool.permission_level(), PermissionLevel::ReadOnly);
    }

    #[test]
    fn test_file_edit_permission_level() {
        assert_eq!(FileEditTool.permission_level(), PermissionLevel::Write);
    }

    #[test]
    fn test_file_write_permission_level() {
        assert_eq!(FileWriteTool.permission_level(), PermissionLevel::Write);
    }

    // ---- access_tool_names --------------------------------------------------

    #[test]
    fn full_access_is_unrestricted() {
        assert!(access_tool_names("full").is_none());
        assert!(access_tool_names("anything-else").is_none());
    }

    #[test]
    fn read_only_access_excludes_write_and_execute_tools() {
        let names = access_tool_names("read-only").expect("read-only allowlist");
        assert!(names.iter().any(|n| n == "Read"));
        assert!(names.iter().any(|n| n == "AskUserQuestion"));
        // A write tool and an execute tool are both filtered out.
        assert!(!names.iter().any(|n| n == "Write"));
        assert!(!names.iter().any(|n| n == "Bash"));
    }

    #[test]
    fn search_only_access_is_the_fixed_search_set() {
        let names = access_tool_names("search-only").expect("search-only allowlist");
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["Glob", "Grep", "Read", "WebFetch", "WebSearch"]
        );
    }

    // ---- Tool to_definition tests ------------------------------------------

    #[test]
    fn test_tool_to_definition() {
        let def = PtyBashTool.to_definition();
        assert_eq!(def.name, "Bash");
        assert!(!def.description.is_empty());
        assert!(def.input_schema.is_object());
    }

    // ---- write_atomic tests -------------------------------------------------
    //
    // `write_atomic` is the single atomic-write path that ApplyPatch, BatchEdit,
    // NotebookEdit and the cron store (#226) all route through. These tests pin
    // its contract: it writes the exact bytes and never leaves a temp file
    // behind on success — the guarantee that makes those tools crash-safe.

    /// Count the `.mikmik-tmp-*` scratch files left in `dir`.
    fn count_atomic_tmp_files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".mikmik-tmp-"))
            .count()
    }

    #[tokio::test]
    async fn write_atomic_writes_content_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");

        // Fresh file.
        write_atomic(&path, b"hello\nworld\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
        assert_eq!(count_atomic_tmp_files(dir.path()), 0, "no tmp after create");

        // Overwrite an existing file (the crash-truncation scenario #226 fixes).
        write_atomic(&path, b"replaced").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replaced");
        assert_eq!(
            count_atomic_tmp_files(dir.path()),
            0,
            "no tmp after overwrite"
        );
    }

    /// The executable bit (and other permissions) must survive an atomic
    /// overwrite, since we rename a fresh temp file over the destination.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_atomic_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");

        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_atomic(&path, b"#!/bin/sh\necho hi\n").await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "executable bit preserved");
        assert_eq!(count_atomic_tmp_files(dir.path()), 0);
    }

    // ---- live output sink -------------------------------------------------

    fn ctx_with_output_channel(
        live: bool,
        with_call: bool,
    ) -> (
        ToolContext,
        tokio::sync::mpsc::UnboundedReceiver<ToolOutputChunk>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = crate::test_support::allow_all_context(std::env::temp_dir());
        ctx.config.live_tool_output = live;
        ctx.tool_output_tx = Some(tx);
        if with_call {
            ctx.current_call = Some(Arc::new(ActiveToolCall {
                id: "call-1".to_string(),
                input: serde_json::json!({}),
                permission_wait_ms: Arc::default(),
            }));
        }
        (ctx, rx)
    }

    #[test]
    fn the_sink_stamps_each_chunk_with_the_call_it_came_from() {
        let (ctx, mut rx) = ctx_with_output_channel(true, true);
        let sink = ctx.live_output_sink().expect("sink");

        sink("first");
        sink("second");

        let a = rx.try_recv().expect("first chunk");
        assert_eq!(a.tool_id, "call-1");
        assert_eq!(a.text, "first");
        assert_eq!(rx.try_recv().expect("second chunk").text, "second");
    }

    #[test]
    fn the_sink_is_absent_unless_every_condition_holds() {
        // Producing chunks nobody draws would cost a clone per read for the
        // whole run of every command.
        assert!(
            ctx_with_output_channel(false, true)
                .0
                .live_output_sink()
                .is_none(),
            "the setting is off"
        );
        assert!(
            ctx_with_output_channel(true, false)
                .0
                .live_output_sink()
                .is_none(),
            "the dispatcher has not said which call this is"
        );

        let mut no_channel = crate::test_support::allow_all_context(std::env::temp_dir());
        no_channel.config.live_tool_output = true;
        no_channel.current_call = Some(Arc::new(ActiveToolCall {
            id: "call-1".to_string(),
            input: serde_json::json!({}),
            permission_wait_ms: Arc::default(),
        }));
        assert!(
            no_channel.live_output_sink().is_none(),
            "no frontend asked for live output"
        );
    }

    #[test]
    fn each_setting_reaches_the_shell_as_itself() {
        // The two enums are separate because `mikmik-shell` does not depend on
        // `mikmik-core`. A swapped arm here would silently change which `ls`
        // every session runs, and nothing else would notice.
        assert_eq!(
            bundled_policy(mikmik_core::config::BundledUtilities::Prefer),
            mikmik_shell::BundledUtilities::Prefer
        );
        assert_eq!(
            bundled_policy(mikmik_core::config::BundledUtilities::Fallback),
            mikmik_shell::BundledUtilities::Fallback
        );
    }
}
