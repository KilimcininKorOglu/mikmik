// cc-query: The core agentic query loop.
//
// This crate implements the main conversation loop that:
// 1. Sends messages to the Anthropic API
// 2. Processes streaming responses
// 3. Detects tool-use requests and dispatches them
// 4. Feeds tool results back to the model
// 5. Handles auto-compact when the context window fills up
// 6. Manages stop conditions (end_turn, max_turns, cancellation)

// too_many_arguments: `run_query_loop` and related orchestration entrypoints
// thread many parameters by design; splitting would obscure the control flow.
#![allow(clippy::too_many_arguments)]

pub mod advisor_runtime;
pub mod agent_tool;
pub mod auto_dream;
pub mod command_queue;
pub mod compact;
pub mod context_analyzer;
pub mod continuation;
pub mod cron_scheduler;
pub mod goal_loop;
pub mod managed_orchestrator;
pub mod roster;
pub mod sanitize;
pub mod session_memory;
pub mod skill_prefetch;

mod runner;
pub use agent_tool::{init_team_swarm_runner, AgentTool};
pub use command_queue::{drain_command_queue, CommandPriority, CommandQueue, QueuedCommand};
pub use compact::{
    attempt_compaction, calculate_messages_to_keep_index, calculate_token_warning_state,
    calculate_token_warning_state_for_window, compact_conversation, compact_with_fallback,
    context_collapse, context_window_for_model, estimate_context_tokens, format_compact_summary,
    get_compact_prompt, reactive_compact, resolve_context_window, should_auto_compact_for_window,
    should_compact, should_context_collapse, AutoCompactState, CompactResult, CompactTrigger,
    TokenWarningState,
};
pub use continuation::{
    ContinuationDecision, ContinuationMode, ContinuationPolicy, StopPolicy, TurnEndContext,
};
pub use cron_scheduler::start_cron_scheduler;
pub use goal_loop::{
    check_and_continue_goal, decide_goal_continuation, mark_goal_complete, GoalContinuation,
    StopReason,
};
pub use roster::build_tool_roster;
pub use runner::*;
pub use sanitize::sanitize_history;
pub use session_memory::{
    ExtractedMemory, MemoryCategory, SessionMemoryExtractor, SessionMemoryState,
};
pub use skill_prefetch::{
    format_skill_listing, prefetch_skills, SharedSkillIndex, SkillDefinition, SkillIndex,
};

use mikmik_api::{
    AnthropicStreamEvent, ApiMessage, ApiToolDefinition, CreateMessageRequest, StreamAccumulator,
    StreamHandler, SystemPrompt, ThinkingConfig,
};
use mikmik_core::config::Config;
use mikmik_core::cost::CostTracker;
use mikmik_core::error::ClaudeError;
use mikmik_core::types::{ContentBlock, Message, Role, ToolResultContent, UsageInfo};
use mikmik_tools::{PermissionLevel, Tool, ToolContext, ToolResult};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Outcome of a single query-loop run.
#[derive(Debug)]
pub enum QueryOutcome {
    /// The model finished its turn (end_turn stop reason).
    EndTurn { message: Message, usage: UsageInfo },
    /// The model hit max_tokens.
    MaxTokens {
        partial_message: Message,
        usage: UsageInfo,
    },
    /// The conversation was cancelled by the user.
    Cancelled,
    /// An unrecoverable error occurred.
    Error(ClaudeError),
    /// The configured USD budget was exceeded.
    BudgetExceeded { cost_usd: f64, limit_usd: f64 },
}

/// Configuration for a single query-loop invocation.
#[derive(Clone)]
pub struct QueryConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: u32,
    /// Whether exceeding `max_turns` runs one final tool-less summary turn.
    /// When false the loop returns the last assistant message at the limit.
    pub degradation_summary: bool,
    /// Whether the incomplete-todo reminder is appended to the system prompt
    /// after the second turn.
    pub auto_poke: bool,
    /// Whether the context is compacted automatically when it fills up.
    pub auto_compact: bool,
    /// Context fill, as a percentage 0-100, at which auto-compact fires.
    pub compact_threshold: u8,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub output_style: mikmik_core::system_prompt::OutputStyle,
    pub output_style_prompt: Option<String>,
    pub working_directory: Option<String>,
    /// Every directory the session can reach, by name. Forwarded to the
    /// system prompt so the model can address them as `&name/path`.
    pub workspace_roots: std::collections::BTreeMap<String, String>,
    pub thinking_budget: Option<u32>,
    pub temperature: Option<f32>,
    /// Maximum cumulative character count of all tool results in the message
    /// history before older results are replaced with a truncation notice.
    /// Mirrors the TS `applyToolResultBudget` mechanism.  Default: 50_000.
    pub tool_result_budget: usize,
    /// Optional effort level.  When set and `thinking_budget` is `None`,
    /// the effort level's `thinking_budget_tokens()` is used as the
    /// thinking budget.  Also provides a temperature override when the
    /// level specifies one.
    pub effort_level: Option<mikmik_core::effort::EffortLevel>,
    /// T1-4: Optional shared command queue.
    ///
    /// When set, the query loop drains this queue before each API call and
    /// injects any resulting messages into the conversation.  The queue is
    /// shared (Arc-backed) so the TUI input thread can push commands while the
    /// loop is waiting for a model response.
    pub command_queue: Option<CommandQueue>,
    /// T1-5: Optional shared skill index.
    ///
    /// When set, `prefetch_skills` is spawned once before the loop begins and
    /// the resulting index is used to inject a skill listing attachment into
    /// the conversation context.
    pub skill_index: Option<SharedSkillIndex>,
    /// Optional USD spend cap. The query loop checks accumulated cost after
    /// each turn and aborts with `QueryOutcome::BudgetExceeded` when exceeded.
    pub max_budget_usd: Option<f64>,
    /// Fallback model name. Used when the primary model returns overloaded /
    /// rate-limit errors (mirrors TS `--fallback-model`).
    pub fallback_model: Option<String>,
    /// Optional ProviderRegistry for dispatching to non-Anthropic providers.
    /// When `config.provider` is set to something other than "anthropic" and
    /// this registry contains that provider, the registry's provider is used
    /// instead of `AnthropicClient`.
    pub provider_registry: Option<std::sync::Arc<mikmik_api::ProviderRegistry>>,
    /// Active agent name (e.g., "build", "plan", "explore", or None for default).
    pub agent_name: Option<String>,
    /// Resolved agent definition for the current session.
    pub agent_definition: Option<mikmik_core::AgentDefinition>,
    /// Optional shared model registry for dynamic provider and model resolution.
    /// When set, the query loop uses this instead of constructing a fresh registry.
    pub model_registry: Option<std::sync::Arc<mikmik_api::ModelRegistry>>,
    /// Managed agent (manager-executor) configuration.
    pub managed_agents: Option<mikmik_core::ManagedAgentConfig>,
    /// Names of the tools enabled for this session (issue #233).
    ///
    /// When populated, `build_system_prompt` forwards these to
    /// `SystemPromptOptions::enabled_tools` so the "Tool use guidelines"
    /// section only emits per-tool guidance for tools that are actually
    /// loaded. `None`/empty means "unknown" and every block is emitted,
    /// which keeps existing behaviour for callers that don't set it.
    ///
    // Populated in-loop (issue #233 completion): when left `None`,
    // `run_query_loop` fills this from its live `tools: &[Box<dyn Tool>]`
    // argument before assembling the system prompt, so the top-level
    // interactive session gets progressive tool disclosure. Callers that build
    // both the tool vec and the config (e.g. sub-agents) may still set it
    // explicitly; the loop only fills an unset field.
    pub enabled_tools: Option<Vec<String>>,
    /// End-of-turn continuation policy (issue #230 / MI-3).
    ///
    /// `Default` stops after one turn (normal, non-goal behaviour). Goal-driven
    /// autonomy selects `Goal`, which keeps the loop running while an active
    /// goal's guards allow, injecting the goal continuation message as the next
    /// user turn — instead of the CLI REPL re-dispatching a fresh turn.
    pub continuation: crate::continuation::ContinuationMode,
    /// Text describing the companion sitting beside the input box, forwarded
    /// to `SystemPromptOptions::companion_addendum`.
    ///
    /// The model has to know the companion exists, or it narrates what the
    /// companion might say and the bubble says it too.
    ///
    /// Set only by the interactive REPL. Headless runs and sub-agents have no
    /// input box for the companion to sit beside, so describing it there would
    /// spend tokens on something the user cannot see.
    pub companion_addendum: Option<String>,
    /// Whether the project's memory directory is read into the system prompt.
    ///
    /// Resolved once here rather than re-read per turn, so every turn in a
    /// session builds the same prompt shape.
    pub auto_memory_enabled: bool,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            model: mikmik_core::constants::DEFAULT_MODEL.to_string(),
            max_tokens: mikmik_core::constants::DEFAULT_MAX_TOKENS,
            max_turns: mikmik_core::constants::MAX_TURNS_DEFAULT,
            degradation_summary: true,
            auto_poke: true,
            auto_compact: true,
            compact_threshold: mikmik_core::constants::DEFAULT_COMPACT_THRESHOLD,
            system_prompt: None,
            append_system_prompt: None,
            output_style: mikmik_core::system_prompt::OutputStyle::Default,
            output_style_prompt: None,
            working_directory: None,
            workspace_roots: std::collections::BTreeMap::new(),
            thinking_budget: None,
            temperature: None,
            tool_result_budget: 50_000,
            effort_level: None,
            command_queue: None,
            skill_index: None,
            max_budget_usd: None,
            fallback_model: None,
            provider_registry: None,
            agent_name: None,
            agent_definition: None,
            model_registry: None,
            managed_agents: None,
            enabled_tools: None,
            continuation: crate::continuation::ContinuationMode::Default,
            companion_addendum: None,
            auto_memory_enabled: false,
        }
    }
}

impl QueryConfig {
    pub fn from_config(cfg: &Config) -> Self {
        // Canonical, not `effective_model()`. The turn loop resolves this
        // string back into a route on every request, and several of the
        // per-provider fallbacks are slashed ids of their own
        // (`"anthropic/claude-sonnet-4"` for OpenRouter), which the resolver
        // would read as an account prefix and send elsewhere.
        let route = cfg.effective_route();
        Self {
            model: cfg.canonical_model(&route.account, &route.model),
            max_tokens: cfg.effective_max_tokens(),
            output_style: cfg.effective_output_style(),
            output_style_prompt: cfg.resolve_output_style_prompt(),
            working_directory: cfg.project_dir.as_ref().map(|p| p.display().to_string()),
            managed_agents: cfg.managed_agents.clone(),
            // One pool for the manager and every executor: a sub-agent runs on
            // the parent's `CostTracker`, so the loop's own cap already counts
            // what they spend together. A `--max-budget-usd` flag is applied
            // after this and still wins.
            max_budget_usd: cfg
                .managed_agents
                .as_ref()
                .filter(|ma| ma.enabled)
                .and_then(|ma| ma.total_budget_usd),
            effort_level: cfg.effective_effort_level(),
            max_turns: cfg
                .max_turns
                .unwrap_or(mikmik_core::constants::MAX_TURNS_DEFAULT),
            degradation_summary: cfg.degradation_summary.unwrap_or(true),
            auto_poke: cfg.auto_poke.unwrap_or(true),
            auto_compact: cfg.effective_auto_compact(),
            compact_threshold: cfg.effective_compact_threshold(),
            auto_memory_enabled: mikmik_core::memdir::is_auto_memory_enabled(
                cfg.auto_memory_enabled,
            ),
            ..Default::default()
        }
    }

    /// Build a QueryConfig using dynamic model resolution from the model registry.
    ///
    /// Prefers the best model for the configured provider (from models.dev data)
    /// over the hardcoded defaults.
    pub fn from_config_with_registry(cfg: &Config, registry: &mikmik_api::ModelRegistry) -> Self {
        // We can't move the Arc here, but we need a clone for the query loop.
        // Callers typically wrap the registry in an Arc already.
        let route = mikmik_api::resolve_effective_route(cfg, registry);
        Self {
            model: cfg.canonical_model(&route.account, &route.model),
            max_tokens: cfg.effective_max_tokens(),
            output_style: cfg.effective_output_style(),
            output_style_prompt: cfg.resolve_output_style_prompt(),
            working_directory: cfg.project_dir.as_ref().map(|p| p.display().to_string()),
            managed_agents: cfg.managed_agents.clone(),
            // One pool for the manager and every executor: a sub-agent runs on
            // the parent's `CostTracker`, so the loop's own cap already counts
            // what they spend together. A `--max-budget-usd` flag is applied
            // after this and still wins.
            max_budget_usd: cfg
                .managed_agents
                .as_ref()
                .filter(|ma| ma.enabled)
                .and_then(|ma| ma.total_budget_usd),
            effort_level: cfg.effective_effort_level(),
            max_turns: cfg
                .max_turns
                .unwrap_or(mikmik_core::constants::MAX_TURNS_DEFAULT),
            degradation_summary: cfg.degradation_summary.unwrap_or(true),
            auto_poke: cfg.auto_poke.unwrap_or(true),
            auto_compact: cfg.effective_auto_compact(),
            compact_threshold: cfg.effective_compact_threshold(),
            auto_memory_enabled: mikmik_core::memdir::is_auto_memory_enabled(
                cfg.auto_memory_enabled,
            ),
            ..Default::default()
        }
    }
}

/// Events emitted by the query loop for the TUI to render.
#[derive(Debug, Clone)]
pub enum QueryEvent {
    /// A stream event from the API.
    Stream(AnthropicStreamEvent),
    /// A tool is about to be executed.
    ToolStart {
        tool_name: String,
        tool_id: String,
        input_json: String,
    },
    /// A tool has finished executing.
    ToolEnd {
        tool_name: String,
        tool_id: String,
        result: String,
        is_error: bool,
        /// How long the tool's own work took, in milliseconds. `None` when
        /// nothing ran, which is what a cancelled call answers.
        duration_ms: Option<u64>,
    },
    /// The model finished a turn.
    TurnComplete {
        turn: u32,
        stop_reason: String,
        usage: Option<UsageInfo>,
        /// The model that ran this turn. It is not always the session model:
        /// an agent definition can override it and a fallback switch can
        /// replace it mid-turn, and the usage above must be priced at its
        /// rates.
        model: String,
    },
    /// An informational status message.
    Status(String),
    /// The conversation was replaced by a summary of its older part.
    ///
    /// Carries the new size because nothing else can report it: the next
    /// provider usage figure arrives a turn later, so without this the footer
    /// would go on showing the pre-compaction figure until then.
    Compacted {
        /// Message count before the summary replaced the head.
        messages_before: usize,
        /// Message count after.
        messages_after: usize,
        /// Estimated size of the conversation now, in tokens.
        tokens_after: u64,
    },
    /// A watching advisor said something about the work.
    ///
    /// The note is already in the conversation by the time this is sent; this
    /// is so the front end can draw it as the advisor's own remark rather than
    /// as another user message.
    Advisory {
        /// The roster entry that raised it, when the session runs more than the
        /// single default watcher.
        advisor: Option<String>,
        /// `nit`, `concern` or `blocker`.
        severity: String,
        note: String,
    },
    /// An error.
    Error(String),
    /// Token usage has crossed a warning threshold.
    /// `state` is Warning (≥ 80 %) or Critical (≥ 95 %).
    /// `pct_used` is the fraction of the context window consumed (0.0–1.0).
    TokenWarning {
        state: TokenWarningState,
        pct_used: f64,
    },
}

// ---------------------------------------------------------------------------
// Tool-result budgeting
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Query loop
// ---------------------------------------------------------------------------

/// Maximum number of max_tokens continuation attempts before surfacing the
/// partial response.  Mirrors `MAX_OUTPUT_TOKENS_RECOVERY_LIMIT` in query.ts.
const MAX_TOKENS_RECOVERY_LIMIT: u32 = 3;

/// Message injected when the model hits its output-token limit.
/// Mirrors the TS recovery message in query.ts lines 1224-1228.
const MAX_TOKENS_RECOVERY_MSG: &str =
    "Output token limit hit. Resume directly — no apology, no recap of what \
     you were doing. Pick up mid-thought if that is where the cut happened. \
     Break remaining work into smaller pieces.";

/// Injected as the final user turn when `effective_max_turns` is reached. That
/// turn runs with tools DISABLED (graceful degradation, mirroring opencode's
/// max-steps `toolChoice:"none"` behaviour), so the model produces a plain-text
/// wrap-up instead of the loop returning cold.
const MAX_STEPS_DEGRADATION_MSG: &str =
    "You have reached the maximum number of steps for this run, so tools are now \
     disabled — do not attempt to call any tools. In plain text, briefly \
     summarize what you accomplished, what remains unfinished, and exactly where \
     you stopped, so the work can be resumed later.";

/// Content stored in the synthetic `tool_result` for a tool that was abandoned
/// mid-flight because the query loop was cancelled (issue #218). Every
/// outstanding `tool_use` still receives a matching `tool_result` carrying this
/// text so the message history stays well-formed.
const TOOL_CANCELLED_MSG: &str = "Tool execution was cancelled by the user before it completed.";

fn merge_provider_stream_usage(current: &mut UsageInfo, update: &UsageInfo) {
    if update.total_input() > 0 {
        current.input_tokens = update.input_tokens;
        current.cache_read_input_tokens = update.cache_read_input_tokens;
        current.cache_creation_input_tokens = update.cache_creation_input_tokens;
    }
    if update.output_tokens > 0 {
        current.output_tokens = update.output_tokens;
    }
}

/// What this turn used, priced at the model that ran it.
///
/// Stored on the assistant message because that is where `/stats` reads it
/// from: a session whose messages carry no cost reports zero tokens and zero
/// dollars however much it actually spent. Priced from `effective_model`
/// rather than `config.model`, for the same reason `cost_tracker` is.
fn cost_of_turn(
    model: &str,
    pricing: mikmik_core::cost::ModelPricing,
    usage: &UsageInfo,
) -> mikmik_core::types::MessageCost {
    mikmik_core::types::MessageCost {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cost_usd: pricing.cost_of(
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        ),
        model: Some(model.to_string()),
    }
}

// Spinner verbs are imported from mikmik_core::spinner

/// Resolve the effective effort level for a turn.
///
/// Ultracode is a keyword-activated effort: if the most recent user message
/// contains the `ultracode` keyword (whole-word, case-insensitive), the effort
/// for this turn is raised to [`EffortLevel::Ultracode`] — the model's top
/// reasoning plus the ultracode operating procedure (injected as a system
/// addendum by the loop). Otherwise the configured `config_effort` is used
/// unchanged. Checking only the *last* user message keeps the mode scoped to the
/// turn that asked for it (a later plain turn deactivates it automatically).
///
/// [`EffortLevel::Ultracode`]: mikmik_core::effort::EffortLevel::Ultracode
fn effective_effort_for_turn(
    config_effort: Option<mikmik_core::effort::EffortLevel>,
    messages: &[Message],
) -> Option<mikmik_core::effort::EffortLevel> {
    if let Some(last_user) = messages.iter().rev().find(|m| m.role == Role::User) {
        if mikmik_core::effort::text_triggers_ultracode(&last_user.get_all_text()) {
            return Some(mikmik_core::effort::EffortLevel::Ultracode);
        }
    }
    config_effort
}

/// Resolve the effective output-style persona for a turn.
///
/// Personas (`rocky` / `caveman` / `normal`) mirror the ultracode keyword: an
/// **inline** persona word in the most recent user message applies to *that one
/// turn* (transient) and then reverts, while the persona chosen via `/rocky`,
/// `/caveman`, or `/output-style` lives in `config` and **persists** until
/// changed. Inline `normal` resets to the default (no persona) for the turn.
///
/// Returns the `(output_style, output_style_prompt)` pair to assemble the
/// system prompt with for this turn. When no inline persona keyword is present,
/// the configured (persistent) pair is returned unchanged. Checking only the
/// *last* user message keeps the mode scoped to the turn that asked for it.
fn effective_output_style_for_turn(
    config: &QueryConfig,
    messages: &[Message],
) -> (mikmik_core::system_prompt::OutputStyle, Option<String>) {
    if let Some(last_user) = messages.iter().rev().find(|m| m.role == Role::User) {
        if let Some(style_name) =
            mikmik_core::keywords::inline_persona_style(&last_user.get_all_text())
        {
            // Inline `normal` (→ "default") resets the persona for this turn.
            if style_name == "default" {
                return (mikmik_core::system_prompt::OutputStyle::Default, None);
            }
            // Otherwise apply the named persona's prompt for this turn only.
            let prompt = mikmik_core::output_styles::find_style(
                &mikmik_core::output_styles::builtin_styles(),
                style_name,
            )
            .map(|style| style.prompt.clone())
            .filter(|prompt| !prompt.trim().is_empty());
            return (mikmik_core::system_prompt::OutputStyle::Default, prompt);
        }
    }
    // No inline persona keyword — keep the persistent selection.
    (config.output_style, config.output_style_prompt.clone())
}

/// Run the agentic query loop.
///
/// This sends the conversation to the API, handles tool calls in a loop, and
/// returns when the model issues an end_turn or an error/limit is hit.
///
/// `pending_messages` is an optional queue of user messages that were enqueued
/// during tool execution (e.g. by the UI or a command queue).  Each string is
/// appended as a plain user message between turns.  Callers that do not need
/// command queuing may pass `None` or an empty `Vec`.
///
/// Fires the plugin `Stop` hook when the loop ends, or `StopFailure` when it
/// ends on an error, which is why the body lives in `run_query_loop_inner`:
/// the loop returns from too many places to hook each one.
#[allow(clippy::too_many_arguments)]
pub async fn run_query_loop(
    client: &mikmik_api::AnthropicClient,
    messages: &mut Vec<Message>,
    tools: &[Box<dyn Tool>],
    tool_ctx: &ToolContext,
    config: &QueryConfig,
    cost_tracker: Arc<CostTracker>,
    event_tx: Option<mpsc::UnboundedSender<QueryEvent>>,
    cancel_token: tokio_util::sync::CancellationToken,
    pending_messages: Option<&mut Vec<String>>,
) -> QueryOutcome {
    let outcome = run_query_loop_inner(
        client,
        messages,
        tools,
        tool_ctx,
        config,
        cost_tracker,
        event_tx,
        cancel_token,
        pending_messages,
    )
    .await;

    let event = if matches!(outcome, QueryOutcome::Error(_)) {
        mikmik_plugins::HookEventKind::StopFailure
    } else {
        mikmik_plugins::HookEventKind::Stop
    };
    mikmik_plugins::run_global_hook(
        event,
        None,
        serde_json::json!({
            "outcome": agent_tool::outcome_label(&outcome),
            "session_id": tool_ctx.session_id,
        }),
    )
    .await;

    outcome
}

/// How much inbox text one turn may carry, across every message in it.
const MAX_INBOX_RENDER_CHARS: usize = 16_000;

/// Give a context an address unless it arrived with one.
///
/// A sub-agent is addressed by `AgentTool` before its loop starts; anything
/// else is a top-level session, which answers to its bare session id under the
/// reserved name `main`. Returning the guard hands the caller the lifetime:
/// the address lasts exactly as long as the loop does.
fn bind_address(ctx: &mut ToolContext) -> Option<mikmik_tools::InboxGuard> {
    if !ctx.inbox.own.is_empty() {
        return None;
    }
    ctx.inbox.own = ctx.session_id.clone();
    ctx.inbox.name = Some(mikmik_tools::MAIN_NAME.to_string());
    Some(mikmik_tools::register_main(&ctx.session_id))
}

/// Move anything another agent sent into this turn's conversation.
fn deliver_inbox(address: &str, messages: &mut Vec<Message>) {
    let inbox = mikmik_tools::drain_inbox(address);
    if inbox.is_empty() {
        return;
    }
    debug!(count = inbox.len(), "Delivering agent messages");
    messages.push(Message::user(render_inbox(&inbox)));
}

/// Turn collected messages into the one user turn that delivers them.
///
/// Each is framed as a system notice naming its sender, because the text was
/// written by another agent and must not read as something the user typed.
fn render_inbox(messages: &[mikmik_tools::AgentMessage]) -> String {
    let mut out = String::new();
    let mut dropped = 0usize;

    for message in messages {
        let block = format!(
            "[System]: Message from '{}':\n{}\n\n",
            message.from, message.content
        );
        if out.len() + block.len() > MAX_INBOX_RENDER_CHARS {
            dropped += 1;
            continue;
        }
        out.push_str(&block);
    }

    if dropped > 0 {
        out.push_str(&format!(
            "[System]: {} further message(s) were dropped; this turn had no room for them.\n",
            dropped
        ));
    }

    out
}

async fn run_query_loop_inner(
    client: &mikmik_api::AnthropicClient,
    messages: &mut Vec<Message>,
    tools: &[Box<dyn Tool>],
    tool_ctx: &ToolContext,
    config: &QueryConfig,
    cost_tracker: Arc<CostTracker>,
    event_tx: Option<mpsc::UnboundedSender<QueryEvent>>,
    cancel_token: tokio_util::sync::CancellationToken,
    mut pending_messages: Option<&mut Vec<String>>,
) -> QueryOutcome {
    // Rebind the tool context to carry the loop's actual cancel token so the
    // parallel tool executor — and any tools or sub-agents that read
    // `ctx.cancel_token` — observe the same cancellation signal that drives this
    // loop (issue #218). Callers construct the context with a placeholder token;
    // making the loop authoritative here means a parent cancel reaches tools.
    let mut loop_ctx = tool_ctx.clone();
    loop_ctx.cancel_token = cancel_token.clone();
    // Binding here rather than at each frontend means the TUI, ACP and
    // headless paths all become addressable at once.
    let _inbox_guard = bind_address(&mut loop_ctx);
    let tool_ctx = &loop_ctx;

    let mut turn = 0u32;
    // Tracks how many consecutive max_tokens recoveries we've attempted so
    // we don't loop forever on a model that can't finish within any budget.
    let mut max_tokens_recovery_count: u32 = 0;
    // Active model — may switch to fallback on overloaded errors.
    // Agent model override takes priority over the session model when set.
    let mut effective_model = if let Some(ref agent) = config.agent_definition {
        agent.model.clone().unwrap_or_else(|| config.model.clone())
    } else {
        config.model.clone()
    };

    // If managed-agent mode is active, override the model to the manager model.
    if let Some(ref ma_config) = config.managed_agents {
        if ma_config.enabled && !ma_config.manager_model.is_empty() {
            effective_model = ma_config.manager_model.clone();
        }
    }

    let mut used_fallback = false;
    // Watches what the model writes so a `scope: text` or `scope: thinking`
    // rule can stop the turn at the point of the violation instead of after it.
    // Built once for the whole query, because the interrupt budget it carries
    // is a query-wide one: an interrupt does not count against `max_turns`, so
    // a per-turn budget would let a `repeat: always` rule loop forever.
    let mut prose_watch = runner::hooks::ProseWatch::new(tool_ctx);
    // The watching advisor, when the mode runs one. It reviews each turn on its
    // own model in a background task; the loop hands it deltas and drains its
    // notes. `None` for every session that did not ask for one, which is every
    // session by default.
    let mut advisor = crate::advisor_runtime::AdvisorSession::start(
        tool_ctx,
        config,
        cost_tracker.clone(),
        cancel_token.clone(),
    );
    // How many automatic retries remain when a stream stalls (no data for 45s).
    let mut retries_left: u32 = 2;
    // Max-steps graceful degradation (issue #230 / MI-3): set once the final
    // tool-less summary turn has been dispatched so it can never re-trigger
    // (anti-recursion guard).
    let mut degradation_done = false;

    // If an agent defines a max_turns override, respect it (agent wins over config).
    let effective_max_turns = config
        .agent_definition
        .as_ref()
        .and_then(|a| a.max_turns)
        .unwrap_or(config.max_turns);

    // In-loop continuation policy (issue #230 / MI-3). Consulted at the end of
    // every turn that finishes with `end_turn`. The default policy stops after
    // one turn; the goal policy keeps the loop running while an active goal's
    // guards allow. Built once per run.
    let continuation_policy = config.continuation.policy();
    // Wall-clock start of the current "continuation turn" (a span from a user /
    // continuation message to the next `end_turn`). Reset on each accepted
    // continuation so goal time/turn accounting matches the old per-dispatch
    // measurement.
    let mut goal_turn_start = std::time::Instant::now();

    // Shadow-git snapshot: capture the worktree state before any tools run so we
    // can produce a per-turn file-change patch when the turn ends.
    let shadow_snap: Option<std::sync::Arc<mikmik_core::snapshot::ShadowSnapshot>> =
        if tool_ctx.config.auto_commits == Some(true) {
            mikmik_core::snapshot::get_or_create(&tool_ctx.working_dir)
        } else {
            None
        };
    // Pre-capture tree hash; refreshed at the start of each turn's tool phase.
    let initial_snapshot: Option<String> = if let Some(ref s) = shadow_snap {
        s.track().await
    } else {
        None
    };

    loop {
        turn += 1;
        tool_ctx
            .current_turn
            .store(turn as usize, std::sync::atomic::Ordering::Relaxed);

        // Anything the watcher said while the last turn ran, or held back
        // because a recent interruption had not cooled down. A note that waited
        // for a boundary reaches the model here, before the request is built.
        if let Some(session) = advisor.as_mut() {
            let notes = session.take_pending();
            if !notes.is_empty() {
                for note in &notes {
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::Advisory {
                            advisor: note.advisor.clone(),
                            severity: note.severity.as_str().to_string(),
                            note: note.note.clone(),
                        });
                    }
                }
                messages.push(crate::advisor_runtime::AdvisorSession::message_for(&notes));
            }
        }
        // Max-steps graceful degradation (issue #230 / MI-3). Rather than
        // returning cold when the turn cap is hit, run ONE final turn with tools
        // disabled that asks the model to summarize progress and its stopping
        // point (mirrors opencode's max-steps `toolChoice:"none"` fallback).
        // `degradation_done` is the anti-recursion guard: the summary turn is
        // dispatched exactly once, and re-exceeding the cap afterwards returns
        // cold. Applies to both goal and non-goal runs.
        let degradation_turn = if turn > effective_max_turns {
            // The summary turn costs one more request. A caller that only wants
            // the work stopped at the limit turns it off and takes what the
            // model last said.
            if !config.degradation_summary {
                info!(
                    turns = turn,
                    max = effective_max_turns,
                    "Max turns reached — summary turn disabled, returning the last message"
                );
                let last_msg = messages
                    .last()
                    .cloned()
                    .unwrap_or_else(|| Message::assistant("Max turns reached."));
                return QueryOutcome::EndTurn {
                    message: last_msg,
                    usage: UsageInfo::default(),
                };
            }
            if degradation_done {
                info!(
                    turns = turn,
                    "Max turns reached after degradation summary — returning"
                );
                let last_msg = messages
                    .last()
                    .cloned()
                    .unwrap_or_else(|| Message::assistant("Max turns reached."));
                return QueryOutcome::EndTurn {
                    message: last_msg,
                    usage: UsageInfo::default(),
                };
            }
            degradation_done = true;
            info!(
                turns = turn,
                max = effective_max_turns,
                "Max turns reached — running final tool-less summary turn"
            );
            if let Some(ref tx) = event_tx {
                let _ = tx.send(QueryEvent::Status(format!(
                    "Reached maximum turn limit ({}) — summarizing progress before stopping.",
                    effective_max_turns
                )));
            }
            // Inject the summary request as the next user turn. Tools are
            // disabled for this turn where `api_tools` / `provider_tools` are
            // built below.
            messages.push(Message::user(MAX_STEPS_DEGRADATION_MSG));
            true
        } else {
            false
        };

        // Continuation decision at `end_turn` (issue #230 / MI-3). Consults the
        // active continuation policy: `Continue` injects the follow-up message
        // as the next user turn and keeps looping (resetting the per-turn budget
        // so `effective_max_turns` bounds tool-rounds *within* a continuation
        // turn — the cross-turn cap is the policy's own guard, e.g. the goal
        // runaway limit); `Stop` surfaces any note and returns `EndTurn`.
        // Defined as a macro because it must `continue`/`return` the loop.
        macro_rules! continue_or_end {
            ($assistant_msg:expr, $usage:expr) => {{
                // The tool-less max-steps summary turn must never re-trigger
                // continuation (anti-recursion): return its wrap-up directly.
                if degradation_turn {
                    return QueryOutcome::EndTurn {
                        message: $assistant_msg,
                        usage: $usage,
                    };
                }

                // Close the turn with the watcher before ending it.
                if let Some(session) = advisor.as_mut() {
                    let (notes, wake) = session.finish_turn(messages).await;
                    if !notes.is_empty() {
                        if let Some(ref tx) = event_tx {
                            for note in &notes {
                                let _ = tx.send(QueryEvent::Advisory {
                                    advisor: note.advisor.clone(),
                                    severity: note.severity.as_str().to_string(),
                                    note: note.note.clone(),
                                });
                            }
                        }
                        messages.push(crate::advisor_runtime::AdvisorSession::message_for(&notes));
                        if wake {
                            turn -= 1;
                            continue;
                        }
                    }
                }
                let decision = continuation_policy.decide(&crate::continuation::TurnEndContext {
                    session_id: &tool_ctx.session_id,
                    total_tokens_used: cost_tracker.total_tokens(),
                    turn_elapsed_secs: goal_turn_start.elapsed().as_secs(),
                });
                match decision {
                    crate::continuation::ContinuationDecision::Continue { message } => {
                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(QueryEvent::Status(
                                "Goal: continuing autonomously… (use /goal pause to stop)"
                                    .to_string(),
                            ));
                        }
                        messages.push(Message::user(message));
                        // Fresh per-continuation-turn budget, mirroring the old
                        // one-loop-per-goal-turn design.
                        turn = 0;
                        max_tokens_recovery_count = 0;
                        retries_left = 2;
                        goal_turn_start = std::time::Instant::now();
                        continue;
                    }
                    crate::continuation::ContinuationDecision::Stop { note } => {
                        if let Some(note) = note {
                            if let Some(ref tx) = event_tx {
                                let _ = tx.send(QueryEvent::Status(note));
                            }
                        }
                        return QueryOutcome::EndTurn {
                            message: $assistant_msg,
                            usage: $usage,
                        };
                    }
                }
            }};
        }

        // Check for cancellation
        if cancel_token.is_cancelled() {
            return QueryOutcome::Cancelled;
        }

        // Drain any pending user messages that were queued during the previous
        // tool-execution phase (e.g. commands entered while tools ran).
        // Mirrors the TS `messageQueueManager` drain between turns.
        if let Some(queue) = pending_messages.as_deref_mut() {
            for text in queue.drain(..) {
                debug!("Injecting pending message: {}", &text);
                messages.push(Message::user(text));
            }
        }

        // Collect anything another agent sent this one. This sits before the
        // two dispatch arms split, so both the provider arm and the raw
        // Anthropic arm deliver.
        deliver_inbox(&tool_ctx.inbox.own, messages);

        // T1-4: Drain the priority command queue (if wired up) and prepend any
        // resulting messages to the conversation before the API call.
        // Mirrors the TS `messageQueueManager` priority-queue drain.
        if let Some(ref cq) = config.command_queue {
            if !cq.is_empty() {
                let injected = drain_command_queue(cq);
                if !injected.is_empty() {
                    debug!(count = injected.len(), "Injecting command-queue messages");
                    // Prepend so that higher-priority commands appear first.
                    let tail = std::mem::take(messages);
                    messages.extend(injected);
                    messages.extend(tail);
                }
            }
        }

        // Apply tool-result budget: if the cumulative size of all tool results
        // in the conversation exceeds the configured threshold, replace the
        // oldest results with a placeholder until we're back under budget.
        // This mirrors the TS `applyToolResultBudget` call in query.ts.
        if config.tool_result_budget > 0 {
            let (budgeted, truncated) =
                apply_tool_result_budget(std::mem::take(messages), config.tool_result_budget);
            *messages = budgeted;
            if truncated > 0 {
                info!(
                    truncated,
                    budget = config.tool_result_budget,
                    "Tool-result budget exceeded: truncated {} result(s)",
                    truncated
                );
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(QueryEvent::Status(format!(
                        "[{} older tool result(s) truncated to save context]",
                        truncated
                    )));
                }
            }
        }

        // Resolve the account and the wire model ONCE, before either dispatch
        // arm builds a request. The Anthropic arm below and the provider arm
        // further down both read this, so a `"<account>/<model>"` prefix is
        // stripped for both instead of only for the provider arm.
        let route = tool_ctx.config.resolve_route(&effective_model);

        // Refuse a model the account is known not to serve rather than moving
        // the request to whichever account does serve it.
        if let Some(message) = tool_ctx.config.reject_unserved_model(&route) {
            return QueryOutcome::Error(ClaudeError::Config(message));
        }

        // Compaction runs here, at the request boundary and in front of the
        // sanitiser, for two reasons. One call reaches both dispatch arms,
        // where the old end-of-turn placement reached only the raw Anthropic
        // one and left every registry-served provider uncompacted. And a cut
        // can strand a `tool_result` whose `tool_use` was summarised away, so
        // the sanitiser standing immediately behind it repairs that in the
        // same pass rather than a turn later.
        let turn_backend = runner::backend_for(
            &route,
            config.provider_registry.as_deref(),
            &tool_ctx.config,
            client,
        );

        // Who writes the summary. The turn's own model unless the user chose a
        // compact model, which may name an account of its own: a long session
        // on an expensive account can have its summaries written somewhere
        // cheap while the conversation stays where it is.
        let compact_route = tool_ctx.config.resolve_compact_route(&route);
        let compact_backend = tool_ctx
            .config
            .reject_unserved_model(&compact_route)
            .is_none()
            .then(|| {
                runner::backend_for(
                    &compact_route,
                    config.provider_registry.as_deref(),
                    &tool_ctx.config,
                    client,
                )
            });

        let context_pass = runner::compact_before_request(
            messages,
            config,
            runner::ContextPassInput {
                route: &route,
                turn_backend: turn_backend.as_ref(),
                compact_route: &compact_route,
                compact_backend: compact_backend.as_deref(),
                session_id: &tool_ctx.session_id,
            },
            event_tx.as_ref(),
            &cancel_token,
        )
        .await;
        if context_pass.compacted {
            if let Some(ref tx) = event_tx {
                let _ = tx.send(QueryEvent::Compacted {
                    messages_before: context_pass.before,
                    messages_after: context_pass.after,
                    tokens_after: context_pass.tokens_after,
                });
            }
            // Compaction replaces the history the watcher was reading. Its
            // cursor points into a transcript that no longer exists, so drop
            // it and let the next delta start from the summary.
            if let Some(session) = advisor.as_mut() {
                session.reset(messages);
            }
        }

        // Request-boundary invariant pass (issue #229 / MI-2). Compaction,
        // max_tokens recovery, and the command-queue / pending-message drains
        // above can each independently leave the history with a broken
        // tool_use ↔ tool_result pairing (an orphan result, or a dangling
        // tool_use) that the provider rejects with HTTP 400. Heal it here —
        // the single choke point covering BOTH the legacy Anthropic path
        // (`api_messages` below) and the modern provider path (`provider_messages`
        // built later in the dispatch branch), since both derive from `messages`.
        // sanitize_history is idempotent, so a well-formed history is untouched.
        *messages = sanitize::sanitize_history(std::mem::take(messages));

        // Build API request
        let api_messages: Vec<ApiMessage> = messages.iter().map(ApiMessage::from).collect();
        // Max-steps degradation: the final summary turn is dispatched with NO
        // tool definitions so the model can only produce text (issue #230).
        let api_tools: Vec<ApiToolDefinition> = if degradation_turn {
            Vec::new()
        } else {
            tools
                .iter()
                .map(|t| ApiToolDefinition::from(&t.to_definition()))
                .collect()
        };

        // Effective effort for THIS turn. The configured effort is overridden to
        // Ultracode when the latest user message invokes the `ultracode` keyword,
        // so an ultracode turn gets the model's top reasoning (via the budget /
        // provider mapping below) plus the ultracode procedure addendum injected
        // into the system prompt.
        let effective_effort_level =
            effective_effort_for_turn(config.effort_level, messages.as_slice());

        // Verification nudge: if there are incomplete todos for this session
        // and the conversation has more than 2 turns, append a reminder.
        let system = {
            // Build a (possibly patched) config for system-prompt assembly.
            // Agent prompt prefix and todo nudge are both applied here.
            let mut patched = config.clone();

            // Progressive tool disclosure (issue #233 completion): populate
            // `enabled_tools` from the live tool set this run exposes so
            // `build_system_prompt` only emits per-tool guideline blocks for
            // tools that are actually loaded. This is the boundary #233 wired
            // up; sub-agents already set it explicitly, so only fill it in when
            // the caller left it unset.
            if patched.enabled_tools.is_none() {
                patched.enabled_tools = Some(tools.iter().map(|t| t.name().to_string()).collect());
            }

            // Apply agent system-prompt prefix: prepend before the main system prompt.
            if let Some(ref agent) = config.agent_definition {
                if let Some(ref agent_prompt) = agent.prompt {
                    patched.system_prompt = Some(match &config.system_prompt {
                        Some(existing) => format!("{}\n\n{}", agent_prompt, existing),
                        None => agent_prompt.clone(),
                    });
                }
            }

            // If managed-agent mode is active, append orchestration instructions.
            if let Some(ref ma_config) = config.managed_agents {
                if ma_config.enabled {
                    let ma_prompt =
                        crate::managed_orchestrator::managed_agent_system_prompt(ma_config);
                    patched.append_system_prompt = Some(match &patched.append_system_prompt {
                        Some(existing) => format!("{}\n\n{}", existing, ma_prompt),
                        None => ma_prompt,
                    });
                }
            }

            // Apply todo nudge on turns > 2.
            if turn > 2 && config.auto_poke {
                let nudge = build_todo_nudge(&tool_ctx.session_id);
                if !nudge.is_empty() {
                    patched.append_system_prompt = Some(match &config.append_system_prompt {
                        Some(existing) => format!("{}\n\n{}", existing, nudge),
                        None => nudge,
                    });
                }
            }

            // Goal system-prompt addendum (issue #230 / MI-3). Applied fresh
            // each turn (goal state — turns used, elapsed — changes over the
            // run) whenever goal continuation mode is active and a live goal
            // exists for this session. This relocates the addendum injection
            // from the CLI into the loop so continuation turns get it too.
            // GoalStore access here is fully synchronous (no lock held across
            // an `.await`).
            if matches!(
                config.continuation,
                crate::continuation::ContinuationMode::Goal
            ) {
                if let Some(goal) = mikmik_core::GoalStore::open_default()
                    .and_then(|s| s.get_active_goal(&tool_ctx.session_id))
                {
                    let addendum = mikmik_core::goal_system_prompt_addendum(&goal);
                    patched.append_system_prompt =
                        Some(match patched.append_system_prompt.take() {
                            Some(existing) => format!("{}\n{}", existing, addendum),
                            None => addendum,
                        });
                }
            }

            // Ultracode effort. When the effective effort for this turn is
            // Ultracode (set by the `ultracode` keyword or an explicit /effort
            // ultracode), inject the ultracode operating procedure as a per-turn
            // system addendum — the same injection path the goal addendum uses.
            // The keyword also raises the effort to top reasoning (see the
            // budget / provider mapping below). Applied fresh each turn so it
            // deactivates naturally, and composes with goal mode.
            if effective_effort_level == Some(mikmik_core::effort::EffortLevel::Ultracode) {
                let uc_addendum = mikmik_core::effort::ultracode_system_prompt_addendum();
                patched.append_system_prompt = Some(match patched.append_system_prompt.take() {
                    Some(existing) => format!("{}\n{}", existing, uc_addendum),
                    None => uc_addendum,
                });
            }

            // Output-style persona for THIS turn. An inline `rocky` / `caveman`
            // / `normal` keyword in the latest user message overrides the
            // persisted output style transiently (used for this turn, then
            // reverts); otherwise the persisted selection stands. Mirrors the
            // ultracode keyword's transient-vs-persistent behaviour above.
            let (turn_output_style, turn_output_style_prompt) =
                effective_output_style_for_turn(config, messages.as_slice());
            patched.output_style = turn_output_style;
            patched.output_style_prompt = turn_output_style_prompt;

            build_system_prompt(&patched)
        };

        let system_for_provider = system.clone(); // used by non-Anthropic dispatch below
        let mut req_builder = CreateMessageRequest::builder(&route.model, config.max_tokens)
            .messages(api_messages)
            .system(system)
            .tools(api_tools);

        // Resolve effective thinking budget:
        //   1. Explicit `thinking_budget` in config takes precedence.
        //   2. Fall back to the effort level's budget when no explicit budget is set.
        let effective_thinking_budget = config
            .thinking_budget
            .or_else(|| effective_effort_level.and_then(|el| el.thinking_budget_tokens()));

        if let Some(budget) = effective_thinking_budget {
            req_builder = req_builder.thinking(ThinkingConfig::enabled(budget));
        }

        // Apply temperature: explicit config value takes precedence, then agent override,
        // then effort-level override.
        let effective_temperature = config
            .temperature
            .or_else(|| {
                config
                    .agent_definition
                    .as_ref()
                    .and_then(|a| a.temperature)
                    .map(|t| t as f32)
            })
            .or_else(|| effective_effort_level.and_then(|el| el.temperature()));
        if let Some(t) = effective_temperature {
            req_builder = req_builder.temperature(t);
        }

        let request = req_builder.build();

        // Create a stream handler that forwards to the event channel
        let handler: Arc<dyn StreamHandler> = if let Some(ref tx) = event_tx {
            let tx = tx.clone();
            Arc::new(ChannelStreamHandler { tx })
        } else {
            Arc::new(mikmik_api::streaming::NullStreamHandler)
        };

        // Switching to the fallback model is the same decision on either
        // dispatch path, and the provider and model IDs are re-derived from
        // `effective_model` at the top of the loop, so the retry goes out as
        // the fallback. Defined as a macro because it must `continue`.
        //
        // Which failures qualify is the error type's own classification, not
        // its prose. `ProviderError` and `ClaudeError` both answer
        // `is_retryable`, and both are the capacity cases a second model can
        // serve: a rate limit, an overload, a 5xx. Reading the message instead
        // missed a 429, which renders as "Rate limited".
        macro_rules! try_fallback_model {
            ($err:expr) => {
                if !used_fallback && $err.is_retryable() {
                    if let Some(ref fb) = config.fallback_model {
                        warn!(
                            primary = %effective_model,
                            fallback = %fb,
                            "Primary model unavailable — switching to fallback"
                        );
                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(QueryEvent::Status(format!(
                                "Model unavailable — switching to fallback ({})",
                                fb
                            )));
                        }
                        effective_model = fb.clone();
                        used_fallback = true;
                        turn -= 1; // don't count this attempt against max_turns
                        continue;
                    }
                }
            };
        }

        // Account dispatch. `route` was resolved once, before the Anthropic
        // request was built, so both arms below agree on where this turn goes
        // and on the model id that travels on the wire.
        if let Some(ref registry) = config.provider_registry {
            let (provider_id_str, model_id_str) = (route.account.clone(), route.model.clone());

            // Dispatch through the provider path for non-Anthropic providers,
            // AND for Anthropic when the pre-built client has no API key
            // (user started without ANTHROPIC_API_KEY but added one via /connect).
            //
            // The wire format the account speaks, which is not always what the
            // account is called. `build_provider_options` keys its rules off
            // this, so an account filed under a name of its owner's choosing
            // still gets the right request body.
            let vendor = tool_ctx.config.vendor_id_for_account(&provider_id_str);
            let use_provider_dispatch =
                runner::dispatches_through_provider(&provider_id_str, &tool_ctx.config, client);

            if use_provider_dispatch {
                let provider =
                    runner::provider_for_turn(registry, &tool_ctx.config, &provider_id_str);
                if let Some(provider) = provider {
                    debug!(provider = %provider_id_str, model = %model_id_str, "Dispatching to non-Anthropic provider");

                    // Notify TUI that we're calling the provider using a random spinner verb
                    if let Some(ref tx) = event_tx {
                        use mikmik_core::sample_spinner_verb;
                        let seed = provider_id_str.len() ^ model_id_str.as_str().len();
                        let verb = sample_spinner_verb(seed);
                        let _ = tx.send(QueryEvent::Status(format!("✳ {}…", verb)));
                    }

                    // Build ProviderRequest from the already-assembled request data.
                    // tools comes from the api_tools we already built above.
                    // Filter unsupported modalities: replace Image/Document blocks
                    // with placeholder text when the provider doesn't support them,
                    // preventing crashes on text-only models.
                    let mut caps = provider.capabilities();
                    if let Some(model_entry) =
                        config.model_registry.as_ref().and_then(|model_registry| {
                            model_registry.get(&provider_id_str, model_id_str.as_str())
                        })
                    {
                        caps.image_input = model_entry.vision();
                        caps.tool_calling = model_entry.tool_calling;
                        caps.thinking = model_entry.reasoning;
                    }
                    // Max-steps degradation (issue #230): dispatch the final
                    // summary turn with no tools so the provider can only emit
                    // text (opencode's `toolChoice:"none"` equivalent).
                    let provider_tools: Vec<mikmik_core::types::ToolDefinition> =
                        if caps.tool_calling && !degradation_turn {
                            tools.iter().map(|t| t.to_definition()).collect()
                        } else {
                            Vec::new()
                        };
                    let provider_messages: Vec<mikmik_core::types::Message> = messages
                        .iter()
                        .map(|msg| {
                            let mut msg = msg.clone();
                            if let mikmik_core::types::MessageContent::Blocks(ref mut blocks) =
                                msg.content
                            {
                                for block in blocks.iter_mut() {
                                    match block {
                                        mikmik_core::types::ContentBlock::Image { .. }
                                            if !caps.image_input =>
                                        {
                                            *block = mikmik_core::types::ContentBlock::Text {
                                                text: "[Image not supported by this model]"
                                                    .to_string(),
                                            };
                                        }
                                        mikmik_core::types::ContentBlock::Document { .. }
                                            if !caps.pdf_input =>
                                        {
                                            *block = mikmik_core::types::ContentBlock::Text {
                                                text: "[PDF not supported by this model]"
                                                    .to_string(),
                                            };
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            msg
                        })
                        .collect();

                    let provider_request = mikmik_api::ProviderRequest {
                        model: model_id_str.clone(),
                        messages: provider_messages,
                        system_prompt: Some(system_for_provider.clone()),
                        tools: provider_tools,
                        max_tokens: config.max_tokens,
                        temperature: effective_temperature.map(|t| t as f64),
                        top_p: None,
                        top_k: None,
                        stop_sequences: vec![],
                        thinking: if caps.thinking {
                            effective_thinking_budget.map(mikmik_api::ThinkingConfig::enabled)
                        } else {
                            None
                        },
                        // `vendor`, not `provider_id_str`: the rules inside key
                        // off the wire format, so an account filed under a name
                        // of its owner's choosing would match none of them and
                        // silently send an empty body. The options, in contrast,
                        // belong to the account itself.
                        provider_options: build_provider_options(
                            &vendor,
                            model_id_str.as_str(),
                            effective_effort_level,
                            effective_thinking_budget,
                            tool_ctx
                                .config
                                .provider_configs
                                .get(&provider_id_str)
                                .map(|entry| &entry.options),
                        ),
                    };

                    // Use create_message_stream so the TUI receives real-time
                    // text deltas instead of waiting for the full response.
                    let mut stream = match provider.create_message_stream(provider_request).await {
                        Ok(s) => s,
                        Err(e) => {
                            try_fallback_model!(e);
                            error!(provider = %provider_id_str, error = %e, "Provider stream failed");
                            return QueryOutcome::Error(mikmik_core::error::ClaudeError::Api(
                                e.to_string(),
                            ));
                        }
                    };

                    // Accumulators for building the final assistant message.
                    let mut text_chunks: Vec<String> = Vec::new();
                    // Accumulate reasoning/thinking content for providers like
                    // DeepSeek that require reasoning_content to be sent back.
                    let mut thinking_chunks: Vec<String> = Vec::new();
                    // tool_call_blocks: index → (id, name, accumulated_json, thought_signature)
                    // thought_signature carries Gemini's opaque per-call signature
                    // through stream assembly so it survives into the persisted
                    // ToolUse block and is echoed back next turn (#311).
                    let mut tool_call_blocks: std::collections::HashMap<
                        usize,
                        (String, String, String, Option<String>),
                    > = std::collections::HashMap::new();
                    let mut usage = UsageInfo::default();
                    let mut stop_str = "end_turn".to_string();
                    let mut msg_id = uuid::Uuid::new_v4().to_string();

                    use futures::StreamExt as ProviderStreamExt;
                    let provider_stall_timeout = std::time::Duration::from_secs(45);
                    let provider_stall = tokio::time::sleep(provider_stall_timeout);
                    tokio::pin!(provider_stall);
                    let mut provider_stream_stalled = false;
                    // Set when the stream yields a mid-stream `Err`. The
                    // accumulated text/tool-calls are then incomplete and MUST
                    // NOT be assembled into a "completed" turn (issue #215).
                    let mut provider_stream_error: Option<String> = None;
                    // Set when a rule matched what the model was writing. The
                    // accumulated text is then thrown away, exactly as it is on
                    // a stall: the assistant message is built after this loop,
                    // so there is nothing to unwind.
                    let mut prose_interrupt = false;
                    // Set when the watching advisor raised a concern or a
                    // blocker while this turn was streaming. Same unwind as a
                    // rule: the half-written turn goes and is written again.
                    let mut advisor_interrupt: Vec<mikmik_core::advisor::AdvisorNote> = Vec::new();
                    prose_watch.start_turn();

                    loop {
                        tokio::select! {
                            _ = cancel_token.cancelled() => {
                                return QueryOutcome::Cancelled;
                            }
                            _ = &mut provider_stall => {
                                provider_stream_stalled = true;
                                break;
                            }
                            event = stream.next() => {
                                provider_stall.as_mut().reset(tokio::time::Instant::now() + provider_stall_timeout);
                                match event {
                                    None => break,
                                    Some(Err(e)) => {
                                        error!(provider = %provider_id_str, error = %e, "Provider stream error");
                                        provider_stream_error = Some(e.to_string());
                                        break;
                                    }
                                    Some(Ok(evt)) => {
                                        // Forward to TUI via AnthropicStreamEvent mapping.
                                        if let Some(ref tx) = event_tx {
                                            if let Some(ae) = map_to_anthropic_event(&evt) {
                                                let _ = tx.send(QueryEvent::Stream(ae));
                                            }
                                        }

                                        // Checked per event rather than in the
                                        // select above: `poll_interrupt` never
                                        // blocks, and a stream with no events is
                                        // a turn with nothing to interrupt.
                                        if let Some(session) = advisor.as_mut() {
                                            if let crate::advisor_runtime::Interrupt::Stop(notes) =
                                                session.poll_interrupt(turn)
                                            {
                                                advisor_interrupt = notes;
                                                break;
                                            }
                                        }

                                        // Accumulate response data.
                                        match &evt {
                                            mikmik_api::StreamEvent::MessageStart { id, usage: u, .. } => {
                                                msg_id = id.clone();
                                                merge_provider_stream_usage(&mut usage, u);
                                            }
                                            mikmik_api::StreamEvent::ContentBlockStart {
                                                index,
                                                content_block: ContentBlock::ToolUse { id, name, thought_signature, .. },
                                            } => {
                                                tool_call_blocks.insert(*index, (id.clone(), name.clone(), String::new(), thought_signature.clone()));
                                            }
                                            mikmik_api::StreamEvent::TextDelta { text, .. } => {
                                                text_chunks.push(text.clone());
                                                if prose_watch.push(text, mikmik_core::rules::ProseStream::Text) {
                                                    prose_interrupt = true;
                                                    break;
                                                }
                                            }
                                            mikmik_api::StreamEvent::ThinkingDelta { thinking, .. } => {
                                                thinking_chunks.push(thinking.clone());
                                                if prose_watch.push(thinking, mikmik_core::rules::ProseStream::Thinking) {
                                                    prose_interrupt = true;
                                                    break;
                                                }
                                            }
                                            mikmik_api::StreamEvent::ReasoningDelta { reasoning, .. } => {
                                                thinking_chunks.push(reasoning.clone());
                                                if prose_watch.push(reasoning, mikmik_core::rules::ProseStream::Thinking) {
                                                    prose_interrupt = true;
                                                    break;
                                                }
                                            }
                                            mikmik_api::StreamEvent::InputJsonDelta { index, partial_json } => {
                                                if let Some((_, _, buf, _)) = tool_call_blocks.get_mut(index) {
                                                    buf.push_str(partial_json);
                                                }
                                            }
                                            mikmik_api::StreamEvent::MessageDelta { stop_reason, usage: u } => {
                                                stop_str = match stop_reason {
                                                    Some(mikmik_api::provider_types::StopReason::ToolUse) => "tool_use".to_string(),
                                                    Some(mikmik_api::provider_types::StopReason::MaxTokens) => "max_tokens".to_string(),
                                                    Some(mikmik_api::provider_types::StopReason::StopSequence) => "stop_sequence".to_string(),
                                                    Some(mikmik_api::provider_types::StopReason::ContentFiltered) => "content_filtered".to_string(),
                                                    Some(mikmik_api::provider_types::StopReason::EndTurn) => "end_turn".to_string(),
                                                    Some(mikmik_api::provider_types::StopReason::Other(s)) => s.clone(),
                                                    None => "end_turn".to_string(),
                                                };
                                                if let Some(u) = u {
                                                    merge_provider_stream_usage(&mut usage, u);
                                                }
                                            }
                                            mikmik_api::StreamEvent::MessageStop => break,
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // The watcher raised something worth stopping for. Same
                    // unwind as a rule, and the cooldown it just started is
                    // what stops a watcher from holding the turn open.
                    if !advisor_interrupt.is_empty() {
                        if let Some(ref tx) = event_tx {
                            for note in &advisor_interrupt {
                                let _ = tx.send(QueryEvent::Advisory {
                                    advisor: note.advisor.clone(),
                                    severity: note.severity.as_str().to_string(),
                                    note: note.note.clone(),
                                });
                            }
                        }
                        messages.push(crate::advisor_runtime::AdvisorSession::message_for(
                            &advisor_interrupt,
                        ));
                        turn -= 1;
                        continue;
                    }

                    // A rule spoke about what was being written. Drop the half
                    // answer, hand the rule to the model, and let it write the
                    // turn again. This does not count against `max_turns`,
                    // which is what `ProseWatch`'s own budget is there to bound.
                    if prose_interrupt {
                        if let Some(msg) = prose_watch.take_message().await {
                            if let Some(ref tx) = event_tx {
                                let _ = tx.send(QueryEvent::Status(
                                    "A rule matched what was being written — writing it again…"
                                        .to_string(),
                                ));
                            }
                            messages.push(msg);
                        }
                        turn -= 1;
                        continue;
                    }

                    // If the stream stalled (no data for 45s), retry.
                    if provider_stream_stalled && retries_left > 0 {
                        retries_left -= 1;
                        warn!(provider = %provider_id_str, model = %model_id_str, retries_left, "Provider stream stalled — retrying");
                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(QueryEvent::Status(format!(
                                "No response for 45s — retrying ({} left)…",
                                retries_left + 1
                            )));
                        }
                        turn -= 1;
                        continue;
                    }

                    // A mid-stream error means the accumulated text and
                    // tool-call JSON are incomplete/untrustworthy. Do NOT fall
                    // through to assemble and execute tools from a truncated
                    // stream (issue #215 — an Edit/Write could otherwise run
                    // with empty `{}` args). Mirror the Anthropic branch's
                    // retry semantics: retry the turn if retries remain,
                    // otherwise surface the failure as a QueryOutcome::Error.
                    if let Some(err) = provider_stream_error {
                        if retries_left > 0 {
                            retries_left -= 1;
                            warn!(
                                provider = %provider_id_str,
                                model = %model_id_str,
                                retries_left,
                                error = %err,
                                "Provider stream error — retrying turn"
                            );
                            if let Some(ref tx) = event_tx {
                                let _ = tx.send(QueryEvent::Status(format!(
                                    "Stream error — retrying ({} left)…",
                                    retries_left + 1
                                )));
                            }
                            turn -= 1;
                            continue;
                        }
                        error!(
                            provider = %provider_id_str,
                            model = %model_id_str,
                            error = %err,
                            "Provider stream error — retries exhausted; aborting turn"
                        );
                        return QueryOutcome::Error(ClaudeError::Api(format!(
                            "Provider '{}' stream error (model '{}'): {}",
                            provider_id_str, model_id_str, err
                        )));
                    }

                    // Build the content blocks from accumulated stream data.
                    let mut content_blocks: Vec<ContentBlock> = Vec::new();

                    // Thinking / reasoning block — must come first so that
                    // inject_reasoning_for_tool_turns can find it later.
                    let combined_thinking = thinking_chunks.join("");
                    if !combined_thinking.is_empty() {
                        content_blocks.push(ContentBlock::Thinking {
                            thinking: combined_thinking.clone(),
                            signature: String::new(),
                        });
                    }

                    let combined_text = text_chunks.join("");
                    if !combined_text.is_empty() {
                        content_blocks.push(ContentBlock::Text {
                            text: combined_text.clone(),
                        });
                    }

                    // Reconstruct tool-use blocks (sorted by index for determinism).
                    let mut tc_indices: Vec<usize> = tool_call_blocks.keys().cloned().collect();
                    tc_indices.sort();
                    // Tool calls whose accumulated JSON arguments failed to
                    // parse. We still emit a tool_use block (so the assistant
                    // message stays well-formed and every tool_use has a
                    // matching tool_result), but we must NOT execute the tool
                    // with empty/garbage input — instead we surface a tool
                    // error to the model so it can retry (issue #215).
                    let mut malformed_tool_calls: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    for idx in tc_indices {
                        if let Some((id, name, json_str, thought_signature)) =
                            tool_call_blocks.remove(&idx)
                        {
                            let input = match parse_tool_args(&json_str) {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(
                                        provider = %provider_id_str,
                                        tool = %name,
                                        tool_id = %id,
                                        error = %e,
                                        "Tool-call arguments failed to parse (truncated/malformed JSON); surfacing a tool error instead of executing with empty args"
                                    );
                                    malformed_tool_calls.insert(id.clone());
                                    // Placeholder input — this call is never executed.
                                    serde_json::json!({})
                                }
                            };
                            content_blocks.push(ContentBlock::ToolUse {
                                id,
                                name,
                                input,
                                thought_signature,
                            });
                        }
                    }

                    let mut assistant_msg = Message {
                        role: mikmik_core::types::Role::Assistant,
                        content: mikmik_core::types::MessageContent::Blocks(content_blocks.clone()),
                        uuid: Some(msg_id),
                        cost: None,
                        snapshot_patch: None,
                        timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        tool_durations: None,
                    };

                    runner::record_turn_usage(
                        &mut assistant_msg,
                        &effective_model,
                        runner::pricing_for_turn(config, &tool_ctx.config, &route),
                        &usage,
                        cost_tracker.as_ref(),
                        &tool_ctx.session_id,
                    );

                    messages.push(assistant_msg.clone());

                    // The same PostModelTurn hooks the Anthropic arm fires.
                    // They used to run only there, so a user's hook was silently
                    // skipped on every other account.
                    if runner::apply_post_model_turn(
                        &assistant_msg,
                        tool_ctx,
                        messages,
                        event_tx.as_ref(),
                    ) == runner::PostModelTurn::Veto
                    {
                        let last = messages
                            .last()
                            .cloned()
                            .unwrap_or_else(|| Message::assistant("Hook blocked continuation."));
                        return QueryOutcome::EndTurn {
                            message: last,
                            usage,
                        };
                    }

                    // Handle tool-use turn: execute tools and loop.
                    let tool_use_blocks: Vec<_> = content_blocks
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::ToolUse {
                                id, name, input, ..
                            } = b
                            {
                                Some((id.clone(), name.clone(), input.clone()))
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Execute tools if any tool_use blocks were returned.
                    // Note: we check the blocks themselves rather than relying
                    // solely on stop_str == "tool_use" because many OpenAI-
                    // compatible providers (Ollama, LM Studio, etc.) return
                    // finish_reason "stop" even when tool calls are present.
                    if !tool_use_blocks.is_empty() {
                        let mut tool_results = Vec::new();
                        let mut tool_durations = Vec::new();
                        for (tool_id, tool_name, tool_input) in tool_use_blocks {
                            // Notify TUI that a tool is starting (matches Anthropic path).
                            if let Some(ref tx) = event_tx {
                                let _ = tx.send(QueryEvent::ToolStart {
                                    tool_name: tool_name.clone(),
                                    tool_id: tool_id.clone(),
                                    input_json: tool_input.to_string(),
                                });
                            }
                            let malformed = malformed_tool_calls.contains(&tool_id);
                            let result = if malformed {
                                // Never execute a tool whose arguments could not
                                // be parsed — return an error the model can see
                                // and recover from (issue #215).
                                ToolResult::error(format!(
                                    "Tool call '{}' was not executed: its arguments were malformed or truncated JSON. Retry the tool call with complete, valid JSON arguments.",
                                    tool_name
                                ))
                            } else if let Some(blocked) =
                                run_pre_tool_hooks(tool_ctx, &tool_name, &tool_input).await
                            {
                                blocked
                            } else {
                                execute_tool(&tool_name, &tool_id, &tool_input, tools, tool_ctx)
                                    .await
                            };
                            // A blocked tool still reports, matching the other
                            // dispatch arm; only an unparsed call stays silent,
                            // because no hook ever saw it.
                            if !malformed {
                                run_post_tool_hooks(tool_ctx, &tool_name, &tool_input, &result)
                                    .await;
                            }
                            if let Some(ref tx) = event_tx {
                                let _ = tx.send(QueryEvent::ToolEnd {
                                    tool_name: tool_name.clone(),
                                    tool_id: tool_id.clone(),
                                    result: result.content.clone(),
                                    is_error: result.is_error,
                                    duration_ms: result.duration_ms,
                                });
                            }
                            if let Some(took) = result.duration_ms {
                                tool_durations.push((tool_id.clone(), took));
                            }
                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: tool_id,
                                content: mikmik_core::types::ToolResultContent::Text(
                                    result.content,
                                ),
                                is_error: Some(result.is_error),
                            });
                        }
                        messages.push(
                            Message::user_blocks(tool_results).with_tool_durations(tool_durations),
                        );
                        // Hand the watcher the round that just ran. The primary
                        // is mid-turn here, so a note that comes back can still
                        // stop the work before the next tool goes out.
                        if let Some(session) = advisor.as_mut() {
                            session.push_delta(messages, true);
                        }
                        continue; // loop for next turn
                    }

                    // End turn — notify TUI and return.
                    runner::ensure_turn_has_output(
                        &mut assistant_msg,
                        messages,
                        event_tx.as_ref(),
                        &stop_str,
                    );

                    // The Stop hooks, session-memory extraction and the
                    // AutoDream check. All three used to run only in the
                    // Anthropic arm, so on any other account no Stop hook fired
                    // and no memory was ever written.
                    runner::fire_end_of_turn(&assistant_msg, tool_ctx, config, messages, &route)
                        .await;

                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::TurnComplete {
                            stop_reason: stop_str.clone(),
                            turn,
                            usage: Some(usage.clone()),
                            model: effective_model.clone(),
                        });
                    }

                    // Attach snapshot patch covering all file changes this query.
                    if let (Some(ref snap), Some(ref hash)) = (&shadow_snap, &initial_snapshot) {
                        let patch = snap.patch(hash).await;
                        if !patch.files.is_empty() {
                            assistant_msg.snapshot_patch = Some(patch);
                        }
                    }

                    continue_or_end!(assistant_msg, usage);
                } else if provider_id_str != "anthropic" {
                    // Non-Anthropic provider detected but no API key / credentials
                    // available.  Return a clear error instead of silently falling
                    // through to the Anthropic client.
                    let hint = match provider_id_str.as_str() {
                        "google" => "Set GOOGLE_API_KEY or run `mikmik auth login --provider google`.",
                        "openai" => "Set OPENAI_API_KEY or run `mikmik auth login --provider openai`.",
                        "groq" => "Set GROQ_API_KEY.",
                        "mistral" => "Set MISTRAL_API_KEY.",
                        "deepseek" => "Set DEEPSEEK_API_KEY.",
                        "xai" => "Set XAI_API_KEY.",
                        "github-copilot" => "Reconnect GitHub Copilot via /connect, or set GITHUB_TOKEN.",
                        "cohere" => "Set COHERE_API_KEY.",
                        _ => "Set the appropriate API key environment variable or use `mikmik auth login`.",
                    };
                    error!(
                        provider = %provider_id_str,
                        model = %model_id_str,
                        "No credentials found for provider"
                    );
                    return QueryOutcome::Error(ClaudeError::Api(format!(
                        "No API key for provider '{}' (model '{}'). {}",
                        provider_id_str, model_id_str, hint
                    )));
                }
                // Anthropic with no auth_store key: fall through to the raw
                // client path below (which has its own deferred key validation
                // with detailed model-specific hints).
            }
        }

        // Send to API
        debug!(turn, model = %effective_model, "Sending API request");
        let mut stream_rx = match client.create_message_stream(request, handler).await {
            Ok(rx) => rx,
            Err(e) => {
                try_fallback_model!(e);
                error!(error = %e, "API request failed");
                return QueryOutcome::Error(e);
            }
        };

        // Accumulate the streamed response.
        // A stall timeout auto-retries the request if no data arrives for 45s
        // (some providers are slow; we don't want to give up too early).
        const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let mut accumulator = StreamAccumulator::new();
        let stall_deadline = tokio::time::sleep(STALL_TIMEOUT);
        tokio::pin!(stall_deadline);

        // Same interrupt as the provider arm. The accumulator holds the partial
        // message, and `finish()` is only called after this loop, so breaking
        // out early leaves nothing behind.
        let mut prose_interrupt = false;
        let mut advisor_interrupt: Vec<mikmik_core::advisor::AdvisorNote> = Vec::new();
        prose_watch.start_turn();

        let stream_stalled = loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    return QueryOutcome::Cancelled;
                }
                _ = &mut stall_deadline => {
                    // No data for 45s — stall detected
                    break true;
                }
                event = stream_rx.recv() => {
                    // Reset stall timer on every received event.
                    stall_deadline.as_mut().reset(tokio::time::Instant::now() + STALL_TIMEOUT);
                    match event {
                        Some(evt) => {
                            accumulator.on_event(&evt);

                            // Same per-event poll as the provider arm.
                            if let Some(session) = advisor.as_mut() {
                                if let crate::advisor_runtime::Interrupt::Stop(notes) =
                                    session.poll_interrupt(turn)
                                {
                                    advisor_interrupt = notes;
                                    break false;
                                }
                            }

                            match &evt {
                                AnthropicStreamEvent::ContentBlockDelta { delta, .. } => {
                                    let written = match delta {
                                        mikmik_api::streaming::ContentDelta::TextDelta { text } => {
                                            Some((text, mikmik_core::rules::ProseStream::Text))
                                        }
                                        mikmik_api::streaming::ContentDelta::ThinkingDelta { thinking } => {
                                            Some((thinking, mikmik_core::rules::ProseStream::Thinking))
                                        }
                                        _ => None,
                                    };
                                    if let Some((written, stream)) = written {
                                        if prose_watch.push(written, stream) {
                                            prose_interrupt = true;
                                            break false;
                                        }
                                    }
                                }
                                AnthropicStreamEvent::Error { error_type, message } => {
                                    if error_type == "overloaded_error" {
                                        warn!(model = %effective_model, "API overloaded");
                                    }
                                    error!(error_type, message, "Stream error");
                                }
                                AnthropicStreamEvent::MessageStop => break false,
                                _ => {}
                            }
                        }
                        None => break false, // Stream ended
                    }
                }
            }
        };

        if !advisor_interrupt.is_empty() {
            if let Some(ref tx) = event_tx {
                for note in &advisor_interrupt {
                    let _ = tx.send(QueryEvent::Advisory {
                        advisor: note.advisor.clone(),
                        severity: note.severity.as_str().to_string(),
                        note: note.note.clone(),
                    });
                }
            }
            messages.push(crate::advisor_runtime::AdvisorSession::message_for(
                &advisor_interrupt,
            ));
            turn -= 1;
            continue;
        }

        if prose_interrupt {
            if let Some(msg) = prose_watch.take_message().await {
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(QueryEvent::Status(
                        "A rule matched what was being written — writing it again…".to_string(),
                    ));
                }
                messages.push(msg);
            }
            turn -= 1;
            continue;
        }

        if stream_stalled && retries_left > 0 {
            retries_left -= 1;
            warn!(model = %effective_model, retries_left, "Stream stalled — retrying request");
            if let Some(ref tx) = event_tx {
                let _ = tx.send(QueryEvent::Status(format!(
                    "No response for 45s — retrying ({} left)…",
                    retries_left + 1
                )));
            }
            turn -= 1; // don't count this stalled attempt
            continue;
        }

        let (mut assistant_msg, usage, stop_reason) = accumulator.finish();

        runner::record_turn_usage(
            &mut assistant_msg,
            &effective_model,
            runner::pricing_for_turn(config, &tool_ctx.config, &route),
            &usage,
            cost_tracker.as_ref(),
            &tool_ctx.session_id,
        );

        // Budget guard: abort the loop if the configured USD cap is exceeded.
        if let Some(limit) = config.max_budget_usd {
            let spent = cost_tracker.total_cost_usd();
            if spent >= limit {
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(QueryEvent::Status(format!(
                        "Budget limit ${:.4} exceeded (spent ${:.4}) — stopping.",
                        limit, spent
                    )));
                }
                return QueryOutcome::BudgetExceeded {
                    cost_usd: spent,
                    limit_usd: limit,
                };
            }
        }

        // Append assistant message to conversation
        messages.push(assistant_msg.clone());

        // If the provider returned an unknown stop reason but the assistant
        // message contains tool_use blocks, treat it as tool_use so we don't
        // silently end the turn (issue #149: agent stops after tool call for
        // providers that emit non-standard finish reasons).
        let raw_stop = stop_reason.as_deref().unwrap_or("end_turn");
        let stop = match raw_stop {
            "end_turn" | "tool_use" | "max_tokens" | "stop_sequence" | "content_filtered" => {
                raw_stop
            }
            _ if !assistant_msg.get_tool_use_blocks().is_empty() => {
                warn!(
                    stop_reason = raw_stop,
                    "Unknown stop reason with tool_use blocks present; treating as tool_use"
                );
                "tool_use"
            }
            _ => raw_stop,
        };

        // Same guarantee the provider arm gives: a turn that produced nothing
        // still says so. Placed before the hooks so the Stop hook sees the text
        // the user sees. A tool round is not silent, so the helper skips it.
        runner::ensure_turn_has_output(&mut assistant_msg, messages, event_tx.as_ref(), stop);

        // T1-3: Fire PostModelTurn hooks after the model samples a response.
        // Hooks can inject blocking errors or veto continuation entirely.
        if runner::apply_post_model_turn(&assistant_msg, tool_ctx, messages, event_tx.as_ref())
            == runner::PostModelTurn::Veto
        {
            let last = messages
                .last()
                .cloned()
                .unwrap_or_else(|| Message::assistant("Hook blocked continuation."));
            return QueryOutcome::EndTurn {
                message: last,
                usage,
            };
        }

        if let Some(ref tx) = event_tx {
            let _ = tx.send(QueryEvent::TurnComplete {
                turn,
                stop_reason: stop.to_string(),
                usage: Some(usage.clone()),
                model: effective_model.clone(),
            });
        }

        match stop {
            "end_turn" => {
                runner::fire_end_of_turn(&assistant_msg, tool_ctx, config, messages, &route).await;

                // Attach snapshot patch covering all file changes this query.
                if let (Some(ref snap), Some(ref hash)) = (&shadow_snap, &initial_snapshot) {
                    let patch = snap.patch(hash).await;
                    if !patch.files.is_empty() {
                        assistant_msg.snapshot_patch = Some(patch);
                    }
                }

                continue_or_end!(assistant_msg, usage);
            }
            "max_tokens" => {
                // Mirror the TS recovery loop: inject a continuation nudge and
                // retry up to MAX_TOKENS_RECOVERY_LIMIT times before surfacing
                // the partial response as QueryOutcome::MaxTokens.
                if max_tokens_recovery_count < MAX_TOKENS_RECOVERY_LIMIT {
                    max_tokens_recovery_count += 1;
                    warn!(
                        attempt = max_tokens_recovery_count,
                        limit = MAX_TOKENS_RECOVERY_LIMIT,
                        "max_tokens hit — injecting continuation message (attempt {}/{})",
                        max_tokens_recovery_count,
                        MAX_TOKENS_RECOVERY_LIMIT,
                    );
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::Status(format!(
                            "Output token limit hit — continuing (attempt {}/{})",
                            max_tokens_recovery_count, MAX_TOKENS_RECOVERY_LIMIT
                        )));
                    }
                    // The partial assistant message must be in the history so
                    // the continuation makes sense to the model.
                    messages.push(Message::user(MAX_TOKENS_RECOVERY_MSG));
                    continue;
                }
                // Recovery exhausted — surface the partial response.
                warn!(
                    "max_tokens recovery exhausted after {} attempts",
                    MAX_TOKENS_RECOVERY_LIMIT
                );
                return QueryOutcome::MaxTokens {
                    partial_message: assistant_msg,
                    usage,
                };
            }
            "tool_use" => {
                // A completed tool-use turn counts as a successful recovery
                // boundary; reset the max_tokens retry counter.
                max_tokens_recovery_count = 0;
                // Extract tool calls and execute them
                let tool_blocks = assistant_msg.get_tool_use_blocks();
                if tool_blocks.is_empty() {
                    // Shouldn't happen but treat as end_turn
                    return QueryOutcome::EndTurn {
                        message: assistant_msg,
                        usage,
                    };
                }

                // ---------------------------------------------------------------------------
                // Streaming tool executor: parallel non-agent tool dispatch.
                //
                // Phase 1: Run PreToolUse hooks sequentially (they can block/deny execution
                //          and may display interactive permission dialogs).
                // Phase 2: Dispatch all non-blocked tool executions concurrently via
                //          futures::future::join_all, preserving original order.
                // Phase 3: Fire PostToolUse hooks + emit events, then collect results.
                //
                // This mirrors the TypeScript StreamingToolExecutor pattern.
                // ---------------------------------------------------------------------------

                // Intermediate record produced during Phase 1.
                struct PreparedTool {
                    id: String,
                    name: String,
                    input: Value,
                    /// None means the pre-hook blocked execution; the String is the error reason.
                    blocked_result: Option<ToolResult>,
                    /// A conditional rule this call matched. Put on top of the
                    /// result once the tool has run.
                    reminder: Option<String>,
                }

                // Phase 1: sequential pre-hook pass.
                let mut prepared: Vec<PreparedTool> = Vec::with_capacity(tool_blocks.len());
                for block in tool_blocks {
                    if let ContentBlock::ToolUse {
                        id, name, input, ..
                    } = block
                    {
                        // Clone from the references returned by get_tool_use_blocks()
                        let id = id.clone();
                        let name = name.clone();
                        let input = input.clone();

                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(QueryEvent::ToolStart {
                                tool_name: name.clone(),
                                tool_id: id.clone(),
                                input_json: input.to_string(),
                            });
                        }

                        let mut blocked_result = run_pre_tool_hooks(tool_ctx, &name, &input).await;

                        // The project's own rules, checked where the arguments
                        // are complete and the tool has not started. A hook
                        // that already refused the call is left alone: it said
                        // its piece and the rule has nothing to add.
                        let mut reminder = None;
                        if blocked_result.is_none() {
                            match check_rules(tool_ctx, &name, &input).await {
                                RuleOutcome::Silent => {}
                                RuleOutcome::Remind(text) => reminder = Some(text),
                                RuleOutcome::Block(result) => blocked_result = Some(result),
                            }
                        }

                        prepared.push(PreparedTool {
                            id,
                            name,
                            input,
                            blocked_result,
                            reminder,
                        });
                    }
                }

                // Phase 2: build execution futures for non-blocked tools and join them.
                // Blocked tools yield a ready future with the pre-computed error result.
                // Non-blocked tools execute concurrently via join_all.
                // Each async block owns its cloned name/input so there are no lifetime issues.
                let exec_futures: Vec<_> = prepared
                    .iter()
                    .map(|p| {
                        if p.blocked_result.is_some() {
                            let r = p.blocked_result.clone().unwrap();
                            futures::future::Either::Left(async move { r })
                        } else {
                            let name = p.name.clone();
                            let id = p.id.clone();
                            let input = p.input.clone();
                            futures::future::Either::Right(async move {
                                execute_tool(&name, &id, &input, tools, tool_ctx).await
                            })
                        }
                    })
                    .collect();

                // Run all tool futures concurrently, but race the batch against the
                // loop's cancel token (issue #218): on cancellation the in-flight
                // tools are abandoned promptly instead of blocking until the
                // slowest one finishes, and a cancelled ToolResult is synthesized
                // for EVERY tool so each tool_use still gets a matching tool_result
                // and the message history stays well-formed.
                let (exec_results, batch_cancelled) =
                    run_tool_batch(exec_futures, &tool_ctx.cancel_token).await;

                // Phase 3: post-hooks, event emission, and result block assembly.
                // When the batch was cancelled we skip the awaiting PostToolUse
                // hooks (they run external commands and would defeat the point of
                // returning promptly) but still emit ToolEnd + build every result
                // block so the conversation and TUI stay consistent.
                let mut result_blocks: Vec<ContentBlock> = Vec::with_capacity(prepared.len());
                let mut tool_durations: Vec<(String, u64)> = Vec::with_capacity(prepared.len());
                for (p, mut result) in prepared.iter().zip(exec_results) {
                    if !batch_cancelled {
                        run_post_tool_hooks(tool_ctx, &p.name, &p.input, &result).await;
                    }

                    // A rule this call matched rides on top of the result, so
                    // the model reads it while it is still on this file.
                    if let Some(ref reminder) = p.reminder {
                        result.content = format!("{reminder}\n\n{}", result.content);
                    }

                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(QueryEvent::ToolEnd {
                            tool_name: p.name.clone(),
                            tool_id: p.id.clone(),
                            result: result.content.clone(),
                            is_error: result.is_error,
                            duration_ms: result.duration_ms,
                        });
                    }

                    if let Some(took) = result.duration_ms {
                        tool_durations.push((p.id.clone(), took));
                    }
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: p.id.clone(),
                        content: ToolResultContent::Text(result.content),
                        is_error: if result.is_error { Some(true) } else { None },
                    });
                }

                // Append tool results as a user message so the history remains
                // valid (every tool_use is answered) even on cancellation.
                messages
                    .push(Message::user_blocks(result_blocks).with_tool_durations(tool_durations));

                // If the batch was abandoned due to cancellation, stop the loop
                // now rather than sending the (cancelled) results back to the model.
                if batch_cancelled {
                    return QueryOutcome::Cancelled;
                }

                // Hand the watcher the round that just ran. The primary is
                // mid-turn here, so a note that comes back can still stop the
                // work before the next tool goes out.
                if let Some(session) = advisor.as_mut() {
                    session.push_delta(messages, true);
                }

                // Continue the loop to send results back to the model
                continue;
            }
            "stop_sequence" => {
                runner::fire_stop_hooks(&assistant_msg, tool_ctx).await;
                if let (Some(ref snap), Some(ref hash)) = (&shadow_snap, &initial_snapshot) {
                    let patch = snap.patch(hash).await;
                    if !patch.files.is_empty() {
                        assistant_msg.snapshot_patch = Some(patch);
                    }
                }
                continue_or_end!(assistant_msg, usage);
            }
            other => {
                warn!(
                    stop_reason = other,
                    "Unknown stop reason, treating as end_turn"
                );
                runner::fire_stop_hooks(&assistant_msg, tool_ctx).await;
                if let (Some(ref snap), Some(ref hash)) = (&shadow_snap, &initial_snapshot) {
                    let patch = snap.patch(hash).await;
                    if !patch.files.is_empty() {
                        assistant_msg.snapshot_patch = Some(patch);
                    }
                }
                continue_or_end!(assistant_msg, usage);
            }
        }
    }
}

/// Stream handler that forwards events to an unbounded channel.
struct ChannelStreamHandler {
    tx: mpsc::UnboundedSender<QueryEvent>,
}

impl StreamHandler for ChannelStreamHandler {
    fn on_event(&self, event: &AnthropicStreamEvent) {
        let _ = self.tx.send(QueryEvent::Stream(event.clone()));
    }
}

// ---------------------------------------------------------------------------
// Provider stream event mapping
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_api::SystemPrompt;

    fn managed_config(total_budget_usd: Option<f64>, enabled: bool) -> mikmik_core::Config {
        mikmik_core::Config {
            managed_agents: Some(mikmik_core::ManagedAgentConfig {
                enabled,
                manager_model: "anthropic/claude-opus-4-6".to_string(),
                executor_model: "anthropic/claude-sonnet-4-6".to_string(),
                executor_max_turns: 10,
                max_concurrent_executors: 4,
                total_budget_usd,
                preset_name: None,
                executor_isolation: false,
            }),
            ..Default::default()
        }
    }

    /// The setting used to reach the model as prompt text and nothing else, so
    /// the run went past the figure the user had set.
    #[test]
    fn a_managed_budget_becomes_the_loop_s_own_cap() {
        let config = QueryConfig::from_config(&managed_config(Some(5.0), true));
        assert_eq!(config.max_budget_usd, Some(5.0));
    }

    #[test]
    fn a_disabled_managed_config_sets_no_cap() {
        let config = QueryConfig::from_config(&managed_config(Some(5.0), false));
        assert_eq!(config.max_budget_usd, None);
    }

    #[test]
    fn a_managed_config_without_a_budget_sets_no_cap() {
        let config = QueryConfig::from_config(&managed_config(None, true));
        assert_eq!(config.max_budget_usd, None);
    }

    #[test]
    fn a_session_without_managed_agents_sets_no_cap() {
        let config = QueryConfig::from_config(&mikmik_core::Config::default());
        assert_eq!(config.max_budget_usd, None);
    }

    /// Every field spelled out because `ToolContext` has no `Default`; the
    /// address is what these tests are actually about.
    fn addressed_context(session: &str, inbox: mikmik_tools::AgentAddress) -> ToolContext {
        struct AllowAll;
        impl mikmik_core::permissions::PermissionHandler for AllowAll {
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

        ToolContext {
            working_dir: std::path::PathBuf::from("/workspace"),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: Arc::new(AllowAll),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: session.to_string(),
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
            inbox,
        }
    }

    fn text_of(message: &Message) -> String {
        match &message.content {
            mikmik_core::types::MessageContent::Text(text) => text.clone(),
            other => panic!("expected a text message, got {other:?}"),
        }
    }

    /// An address of its own is what lets a sub-agent and its parent, which
    /// share a session id, drain two different inboxes.
    #[test]
    fn a_context_without_an_address_becomes_the_session() {
        let mut ctx = addressed_context("sess-bind-address", Default::default());
        let guard = bind_address(&mut ctx);

        assert!(guard.is_some(), "the session was never registered");
        assert_eq!(ctx.inbox.own, "sess-bind-address");
        assert_eq!(ctx.inbox.name.as_deref(), Some(mikmik_tools::MAIN_NAME));
    }

    #[test]
    fn a_context_that_already_has_an_address_keeps_it() {
        let mut ctx = addressed_context(
            "sess-keep-address",
            mikmik_tools::AgentAddress {
                own: "sess-keep-address:scout".to_string(),
                parent: Some("sess-keep-address".to_string()),
                name: Some("scout".to_string()),
                parent_blocked: false,
            },
        );

        let guard = bind_address(&mut ctx);

        assert!(
            guard.is_none(),
            "a sub-agent was re-registered as the session"
        );
        assert_eq!(ctx.inbox.own, "sess-keep-address:scout");
        assert_eq!(ctx.inbox.name.as_deref(), Some("scout"));
    }

    /// The text was written by another agent. Without the frame it reads as
    /// something the user typed, which is how a sub-agent would put words in
    /// the user's mouth.
    #[tokio::test]
    async fn a_delivered_message_names_its_sender() {
        use mikmik_tools::Tool as _;

        let session = "sess-deliver-framing";
        let _main = mikmik_tools::register_main(session);
        let (name, guard) = mikmik_tools::register_named(session, Some("scout"), "look");

        let sender = addressed_context(
            session,
            mikmik_tools::AgentAddress {
                own: session.to_string(),
                parent: None,
                name: Some(mikmik_tools::MAIN_NAME.to_string()),
                parent_blocked: false,
            },
        );
        let sent = mikmik_tools::SendMessageTool
            .execute(
                serde_json::json!({ "to": name, "message": "check the logs" }),
                &sender,
            )
            .await;
        assert!(!sent.is_error, "{}", sent.content);

        let mut messages = Vec::new();
        deliver_inbox(guard.key(), &mut messages);

        assert_eq!(messages.len(), 1);
        let text = text_of(&messages[0]);
        assert!(text.contains("[System]"), "{text}");
        assert!(text.contains(mikmik_tools::MAIN_NAME), "{text}");
        assert!(text.contains("check the logs"), "{text}");
    }

    #[test]
    fn an_empty_inbox_adds_nothing() {
        let mut messages = Vec::new();
        deliver_inbox("sess-empty-inbox", &mut messages);
        assert!(messages.is_empty());
    }

    /// One agent must not be able to spend another's whole context window.
    #[test]
    fn the_rendered_inbox_stays_within_its_budget() {
        let flood: Vec<mikmik_tools::AgentMessage> = (0..40)
            .map(|i| mikmik_tools::AgentMessage {
                from: "scout".to_string(),
                to: "main".to_string(),
                content: format!("{i}").repeat(1_000),
                timestamp: 0,
            })
            .collect();

        let rendered = render_inbox(&flood);

        assert!(
            rendered.len() <= MAX_INBOX_RENDER_CHARS + 200,
            "rendered {} chars",
            rendered.len()
        );
        assert!(
            rendered.contains("were dropped"),
            "the dropped messages were not reported: {rendered}"
        );
    }

    #[test]
    fn a_configured_effort_reaches_the_turn() {
        // Nothing else reads `config.effort`, so if this arm goes missing the
        // setting is written, listed, and silently ignored on every request.
        let cfg = mikmik_core::config::Config {
            effort: Some("xhigh".to_string()),
            ..Default::default()
        };
        assert_eq!(
            QueryConfig::from_config(&cfg).effort_level,
            Some(mikmik_core::effort::EffortLevel::XHigh)
        );
    }

    #[test]
    fn a_turn_carries_its_own_usage_and_price() {
        // `/stats` sums these off the stored messages, so a turn that recorded
        // nothing is a turn that spent nothing as far as every report goes.
        let usage = mikmik_core::types::UsageInfo {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 300,
        };

        let cost = cost_of_turn(
            "claude-sonnet-4-5",
            mikmik_core::cost::ModelPricing::SONNET,
            &usage,
        );

        assert_eq!(cost.input_tokens, 1_000);
        assert_eq!(cost.output_tokens, 500);
        assert_eq!(cost.cache_creation_input_tokens, 20);
        assert_eq!(cost.cache_read_input_tokens, 300);
        assert!(cost.cost_usd > 0.0, "a priced model must cost something");
    }

    #[test]
    fn no_configured_effort_leaves_the_turn_on_its_default() {
        let cfg = mikmik_core::config::Config::default();
        assert_eq!(QueryConfig::from_config(&cfg).effort_level, None);
    }

    #[test]
    fn both_turn_behaviour_toggles_default_to_on_when_unset() {
        // `None` must read as enabled, or upgrading would silently switch off
        // the summary turn and the todo reminder for every existing user.
        let cfg = mikmik_core::config::Config::default();
        let query = QueryConfig::from_config(&cfg);
        assert!(query.degradation_summary);
        assert!(query.auto_poke);
    }

    #[test]
    fn switching_a_turn_behaviour_toggle_off_reaches_the_turn() {
        // Nothing else reads either field, so a missing arm here would leave
        // the setting written, documented, and ignored on every request.
        let cfg = mikmik_core::config::Config {
            degradation_summary: Some(false),
            auto_poke: Some(false),
            ..Default::default()
        };
        let query = QueryConfig::from_config(&cfg);
        assert!(!query.degradation_summary);
        assert!(!query.auto_poke);
    }

    #[test]
    fn a_configured_turn_limit_reaches_the_turn() {
        let cfg = mikmik_core::config::Config {
            max_turns: Some(25),
            ..Default::default()
        };
        assert_eq!(QueryConfig::from_config(&cfg).max_turns, 25);

        let unset = mikmik_core::config::Config::default();
        assert_eq!(
            QueryConfig::from_config(&unset).max_turns,
            mikmik_core::constants::MAX_TURNS_DEFAULT
        );
    }

    #[test]
    fn the_unlimited_ceiling_cannot_be_reached_by_a_run() {
        // `/turns off` stores this value; the loop compares the turn counter
        // against it, so it has to survive the trip to the turn unchanged. A
        // value clamped on the way through would reinstate a limit.
        let cfg = mikmik_core::config::Config {
            max_turns: Some(mikmik_core::constants::MAX_TURNS_UNLIMITED),
            ..Default::default()
        };
        assert_eq!(
            QueryConfig::from_config(&cfg).max_turns,
            mikmik_core::constants::MAX_TURNS_UNLIMITED
        );
    }

    #[test]
    fn the_registry_constructor_carries_the_same_toggles() {
        // `from_config_with_registry` is a second, near-identical constructor;
        // a field added to one and not the other is silently dropped for every
        // caller that uses model discovery.
        let cfg = mikmik_core::config::Config {
            degradation_summary: Some(false),
            auto_poke: Some(false),
            ..Default::default()
        };
        let registry = mikmik_api::ModelRegistry::new();
        let query = QueryConfig::from_config_with_registry(&cfg, &registry);
        assert!(!query.degradation_summary);
        assert!(!query.auto_poke);
    }

    // The fallback switch fires on `is_retryable`. These pin that contract:
    // narrowing either classification for some other reason would kill
    // `--fallback-model` silently.

    #[test]
    fn a_busy_provider_is_worth_a_fallback() {
        use mikmik_api::provider_error::ProviderError;
        use mikmik_core::provider_id::ProviderId;

        assert!(ProviderError::RateLimited {
            provider: ProviderId::new("openai"),
            retry_after: None,
        }
        .is_retryable());
        assert!(ProviderError::ServerError {
            provider: ProviderId::new("openai"),
            status: Some(529),
            message: "overloaded".to_string(),
            is_retryable: true,
        }
        .is_retryable());
    }

    #[test]
    fn a_busy_anthropic_endpoint_is_worth_a_fallback() {
        assert!(ClaudeError::RateLimit.is_retryable());
        assert!(ClaudeError::ApiStatus {
            status: 529,
            message: "Overloaded".to_string(),
        }
        .is_retryable());
    }

    #[test]
    fn a_failure_the_fallback_would_share_is_not_worth_it() {
        use mikmik_api::provider_error::ProviderError;
        use mikmik_core::provider_id::ProviderId;

        // A bad key or a missing model fails the same way on the second model,
        // so switching only doubles the wait before the same error.
        assert!(!ProviderError::AuthFailed {
            provider: ProviderId::new("openai"),
            message: "invalid api key".to_string(),
        }
        .is_retryable());
        assert!(!ProviderError::ModelNotFound {
            provider: ProviderId::new("openai"),
            model: "nope".to_string(),
            suggestions: vec![],
        }
        .is_retryable());
        assert!(!ClaudeError::Auth("invalid api key".to_string()).is_retryable());
    }

    #[test]
    fn final_stream_usage_supplies_prompt_tokens_to_turn_usage() {
        let mut turn_usage = UsageInfo::default();
        let final_usage = UsageInfo {
            input_tokens: 1_200,
            output_tokens: 80,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 300,
        };

        merge_provider_stream_usage(&mut turn_usage, &final_usage);

        assert_eq!(turn_usage.input_tokens, 1_200);
        assert_eq!(turn_usage.cache_read_input_tokens, 300);
        assert_eq!(turn_usage.output_tokens, 80);
        let context_counter_increment = turn_usage.input_tokens
            + turn_usage.output_tokens
            + turn_usage.cache_creation_input_tokens
            + turn_usage.cache_read_input_tokens;
        assert_eq!(context_counter_increment, 1_580);
    }

    #[test]
    fn output_only_final_usage_preserves_start_input_tokens() {
        let mut turn_usage = UsageInfo {
            input_tokens: 900,
            output_tokens: 0,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 0,
        };
        let final_usage = UsageInfo {
            output_tokens: 75,
            ..Default::default()
        };

        merge_provider_stream_usage(&mut turn_usage, &final_usage);

        assert_eq!(turn_usage.input_tokens, 900);
        assert_eq!(turn_usage.cache_creation_input_tokens, 100);
        assert_eq!(turn_usage.output_tokens, 75);
    }

    fn make_config(sys: Option<&str>, append: Option<&str>) -> QueryConfig {
        QueryConfig {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 4096,
            max_turns: 10,
            degradation_summary: true,
            auto_poke: true,
            auto_compact: true,
            compact_threshold: mikmik_core::constants::DEFAULT_COMPACT_THRESHOLD,
            system_prompt: sys.map(String::from),
            append_system_prompt: append.map(String::from),
            output_style: mikmik_core::system_prompt::OutputStyle::Default,
            output_style_prompt: None,
            working_directory: None,
            workspace_roots: std::collections::BTreeMap::new(),
            thinking_budget: None,
            temperature: None,
            tool_result_budget: 50_000,
            effort_level: None,
            command_queue: None,
            skill_index: None,
            max_budget_usd: None,
            fallback_model: None,
            provider_registry: None,
            agent_name: None,
            agent_definition: None,
            model_registry: None,
            managed_agents: None,
            enabled_tools: None,
            continuation: crate::continuation::ContinuationMode::Default,
            companion_addendum: None,
            auto_memory_enabled: false,
        }
    }

    // ---- parse_tool_args tests (issue #215) ---------------------------------

    #[test]
    fn test_parse_tool_args_valid_object() {
        // A complete JSON object parses to the same value.
        let v = parse_tool_args("{\"a\":1}").expect("valid JSON should parse");
        assert_eq!(v, serde_json::json!({ "a": 1 }));

        let v = parse_tool_args("{\"path\": \"/tmp/x\", \"content\": \"hi\"}")
            .expect("valid JSON should parse");
        assert_eq!(v["path"], "/tmp/x");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn test_parse_tool_args_empty_is_empty_object() {
        // No-argument tool calls arrive as an empty (or whitespace-only)
        // buffer and must map to `{}` so the happy path still works.
        assert_eq!(parse_tool_args("").unwrap(), serde_json::json!({}));
        assert_eq!(parse_tool_args("   ").unwrap(), serde_json::json!({}));
        assert_eq!(parse_tool_args("\n\t ").unwrap(), serde_json::json!({}));
    }

    #[test]
    fn test_parse_tool_args_truncated_is_error_not_empty_object() {
        // The core of issue #215: a truncated/malformed stream must surface
        // an error, NOT silently become `{}` (which would run Edit/Write with
        // empty arguments).
        assert!(
            parse_tool_args("{\"a\":").is_err(),
            "truncated JSON must be an error"
        );
        assert!(
            parse_tool_args("{\"path\": \"/etc/passwd").is_err(),
            "truncated string value must be an error"
        );
        assert!(
            parse_tool_args("{not json}").is_err(),
            "invalid JSON must be an error"
        );

        // Regression guard: the failing cases must never resolve to `{}`.
        for bad in ["{\"a\":", "{\"path\": \"/etc/passwd", "{not json}"] {
            let resolved = parse_tool_args(bad).unwrap_or(serde_json::json!({}));
            // The OLD buggy behavior turned these into `{}`; assert we now
            // *detect* the error rather than relying on that fallback.
            assert!(
                parse_tool_args(bad).is_err(),
                "expected error for {:?}, but got {}",
                bad,
                resolved
            );
        }
    }

    // ---- build_system_prompt tests ------------------------------------------

    #[test]
    fn test_system_prompt_default_when_empty() {
        // The default prompt (no custom system prompt set) should include the
        // MikMik attribution and standard sections.
        let cfg = make_config(None, None);
        let prompt = build_system_prompt(&cfg);
        if let SystemPrompt::Text(text) = prompt {
            assert!(
                text.contains("MikMik") || text.contains("Claude agent"),
                "Default prompt should contain attribution: {}",
                text
            );
            assert!(
                text.contains(mikmik_core::system_prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
                "Default prompt must contain the dynamic boundary marker"
            );
        } else {
            panic!("Expected SystemPrompt::Text");
        }
    }

    #[test]
    fn test_system_prompt_with_custom() {
        // A custom system prompt is injected into the cacheable section as
        // <custom_instructions>; the default sections are still present.
        let cfg = make_config(Some("You are a code reviewer."), None);
        let prompt = build_system_prompt(&cfg);
        if let SystemPrompt::Text(text) = prompt {
            assert!(
                text.contains("You are a code reviewer."),
                "Custom prompt text should appear in the output"
            );
            assert!(
                text.contains("MikMik") || text.contains("Claude agent"),
                "Default attribution should still be present"
            );
        } else {
            panic!("Expected SystemPrompt::Text");
        }
    }

    #[test]
    fn test_system_prompt_with_append() {
        // Appended text lands after the dynamic boundary.
        let cfg = make_config(Some("Base prompt."), Some("Additional context."));
        let prompt = build_system_prompt(&cfg);
        if let SystemPrompt::Text(text) = prompt {
            assert!(text.contains("Base prompt."));
            assert!(text.contains("Additional context."));
            // append_system_prompt appears after the boundary
            let boundary_pos = text
                .find(mikmik_core::system_prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY)
                .expect("boundary must exist");
            let append_pos = text.find("Additional context.").unwrap();
            assert!(
                append_pos > boundary_pos,
                "Appended text must appear after the dynamic boundary"
            );
        } else {
            panic!("Expected SystemPrompt::Text");
        }
    }

    #[test]
    fn test_system_prompt_append_only() {
        // When only append is set, default sections are present plus the
        // appended text after the dynamic boundary.
        let cfg = make_config(None, Some("Appended text."));
        let prompt = build_system_prompt(&cfg);
        if let SystemPrompt::Text(text) = prompt {
            assert!(
                text.contains("Appended text."),
                "Appended text must appear in the prompt"
            );
            let boundary_pos = text
                .find(mikmik_core::system_prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY)
                .expect("boundary must exist");
            let append_pos = text.find("Appended text.").unwrap();
            assert!(
                append_pos > boundary_pos,
                "Appended text must appear after the dynamic boundary"
            );
        } else {
            panic!("Expected SystemPrompt::Text");
        }
    }

    #[test]
    fn test_system_prompt_with_custom_output_style_prompt() {
        let mut cfg = make_config(None, None);
        cfg.output_style_prompt = Some("Answer like a pirate.".to_string());
        let prompt = build_system_prompt(&cfg);
        if let SystemPrompt::Text(text) = prompt {
            assert!(text.contains("Answer like a pirate."));
        } else {
            panic!("Expected SystemPrompt::Text");
        }
    }

    // ---- QueryConfig tests --------------------------------------------------

    #[test]
    fn test_query_config_clone() {
        let cfg = make_config(Some("test"), Some("append"));
        let cloned = cfg.clone();
        assert_eq!(cloned.model, "claude-sonnet-4-6");
        assert_eq!(cloned.max_tokens, 4096);
        assert_eq!(cloned.system_prompt, Some("test".to_string()));
    }

    // ---- QueryOutcome variant tests -----------------------------------------

    #[test]
    fn test_query_outcome_debug() {
        // Ensure the enum variants can be created and debug-formatted
        let outcome = QueryOutcome::Cancelled;
        let s = format!("{:?}", outcome);
        assert!(s.contains("Cancelled"));

        let err_outcome = QueryOutcome::Error(mikmik_core::error::ClaudeError::RateLimit);
        let s2 = format!("{:?}", err_outcome);
        assert!(s2.contains("Error"));
    }

    #[test]
    fn test_build_provider_options_for_google_gemini_3() {
        let options = build_provider_options(
            "google",
            "gemini-3-flash-preview",
            Some(mikmik_core::effort::EffortLevel::High),
            None,
            None,
        );
        assert_eq!(
            options["thinkingConfig"]["thinkingLevel"],
            serde_json::json!("high")
        );
        assert_eq!(
            options["thinkingConfig"]["includeThoughts"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn test_build_provider_options_for_openrouter_gpt5() {
        let options = build_provider_options(
            "openrouter",
            "gpt-5.4",
            Some(mikmik_core::effort::EffortLevel::Medium),
            None,
            None,
        );
        assert_eq!(options["reasoningEffort"], serde_json::json!("medium"));
        assert_eq!(options["textVerbosity"], serde_json::json!("low"));
        assert_eq!(options["usage"]["include"], serde_json::json!(true));
    }

    #[test]
    fn test_build_provider_options_codex_effort_ladder() {
        // Codex maps the lower tiers like any OpenAI reasoning model...
        for (level, expected) in [
            (mikmik_core::effort::EffortLevel::Low, "low"),
            (mikmik_core::effort::EffortLevel::Medium, "medium"),
            (mikmik_core::effort::EffortLevel::High, "high"),
        ] {
            let options =
                build_provider_options("openai-codex", "gpt-5.5", Some(level), None, None);
            assert_eq!(options["reasoningEffort"], serde_json::json!(expected));
        }
        // ...but the top "Max" tier becomes "xhigh" (extra high) on Codex.
        let options = build_provider_options(
            "openai-codex",
            "gpt-5.5",
            Some(mikmik_core::effort::EffortLevel::Max),
            None,
            None,
        );
        assert_eq!(options["reasoningEffort"], serde_json::json!("xhigh"));
        assert_eq!(options["reasoningSummary"], serde_json::json!("auto"));

        // Other OpenAI-compatible providers keep "high" for Max (no xhigh).
        let other = build_provider_options(
            "openrouter",
            "gpt-5.4",
            Some(mikmik_core::effort::EffortLevel::Max),
            None,
            None,
        );
        assert_eq!(other["reasoningEffort"], serde_json::json!("high"));
    }

    #[test]
    fn test_build_provider_options_for_bedrock_anthropic() {
        let options = build_provider_options(
            "amazon-bedrock",
            "anthropic.claude-sonnet-4-6-v1",
            Some(mikmik_core::effort::EffortLevel::High),
            Some(10_000),
            None,
        );
        assert_eq!(
            options["reasoningConfig"]["budgetTokens"],
            serde_json::json!(10_000)
        );
    }

    fn account_options(
        pairs: &[(&str, serde_json::Value)],
    ) -> std::collections::HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn an_unknown_endpoint_sends_the_options_its_account_declares() {
        // The rules in `build_provider_options` key off a fixed list of wire
        // formats, so a self-hosted OpenAI-compatible endpoint matches none of
        // them and used to send an empty body. Its account's own options are
        // the only way it can ask for reasoning at all.
        let options = build_provider_options(
            "my-gateway",
            "some-local-model",
            Some(mikmik_core::effort::EffortLevel::High),
            None,
            Some(&account_options(&[
                ("reasoningEffort", serde_json::json!("high")),
                ("service_tier", serde_json::json!("priority")),
            ])),
        );
        assert_eq!(options["reasoningEffort"], serde_json::json!("high"));
        assert_eq!(options["service_tier"], serde_json::json!("priority"));
    }

    #[test]
    fn the_live_effort_level_wins_over_the_one_written_in_settings() {
        // The other order would leave `/effort` and the picker's effort keys
        // silently inert for any account that pinned the field in its settings.
        let options = build_provider_options(
            "github-copilot",
            "gpt-5.2",
            Some(mikmik_core::effort::EffortLevel::Low),
            None,
            Some(&account_options(&[(
                "reasoningEffort",
                serde_json::json!("high"),
            )])),
        );
        assert_eq!(options["reasoningEffort"], serde_json::json!("low"));
    }

    #[test]
    fn an_account_named_after_its_owner_still_matches_its_wire_format() {
        // The call site passes the vendor id, so an account filed as "work" but
        // speaking github-copilot gets the same body as one named after the
        // vendor. Passing the account name matched no rule and sent nothing.
        let named_after_vendor = build_provider_options(
            "github-copilot",
            "claude-sonnet-4-6",
            None,
            Some(8_000),
            None,
        );
        assert_eq!(
            named_after_vendor["thinking_budget"],
            serde_json::json!(8_000)
        );
        assert_eq!(
            build_provider_options("work", "claude-sonnet-4-6", None, Some(8_000), None),
            serde_json::Value::Null
        );
    }

    #[test]
    fn an_account_without_options_sends_exactly_what_it_sent_before() {
        let empty = std::collections::HashMap::new();
        assert_eq!(
            build_provider_options(
                "openrouter",
                "gpt-5.4",
                Some(mikmik_core::effort::EffortLevel::Medium),
                None,
                Some(&empty),
            ),
            build_provider_options(
                "openrouter",
                "gpt-5.4",
                Some(mikmik_core::effort::EffortLevel::Medium),
                None,
                None,
            )
        );
        // An account with no rules and no options still sends nothing at all.
        assert_eq!(
            build_provider_options("my-gateway", "some-local-model", None, None, Some(&empty)),
            serde_json::Value::Null
        );
    }

    #[test]
    fn test_alibaba_is_openaiish_provider() {
        // "alibaba" is an alias for "qwen" (Alibaba's DashScope backend);
        // both must be treated as OpenAI-compatible providers.
        assert!(is_openaiish_provider("alibaba"));
        assert!(is_openaiish_provider("qwen"));
    }

    #[test]
    fn mlx_lm_is_openaiish_under_both_spellings() {
        // The connect dialog writes "mlxlm"; the registry canonicalises to
        // "mlx-lm". Reasoning-parameter shaping has to recognise both.
        assert!(is_openaiish_provider("mlxlm"));
        assert!(is_openaiish_provider("mlx-lm"));
    }

    // ---- apply_compact_result / #213 data-loss guard ------------------------

    fn sample_conversation() -> Vec<Message> {
        vec![
            Message::user("initial user request"),
            Message::assistant("assistant reply with important context"),
            Message::user("follow-up question"),
            Message::assistant("second assistant reply"),
        ]
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        messages.iter().map(|m| m.get_all_text()).collect()
    }

    #[test]
    fn failed_compaction_preserves_messages() {
        // Regression test for #213: a failed compaction must NOT wipe the
        // conversation. Previously the reactive path drained `messages` with
        // std::mem::take and never restored them on error.
        let mut messages = sample_conversation();
        let before = texts(&messages);

        // Simulate a failed reactive_compact / context_collapse (API error,
        // Cancelled, empty summary all map to Err here).
        let outcome: Result<compact::CompactResult, ClaudeError> = Err(ClaudeError::Cancelled);
        let result = apply_compact_result(&mut messages, outcome);

        assert!(result.is_err(), "helper must surface the compaction error");
        assert_eq!(
            messages.len(),
            before.len(),
            "messages must not be emptied on failed compaction"
        );
        assert_eq!(
            texts(&messages),
            before,
            "message contents must be identical after failed compaction"
        );
    }

    #[test]
    fn failed_compaction_with_generic_error_preserves_messages() {
        // The helper is generic over the error type; any Err leaves messages
        // untouched.
        let mut messages = sample_conversation();
        let before = texts(&messages);

        let outcome: Result<compact::CompactResult, &str> = Err("empty summary");
        let result = apply_compact_result(&mut messages, outcome);

        assert_eq!(result, Err("empty summary"));
        assert_eq!(texts(&messages), before);
    }

    #[test]
    fn successful_compaction_replaces_messages() {
        // On success the compacted result replaces the live messages and the
        // freed-token count is returned.
        let mut messages = sample_conversation();
        let compacted = vec![
            Message::user("[summary of earlier conversation]"),
            Message::user("follow-up question"),
        ];
        let expected = texts(&compacted);

        let outcome: Result<compact::CompactResult, ClaudeError> = Ok(compact::CompactResult {
            messages: compacted,
            summary: "[summary of earlier conversation]".to_string(),
            tokens_freed: 4_096,
        });
        let result = apply_compact_result(&mut messages, outcome);

        assert_eq!(
            result.unwrap(),
            4_096,
            "tokens_freed must be surfaced on success"
        );
        assert_eq!(
            texts(&messages),
            expected,
            "messages must be replaced with the compacted result on success"
        );
    }

    // ---- Central permission backstop (issue #210) ---------------------------
    //
    // These tests pin the `execute_tool` backstop contract:
    //  (a) a non-self-gating tool at a gated level is DENIED (never executes)
    //      when the handler denies;
    //  (b) a self-gating tool is NOT gated centrally (no double-prompt) — its
    //      execute() runs even though the handler would deny;
    //  (c) a ReadOnly / None tool is never gated centrally.

    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// Permission handler that denies everything (returns `Ask`, which in a
    /// non-interactive context surfaces as a hard denial).
    struct DenyAllHandler;
    impl mikmik_core::permissions::PermissionHandler for DenyAllHandler {
        fn check_permission(
            &self,
            _request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            mikmik_core::permissions::PermissionDecision::Ask {
                reason: "denied by test handler".to_string(),
            }
        }
        fn request_permission(
            &self,
            request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            self.check_permission(request)
        }
    }

    /// A configurable mock tool that records whether its `execute()` ran.
    struct MockTool {
        name: &'static str,
        level: PermissionLevel,
        self_gates: bool,
        ran: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "mock tool for backstop tests"
        }
        fn permission_level(&self) -> PermissionLevel {
            self.level
        }
        fn self_gates(&self) -> bool {
            self.self_gates
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
            self.ran.store(true, AtomicOrdering::SeqCst);
            ToolResult::success("mock ran")
        }
    }

    fn deny_all_context() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("/workspace"),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: Arc::new(DenyAllHandler),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "backstop-test".to_string(),
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

    /// (a) A tool that does NOT self-gate and requires a gated level (Execute)
    /// is blocked by the central backstop when the handler denies — and its
    /// `execute()` never runs.
    #[tokio::test]
    async fn backstop_denies_non_self_gating_gated_tool() {
        let ran = Arc::new(AtomicBool::new(false));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool {
            name: "MockExec",
            level: PermissionLevel::Execute,
            self_gates: false,
            ran: ran.clone(),
        })];
        let ctx = deny_all_context();

        let result = execute_tool("MockExec", "call-1", &serde_json::json!({}), &tools, &ctx).await;

        assert!(result.is_error, "central backstop must block a denied tool");
        assert!(
            !ran.load(AtomicOrdering::SeqCst),
            "execute() must NOT run when the backstop denies"
        );
    }

    /// (b) A self-gating tool is NOT gated by the central backstop (no double
    /// prompt): even with a deny-all handler, its `execute()` still runs
    /// because the central gate is skipped for self-gaters.
    #[tokio::test]
    async fn backstop_skips_self_gating_tool() {
        let ran = Arc::new(AtomicBool::new(false));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool {
            name: "MockSelfGated",
            level: PermissionLevel::Execute,
            self_gates: true,
            ran: ran.clone(),
        })];
        let ctx = deny_all_context();

        let result = execute_tool(
            "MockSelfGated",
            "call-1",
            &serde_json::json!({}),
            &tools,
            &ctx,
        )
        .await;

        assert!(
            !result.is_error,
            "self-gating tool must not be blocked by the central backstop"
        );
        assert_eq!(result.content, "mock ran");
        assert!(
            ran.load(AtomicOrdering::SeqCst),
            "self-gating tool's execute() must run (central gate skipped)"
        );
    }

    /// (c) ReadOnly and None tools are never gated centrally, so they run even
    /// under a deny-all handler.
    #[tokio::test]
    async fn backstop_skips_read_only_and_none_tools() {
        for level in [PermissionLevel::ReadOnly, PermissionLevel::None] {
            let ran = Arc::new(AtomicBool::new(false));
            let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool {
                name: "MockSafe",
                level,
                self_gates: false,
                ran: ran.clone(),
            })];
            let ctx = deny_all_context();

            let result =
                execute_tool("MockSafe", "call-1", &serde_json::json!({}), &tools, &ctx).await;

            assert!(
                !result.is_error,
                "{:?} tool must not be gated centrally",
                level
            );
            assert!(
                ran.load(AtomicOrdering::SeqCst),
                "{:?} tool's execute() must run",
                level
            );
        }
    }

    /// Records the call the context described while it was executing.
    struct CallRecordingTool {
        seen: Arc<parking_lot::Mutex<Option<mikmik_tools::ActiveToolCall>>>,
    }

    #[async_trait::async_trait]
    impl Tool for CallRecordingTool {
        fn name(&self) -> &str {
            "MockRecorder"
        }
        fn description(&self) -> &str {
            "records what the context said about its own call"
        }
        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::ReadOnly
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: Value, ctx: &ToolContext) -> ToolResult {
            *self.seen.lock() = ctx.current_call.as_deref().cloned();
            ToolResult::success("recorded")
        }
    }

    #[tokio::test]
    async fn a_tool_is_told_which_call_it_is_running() {
        let seen = Arc::new(parking_lot::Mutex::new(None));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(CallRecordingTool { seen: seen.clone() })];
        let ctx = deny_all_context();
        let input = serde_json::json!({"file_path": "/tmp/x"});

        let result = execute_tool("MockRecorder", "toolu_42", &input, &tools, &ctx).await;
        assert!(!result.is_error);

        let call = seen.lock().clone().expect("the tool saw its call");
        assert_eq!(call.id, "toolu_42");
        assert_eq!(call.input, input);
    }

    #[tokio::test]
    async fn the_turns_own_context_names_no_call() {
        // Only the dispatcher's per-call copy carries one; the context the turn
        // was built with must not claim to belong to some call.
        assert!(deny_all_context().current_call.is_none());
    }

    /// A tool that takes a known amount of time, so a measurement of it can be
    /// checked against something.
    struct SlowTool {
        millis: u64,
    }

    #[async_trait::async_trait]
    impl Tool for SlowTool {
        fn name(&self) -> &str {
            "MockSlow"
        }
        fn description(&self) -> &str {
            "mock tool that takes a known time"
        }
        fn permission_level(&self) -> PermissionLevel {
            // Read-only, so the backstop lets it through and the measurement is
            // of the tool alone.
            PermissionLevel::ReadOnly
        }
        fn self_gates(&self) -> bool {
            false
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
            tokio::time::sleep(std::time::Duration::from_millis(self.millis)).await;
            ToolResult::success("slept")
        }
    }

    #[tokio::test]
    async fn a_tool_reports_how_long_its_own_work_took() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(SlowTool { millis: 60 })];
        let ctx = deny_all_context();

        let result = execute_tool("MockSlow", "call-1", &serde_json::json!({}), &tools, &ctx).await;

        let measured = result.duration_ms.expect("the call was timed");
        assert!(
            measured >= 50,
            "a 60 ms tool reported {measured} ms, which is less than it slept"
        );
    }

    #[tokio::test]
    async fn a_call_the_backstop_blocks_reports_no_duration() {
        // The number means the tool's own work. Nothing ran here, so answering
        // a duration would report how long the permission check took.
        let ran = Arc::new(AtomicBool::new(false));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockTool {
            name: "MockExec",
            level: PermissionLevel::Execute,
            self_gates: false,
            ran: ran.clone(),
        })];
        let ctx = deny_all_context();

        let result = execute_tool("MockExec", "call-1", &serde_json::json!({}), &tools, &ctx).await;

        assert!(result.is_error, "the backstop must block this tool");
        assert!(
            result.duration_ms.is_none(),
            "a tool that never ran must report no duration"
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_reports_no_duration() {
        let tools: Vec<Box<dyn Tool>> = Vec::new();
        let ctx = deny_all_context();

        let result =
            execute_tool("NoSuchTool", "call-1", &serde_json::json!({}), &tools, &ctx).await;

        assert!(result.is_error);
        assert!(result.duration_ms.is_none());
    }

    #[test]
    fn backstop_permission_level_gating_matrix() {
        assert!(!permission_level_is_gated(PermissionLevel::None));
        assert!(!permission_level_is_gated(PermissionLevel::ReadOnly));
        assert!(permission_level_is_gated(PermissionLevel::Write));
        assert!(permission_level_is_gated(PermissionLevel::Execute));
        assert!(permission_level_is_gated(PermissionLevel::Dangerous));
        assert!(permission_level_is_gated(PermissionLevel::Forbidden));
    }

    // ---- Issue #218: cancellation plumbing ---------------------------------

    /// (a) The parallel tool executor (`run_tool_batch`, the exact code the query
    /// loop runs) must abandon a long-running tool the moment the cancel token
    /// fires: with a tool future that never completes and a pre-cancelled token,
    /// the batch returns promptly instead of blocking, reports cancellation, and
    /// still yields one cancelled `ToolResult` per tool so every `tool_use` can
    /// be answered and the message history stays valid.
    #[tokio::test]
    async fn executor_abandons_in_flight_tools_on_cancel() {
        use std::future::Future;
        use std::pin::Pin;

        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel(); // pre-cancelled

        // Two tool futures: one that never completes (a long-running tool) and
        // one that would succeed. Boxed so they share a concrete type.
        let never: Pin<Box<dyn Future<Output = ToolResult> + Send>> =
            Box::pin(std::future::pending());
        let quick: Pin<Box<dyn Future<Output = ToolResult> + Send>> =
            Box::pin(async { ToolResult::success("done") });

        // If the executor blocked on the never-completing tool this would time
        // out; it must return promptly instead.
        let (results, cancelled) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_tool_batch(vec![never, quick], &cancel),
        )
        .await
        .expect("executor must return promptly, not block on the pending tool");

        assert!(cancelled, "batch must report that it was cancelled");
        assert_eq!(
            results.len(),
            2,
            "every tool_use must still receive a tool_result"
        );
        assert!(
            results.iter().all(|r| r.is_error),
            "cancelled tool results are errors"
        );
        assert!(
            results[0].content.contains("cancelled"),
            "cancelled result should say so, got: {}",
            results[0].content
        );
    }

    /// The happy path is unchanged: with a live (never-cancelled) token the batch
    /// runs the futures to completion and returns their real results in order.
    #[tokio::test]
    async fn executor_runs_to_completion_without_cancel() {
        let cancel = tokio_util::sync::CancellationToken::new();
        // `std::future::ready` gives both futures the same concrete type so they
        // share a Vec (mirroring the Either-unified futures the real loop builds).
        let f1 = std::future::ready(ToolResult::success("a"));
        let f2 = std::future::ready(ToolResult::error("b"));

        let (results, cancelled) = run_tool_batch(vec![f1, f2], &cancel).await;

        assert!(!cancelled, "no cancellation should have occurred");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "a");
        assert!(!results[0].is_error);
        assert_eq!(results[1].content, "b");
        assert!(results[1].is_error);
    }

    /// (b) A sub-agent receives a CHILD of the parent's cancel token — exactly
    /// how `AgentTool` derives it from `ctx.cancel_token` — so cancelling the
    /// parent query propagates into the sub-agent. `ToolContext` now exposes the
    /// token, and cancelling it must flip the child.
    #[test]
    fn subagent_child_token_propagates_parent_cancel() {
        let ctx = deny_all_context();
        // AgentTool spawns each sub-agent with a token derived exactly this way.
        let child = ctx.cancel_token.child_token();

        assert!(!child.is_cancelled(), "child starts live");
        ctx.cancel_token.cancel();
        assert!(
            child.is_cancelled(),
            "cancelling the parent's token must cancel the sub-agent's child token"
        );
    }

    // ---- Issue #230 (MI-3): in-loop continuation + max-steps degradation -----

    use std::sync::Mutex as StdMutex;

    /// A provider double that records, per request, whether the tool set was
    /// empty (i.e. tools were disabled — the max-steps degradation turn) and
    /// replays a scripted response. Drives `run_query_loop` end-to-end.
    struct RecordingProvider {
        id: mikmik_core::provider_id::ProviderId,
        /// One entry per request: `true` when its tool set was empty.
        tools_empty_per_request: Arc<StdMutex<Vec<bool>>>,
        /// When true, always end the turn with text (ignores tools). Otherwise
        /// emit a `tool_use` while tools are present and end the turn once
        /// they're gone (so the degradation turn ends the loop).
        always_end_turn: bool,
    }

    #[async_trait::async_trait]
    impl mikmik_api::LlmProvider for RecordingProvider {
        fn id(&self) -> &mikmik_core::provider_id::ProviderId {
            &self.id
        }
        fn name(&self) -> &str {
            "recording-mock"
        }

        async fn create_message(
            &self,
            _request: mikmik_api::ProviderRequest,
        ) -> Result<mikmik_api::ProviderResponse, mikmik_api::ProviderError> {
            unimplemented!("these tests only use create_message_stream")
        }

        async fn create_message_stream(
            &self,
            request: mikmik_api::ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<mikmik_api::StreamEvent, mikmik_api::ProviderError>,
                        > + Send,
                >,
            >,
            mikmik_api::ProviderError,
        > {
            use mikmik_api::provider_types::StopReason;
            use mikmik_api::StreamEvent;

            let tools_empty = request.tools.is_empty();
            self.tools_empty_per_request
                .lock()
                .unwrap()
                .push(tools_empty);

            let msg_id = uuid::Uuid::new_v4().to_string();
            let emit_tool_use = !self.always_end_turn && !tools_empty;

            let events: Vec<Result<StreamEvent, mikmik_api::ProviderError>> = if emit_tool_use {
                let tool_id = uuid::Uuid::new_v4().to_string();
                vec![
                    Ok(StreamEvent::MessageStart {
                        id: msg_id,
                        model: "mock-model".to_string(),
                        usage: UsageInfo::default(),
                    }),
                    Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        content_block: ContentBlock::ToolUse {
                            id: tool_id,
                            name: "noop_tool".to_string(),
                            input: serde_json::json!({}),
                            thought_signature: None,
                        },
                    }),
                    Ok(StreamEvent::InputJsonDelta {
                        index: 0,
                        partial_json: "{}".to_string(),
                    }),
                    Ok(StreamEvent::MessageDelta {
                        stop_reason: Some(StopReason::ToolUse),
                        usage: Some(UsageInfo::default()),
                    }),
                    Ok(StreamEvent::MessageStop),
                ]
            } else {
                vec![
                    Ok(StreamEvent::MessageStart {
                        id: msg_id,
                        model: "mock-model".to_string(),
                        usage: UsageInfo::default(),
                    }),
                    Ok(StreamEvent::TextDelta {
                        index: 0,
                        text: "Progress summary.".to_string(),
                    }),
                    Ok(StreamEvent::MessageDelta {
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Some(UsageInfo::default()),
                    }),
                    Ok(StreamEvent::MessageStop),
                ]
            };

            Ok(Box::pin(futures::stream::iter(events)))
        }

        async fn health_check(
            &self,
        ) -> Result<mikmik_api::ProviderStatus, mikmik_api::ProviderError> {
            Ok(mikmik_api::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> mikmik_api::ProviderCapabilities {
            mikmik_api::ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: mikmik_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    fn noop_tools() -> Vec<Box<dyn Tool>> {
        vec![Box::new(MockTool {
            name: "noop_tool",
            level: PermissionLevel::ReadOnly,
            self_gates: false,
            ran: Arc::new(AtomicBool::new(false)),
        })]
    }

    /// Drive `run_query_loop` against the recording provider. Returns the
    /// outcome, the per-request "tools were empty" record, and the final
    /// message history.
    async fn drive_loop_with_mock(
        always_end_turn: bool,
        max_turns: u32,
        tools: Vec<Box<dyn Tool>>,
        continuation: crate::continuation::ContinuationMode,
    ) -> (QueryOutcome, Vec<bool>, Vec<Message>) {
        drive_loop_with_config(always_end_turn, max_turns, tools, continuation, |_| {}).await
    }

    /// As above, with a hook that adjusts the `QueryConfig` before the run.
    async fn drive_loop_with_config(
        always_end_turn: bool,
        max_turns: u32,
        tools: Vec<Box<dyn Tool>>,
        continuation: crate::continuation::ContinuationMode,
        tweak: impl FnOnce(&mut QueryConfig),
    ) -> (QueryOutcome, Vec<bool>, Vec<Message>) {
        drive_loop_with_context(
            always_end_turn,
            max_turns,
            tools,
            continuation,
            tweak,
            |_| {},
        )
        .await
    }

    /// As above, and also adjusts the session `Config` the `ToolContext` holds.
    ///
    /// Hooks live there rather than on `QueryConfig`, so a test about a hook
    /// cannot reach them through the other tweak.
    async fn drive_loop_with_context(
        always_end_turn: bool,
        max_turns: u32,
        tools: Vec<Box<dyn Tool>>,
        continuation: crate::continuation::ContinuationMode,
        tweak: impl FnOnce(&mut QueryConfig),
        tweak_ctx: impl FnOnce(&mut mikmik_core::config::Config),
    ) -> (QueryOutcome, Vec<bool>, Vec<Message>) {
        let recorded = Arc::new(StdMutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider {
            id: mikmik_core::provider_id::ProviderId::new("mockprov"),
            tools_empty_per_request: recorded.clone(),
            always_end_turn,
        });
        let mut registry = mikmik_api::ProviderRegistry::new();
        registry.register(provider);
        let registry = Arc::new(registry);

        let client = mikmik_api::AnthropicClient::new(mikmik_api::client::ClientConfig {
            api_key: "test-key".to_string(),
            ..Default::default()
        })
        .expect("build test client");

        let mut ctx = deny_all_context();
        ctx.session_id = "loop-test".to_string();
        ctx.config.provider = Some("mockprov".to_string());
        tweak_ctx(&mut ctx.config);

        let mut config = make_config(None, None);
        config.model = "mock-model".to_string();
        config.max_turns = max_turns;
        config.provider_registry = Some(registry);
        config.continuation = continuation;
        tweak(&mut config);

        let cost = mikmik_core::cost::CostTracker::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut messages = vec![Message::user("start")];

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_query_loop(
                &client,
                &mut messages,
                &tools,
                &ctx,
                &config,
                cost,
                None,
                cancel,
                None,
            ),
        )
        .await
        .expect("loop must not hang");

        let recorded = recorded.lock().unwrap().clone();
        (outcome, recorded, messages)
    }

    /// A `PostModelTurn` hook with the given command, declared as the session's
    /// only hook.
    fn post_model_turn_hook(
        command: &str,
    ) -> impl FnOnce(&mut mikmik_core::config::Config) + use<'_> {
        move |config: &mut mikmik_core::config::Config| {
            config.hooks.insert(
                mikmik_core::config::HookEvent::PostModelTurn,
                vec![mikmik_core::config::HookEntry {
                    command: command.to_string(),
                    ..Default::default()
                }],
            );
        }
    }

    /// The `PostModelTurn` hook used to fire only in the Anthropic arm, so a
    /// user's hook was silently skipped on every other account. This drives the
    /// provider arm and asserts the hook's message reached the conversation.
    #[tokio::test]
    async fn a_post_model_turn_hook_fires_on_the_provider_arm() {
        let (outcome, _recorded, messages) = drive_loop_with_context(
            true,
            3,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
            |_| {},
            post_model_turn_hook("echo HOOK-SAW-THE-TURN >&2; exit 1"),
        )
        .await;

        assert!(
            matches!(outcome, QueryOutcome::EndTurn { .. }),
            "exit 1 injects a message; it does not veto the turn"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.get_all_text().contains("HOOK-SAW-THE-TURN")),
            "the hook's output must reach the conversation: {:?}",
            messages
                .iter()
                .map(|m| m.get_all_text())
                .collect::<Vec<_>>()
        );
    }

    /// Exit above 1 is a veto: the loop returns rather than sending another
    /// request. The recording provider counts requests, so one is the proof.
    #[tokio::test]
    async fn a_vetoing_hook_ends_the_turn_on_the_provider_arm() {
        let (outcome, recorded, messages) = drive_loop_with_context(
            false,
            5,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
            |_| {},
            post_model_turn_hook("echo HOOK-VETO >&2; exit 2"),
        )
        .await;

        assert!(matches!(outcome, QueryOutcome::EndTurn { .. }));
        assert_eq!(
            recorded.len(),
            1,
            "the veto must stop the loop before a second request"
        );
        assert!(messages
            .iter()
            .any(|m| m.get_all_text().contains("HOOK-VETO")));
    }

    /// (a) A non-goal turn that ends with `end_turn` stops after exactly one
    /// turn — the default `StopPolicy` never continues the loop.
    #[tokio::test]
    async fn non_goal_turn_stops_after_one_turn() {
        let (outcome, recorded, _msgs) = drive_loop_with_mock(
            true,
            5,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
        )
        .await;

        assert!(
            matches!(outcome, QueryOutcome::EndTurn { .. }),
            "a completed turn must yield EndTurn"
        );
        assert_eq!(
            recorded.len(),
            1,
            "a non-goal end_turn must stop after exactly one request/turn, got {:?}",
            recorded
        );
    }

    /// (c) Hitting `effective_max_turns` runs ONE final turn with tools disabled
    /// (graceful degradation) rather than returning cold: the last request has
    /// an empty tool set and the loop then ends.
    #[tokio::test]
    async fn max_steps_runs_tool_less_summary_turn_then_ends() {
        // max_turns = 2: turns 1 & 2 are tool_use turns, turn 3 exceeds the cap
        // and triggers the tool-less summary turn.
        let (outcome, recorded, msgs) = drive_loop_with_mock(
            false,
            2,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
        )
        .await;

        assert!(
            matches!(outcome, QueryOutcome::EndTurn { .. }),
            "the loop must end after the degradation summary turn"
        );
        assert_eq!(
            recorded.len(),
            3,
            "expected 2 tool turns + 1 degradation turn, got {:?}",
            recorded
        );
        assert!(
            *recorded.last().unwrap(),
            "the final (summary) turn must be dispatched with tools DISABLED: {:?}",
            recorded
        );
        assert!(
            recorded[..recorded.len() - 1].iter().all(|&empty| !empty),
            "only the degradation turn disables tools: {:?}",
            recorded
        );
        assert!(
            msgs.iter()
                .any(|m| m.get_all_text().contains("maximum number of steps")),
            "the tool-less summary prompt must be injected into the history"
        );
    }

    /// With the summary turn switched off, the same run stops at the limit and
    /// hands back what the model last said.
    #[tokio::test]
    async fn max_steps_returns_the_last_message_when_the_summary_turn_is_off() {
        let (outcome, recorded, msgs) = drive_loop_with_config(
            false,
            2,
            noop_tools(),
            crate::continuation::ContinuationMode::Default,
            |config| config.degradation_summary = false,
        )
        .await;

        assert!(
            matches!(outcome, QueryOutcome::EndTurn { .. }),
            "the loop must still end cleanly"
        );
        assert_eq!(
            recorded.len(),
            2,
            "the extra summary request must not be sent, got {:?}",
            recorded
        );
        assert!(
            !msgs
                .iter()
                .any(|m| m.get_all_text().contains("maximum number of steps")),
            "the summary prompt must not reach the history"
        );
    }

    /// (b) The goal continuation guards, exercised against an in-memory store:
    /// an active goal within its guards continues (recording the turn), while
    /// the soft-budget and runaway guards each stop with the same paused
    /// outcome as before.
    #[test]
    fn goal_policy_continues_while_active_and_stops_on_guards() {
        use crate::goal_loop::{decide_goal_continuation, GoalContinuation, StopReason};

        let store =
            mikmik_core::GoalStore::open(std::path::Path::new(":memory:")).expect("open store");

        // Active goal, guards allow → continue with the goal continuation message.
        store.set_goal("live", "ship the feature", None).unwrap();
        match decide_goal_continuation(&store, "live", 0, 1) {
            GoalContinuation::Continue { message } => {
                assert!(
                    message.contains("Goal continuation"),
                    "unexpected continuation message: {}",
                    message
                );
            }
            _ => panic!("an active goal within its guards must continue"),
        }
        // The turn was recorded in the store.
        assert_eq!(store.get_goal("live").unwrap().turns_used, 1);

        // Soft token budget tripped → budget-limited (paused) outcome.
        store.set_goal("budget", "big task", Some(100)).unwrap();
        match decide_goal_continuation(&store, "budget", 500, 1) {
            GoalContinuation::Stop {
                reason: StopReason::BudgetLimited,
            } => {}
            _ => panic!("an over-budget goal must stop budget-limited"),
        }
        assert_eq!(
            store.get_goal("budget").unwrap().status,
            mikmik_core::GoalStatus::BudgetLimited,
            "over-budget goal must be persisted as budget-limited"
        );

        // Runaway guard tripped → paused outcome (same as the cross-turn design).
        store.set_goal("runaway", "endless", None).unwrap();
        for _ in 0..mikmik_core::MAX_GOAL_TURNS {
            store.record_turn("runaway", 0).unwrap();
        }
        match decide_goal_continuation(&store, "runaway", 0, 1) {
            GoalContinuation::Stop {
                reason: StopReason::RunawayGuard { turns_used },
            } => {
                assert_eq!(turns_used, mikmik_core::MAX_GOAL_TURNS);
            }
            _ => panic!("a runaway goal must pause"),
        }
        assert_eq!(
            store.get_goal("runaway").unwrap().status,
            mikmik_core::GoalStatus::Paused,
            "runaway goal must be persisted as paused"
        );
    }

    // ---- ultracode activation (effort) ----------------------------------

    #[test]
    fn ultracode_keyword_raises_effort_to_ultracode() {
        use mikmik_core::effort::EffortLevel;
        let msgs = vec![Message::user("please ultracode this refactor")];
        // Even with no configured effort, the keyword forces Ultracode.
        assert_eq!(
            effective_effort_for_turn(None, &msgs),
            Some(EffortLevel::Ultracode)
        );
        // ...and it overrides a lower configured effort for the turn.
        assert_eq!(
            effective_effort_for_turn(Some(EffortLevel::Low), &msgs),
            Some(EffortLevel::Ultracode)
        );
    }

    #[test]
    fn no_keyword_keeps_configured_effort() {
        use mikmik_core::effort::EffortLevel;
        let msgs = vec![Message::user("please refactor this module")];
        assert_eq!(effective_effort_for_turn(None, &msgs), None);
        assert_eq!(
            effective_effort_for_turn(Some(EffortLevel::High), &msgs),
            Some(EffortLevel::High)
        );
    }

    #[test]
    fn ultracode_effort_checks_only_the_last_user_message() {
        // Keyword in an earlier turn does not keep ultracode active on a later
        // plain turn.
        let msgs = vec![
            Message::user("ultracode kick things off"),
            Message::assistant("working on it"),
            Message::user("now just tidy up the docs"),
        ];
        assert_eq!(effective_effort_for_turn(None, &msgs), None);
    }

    #[test]
    fn ultracode_addendum_flows_into_built_system_prompt() {
        use mikmik_core::effort::EffortLevel;
        // Mirrors the loop wiring: when the effective effort is Ultracode the
        // procedure addendum is threaded through `append_system_prompt` into the
        // assembled system prompt.
        let msgs = vec![Message::user("ultracode audit the query loop")];
        assert_eq!(
            effective_effort_for_turn(None, &msgs),
            Some(EffortLevel::Ultracode)
        );
        let addendum = mikmik_core::effort::ultracode_system_prompt_addendum();
        let opts = mikmik_core::system_prompt::SystemPromptOptions {
            append_system_prompt: Some(addendum),
            skip_env_info: true,
            ..Default::default()
        };
        let prompt = mikmik_core::system_prompt::build_system_prompt(&opts);
        assert!(prompt.contains("Ultracode Mode"));
        assert!(prompt.contains("TeamCreate"));

        // Absent path: no keyword -> configured effort stays, no ultracode text.
        assert_eq!(
            effective_effort_for_turn(None, &[Message::user("hi there")]),
            None
        );
        let plain = mikmik_core::system_prompt::build_system_prompt(
            &mikmik_core::system_prompt::SystemPromptOptions {
                skip_env_info: true,
                ..Default::default()
            },
        );
        assert!(!plain.contains("Ultracode Mode"));
    }

    // ---- persona output-style (transient vs persistent) ------------------

    #[test]
    fn inline_persona_keyword_applies_transiently_for_the_turn() {
        // No persisted persona; an inline `rocky` selects the rocky prompt for
        // this turn only.
        let cfg = QueryConfig::default();
        let msgs = vec![Message::user("please rocky explain this borrow error")];
        let (_style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        let prompt = prompt.expect("inline rocky should resolve a persona prompt");
        assert!(prompt.contains("Project Hail Mary"));

        // Caveman likewise.
        let msgs = vec![Message::user("caveman summarize the diff")];
        let (_s, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert!(prompt.unwrap().contains("UNCHANGED"));
    }

    #[test]
    fn persona_only_checks_the_last_user_message() {
        // A persona keyword in an earlier turn does not linger onto a later
        // plain turn (transient, like ultracode).
        let cfg = QueryConfig::default();
        let msgs = vec![
            Message::user("rocky kick things off"),
            Message::assistant("good good good"),
            Message::user("now just tidy the docs"),
        ];
        let (_style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert!(
            prompt.is_none(),
            "persona should not persist to a plain turn"
        );
    }

    #[test]
    fn persisted_persona_stands_without_an_inline_keyword() {
        // A persona chosen via /rocky or /output-style lives in the config and
        // persists across plain turns.
        let cfg = QueryConfig {
            output_style_prompt: Some("PERSISTED PERSONA".to_string()),
            ..QueryConfig::default()
        };
        let msgs = vec![Message::user("just a plain request here please")];
        let (_style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert_eq!(prompt.as_deref(), Some("PERSISTED PERSONA"));
    }

    #[test]
    fn inline_normal_resets_a_persisted_persona_for_the_turn() {
        // With a persona persisted, an inline `normal` clears it for this turn.
        let cfg = QueryConfig {
            output_style_prompt: Some("PERSISTED PERSONA".to_string()),
            ..QueryConfig::default()
        };
        let msgs = vec![Message::user("back to normal for this one please")];
        let (style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert!(prompt.is_none(), "inline normal should reset the persona");
        assert_eq!(style, mikmik_core::system_prompt::OutputStyle::Default);
    }

    #[test]
    fn inline_persona_overrides_a_different_persisted_persona() {
        // Persisted caveman, but this turn asks for rocky inline → rocky wins
        // transiently.
        let cfg = QueryConfig {
            output_style_prompt: Some(
                mikmik_core::output_styles::OutputStyleDef::builtin_caveman().prompt,
            ),
            ..QueryConfig::default()
        };
        let msgs = vec![Message::user("rocky, review this function")];
        let (_style, prompt) = effective_output_style_for_turn(&cfg, &msgs);
        assert!(prompt.unwrap().contains("Project Hail Mary"));
    }
}

#[cfg(test)]
mod conditional_rule_tests {
    use super::*;
    use crate::runner::{check_rules, RuleOutcome};
    use mikmik_tools::ToolContext;

    /// `MIKMIK_HOME` is process-global, so these run one at a time.
    ///
    /// Async-aware, because each test holds it across an await.
    static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Points the config root at a temporary directory, so a rule file the
    /// developer keeps in their real `rules/` directory cannot reach the test.
    struct HomeGuard {
        saved: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl HomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let saved = std::env::var_os("MIKMIK_HOME");
            std::env::set_var("MIKMIK_HOME", dir.path());
            Self { saved, _dir: dir }
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

    struct AllowAll;

    impl mikmik_core::permissions::PermissionHandler for AllowAll {
        fn check_permission(
            &self,
            _request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            mikmik_core::permissions::PermissionDecision::Allow
        }
        fn request_permission(
            &self,
            request: &mikmik_core::permissions::PermissionRequest,
        ) -> mikmik_core::permissions::PermissionDecision {
            self.check_permission(request)
        }
    }

    fn context_in(dir: &std::path::Path, session: &str) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: Arc::new(AllowAll),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: session.to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
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

    fn write_rule(dir: &std::path::Path, name: &str, body: &str) {
        let path = dir.join(".mikmik/rules").join(format!("{name}.md"));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }

    fn writing_unwrap() -> serde_json::Value {
        serde_json::json!({
            "file_path": "src/a.rs",
            "old_string": "let x = 1;",
            "new_string": "let x = y.unwrap();"
        })
    }

    #[tokio::test]
    async fn a_matching_rule_rides_on_the_result() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().expect("tempdir");
        write_rule(
            project.path(),
            "no-unwrap",
            "---\ncondition: \"\\\\.unwrap\\\\(\\\\)\"\nscope: \"tool:Edit(*.rs)\"\n---\nNO-UNWRAP-BODY\n",
        );
        mikmik_core::rules::reload();

        let ctx = context_in(project.path(), "rules-remind");
        mikmik_core::rules::forget_session(&ctx.session_id);
        match check_rules(&ctx, "Edit", &writing_unwrap()).await {
            RuleOutcome::Remind(text) => {
                assert!(text.contains("NO-UNWRAP-BODY"), "{text}");
                assert!(text.contains("rule=\"no-unwrap\""), "{text}");
            }
            other => panic!("expected a reminder, got {other:?}"),
        }

        // `repeat` defaults to once, so the same rule stays quiet after that.
        assert!(matches!(
            check_rules(&ctx, "Edit", &writing_unwrap()).await,
            RuleOutcome::Silent
        ));
        mikmik_core::rules::forget_session(&ctx.session_id);
        mikmik_core::rules::reload();
    }

    #[tokio::test]
    async fn a_blocking_rule_answers_instead_of_the_tool() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().expect("tempdir");
        write_rule(
            project.path(),
            "no-unwrap",
            "---\ncondition: \"\\\\.unwrap\\\\(\\\\)\"\non_match: block\n---\nREFUSED-BODY\n",
        );
        mikmik_core::rules::reload();

        let ctx = context_in(project.path(), "rules-block");
        mikmik_core::rules::forget_session(&ctx.session_id);
        match check_rules(&ctx, "Edit", &writing_unwrap()).await {
            RuleOutcome::Block(result) => {
                assert!(result.is_error);
                assert!(
                    result.content.contains("REFUSED-BODY"),
                    "{}",
                    result.content
                );
            }
            other => panic!("expected a block, got {other:?}"),
        }
        mikmik_core::rules::forget_session(&ctx.session_id);
        mikmik_core::rules::reload();
    }

    #[tokio::test]
    async fn the_switch_silences_every_rule() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().expect("tempdir");
        write_rule(
            project.path(),
            "no-unwrap",
            "---\ncondition: \"\\\\.unwrap\\\\(\\\\)\"\n---\nBODY\n",
        );
        mikmik_core::rules::reload();

        let mut ctx = context_in(project.path(), "rules-off");
        ctx.config.rules_enabled = Some(false);
        mikmik_core::rules::forget_session(&ctx.session_id);
        assert!(matches!(
            check_rules(&ctx, "Edit", &writing_unwrap()).await,
            RuleOutcome::Silent
        ));
        mikmik_core::rules::forget_session(&ctx.session_id);
        mikmik_core::rules::reload();
    }

    /// Longer than one check step, so a single push reaches the scan.
    const ONE_STEP: &str =
        "and here is a long enough sentence to reach the next scan of what was written";

    #[tokio::test]
    async fn a_rule_on_prose_stops_the_turn_and_says_why() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().expect("tempdir");
        write_rule(
            project.path(),
            "no-hedging",
            "---\ncondition: \"probably fine\"\nscope: text\n---\nSAY-WHAT-YOU-MEAN\n",
        );
        mikmik_core::rules::reload();

        let ctx = context_in(project.path(), "prose-hit");
        mikmik_core::rules::forget_session(&ctx.session_id);
        let mut watch = crate::runner::ProseWatch::new(&ctx);
        assert!(!watch.is_idle(), "a rule on text is being watched");

        watch.start_turn();
        assert!(
            !watch.push(ONE_STEP, mikmik_core::rules::ProseStream::Text),
            "innocent text runs on"
        );
        assert!(
            watch.push(
                &format!("that is probably fine.{ONE_STEP}"),
                mikmik_core::rules::ProseStream::Text
            ),
            "the forbidden phrase stops the turn"
        );

        let message = watch.take_message().await.expect("the rule speaks");
        let body = format!("{:?}", message.content);
        assert!(body.contains("SAY-WHAT-YOU-MEAN"), "{body}");

        // `repeat` defaults to once, so the next turn runs clean.
        watch.start_turn();
        assert!(!watch.push(
            &format!("that is probably fine.{ONE_STEP}"),
            mikmik_core::rules::ProseStream::Text
        ));
        assert!(watch.take_message().await.is_none());

        mikmik_core::rules::forget_session(&ctx.session_id);
        mikmik_core::rules::reload();
    }

    #[tokio::test]
    async fn a_rule_on_tools_leaves_the_writing_alone() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().expect("tempdir");
        write_rule(
            project.path(),
            "no-unwrap",
            "---\ncondition: \"\\\\.unwrap\\\\(\\\\)\"\nscope: \"tool:Edit\"\n---\nBODY\n",
        );
        mikmik_core::rules::reload();

        let ctx = context_in(project.path(), "prose-idle");
        mikmik_core::rules::forget_session(&ctx.session_id);
        let mut watch = crate::runner::ProseWatch::new(&ctx);
        assert!(watch.is_idle(), "no rule watches prose, so nothing is read");
        assert!(!watch.push(
            &format!("here we call y.unwrap() on it.{ONE_STEP}"),
            mikmik_core::rules::ProseStream::Text
        ));
        assert!(watch.take_message().await.is_none());

        mikmik_core::rules::forget_session(&ctx.session_id);
        mikmik_core::rules::reload();
    }

    #[tokio::test]
    async fn a_rule_that_repeats_still_cannot_hold_the_query() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().expect("tempdir");
        write_rule(
            project.path(),
            "no-hedging",
            "---\ncondition: \"probably fine\"\nscope: text\nrepeat: always\n---\nBODY\n",
        );
        mikmik_core::rules::reload();

        let ctx = context_in(project.path(), "prose-budget");
        mikmik_core::rules::forget_session(&ctx.session_id);
        let mut watch = crate::runner::ProseWatch::new(&ctx);

        let mut stops = 0;
        for _ in 0..6 {
            watch.start_turn();
            if watch.push(
                &format!("that is probably fine.{ONE_STEP}"),
                mikmik_core::rules::ProseStream::Text,
            ) {
                stops += 1;
                let _ = watch.take_message().await;
            }
        }
        assert_eq!(
            stops, 3,
            "the budget runs out and the turn is allowed to end"
        );

        mikmik_core::rules::forget_session(&ctx.session_id);
        mikmik_core::rules::reload();
    }
}
