// AgentTool: spawn a sub-agent to handle a complex sub-task.
//
// Lives in cc-query (not cc-tools) to avoid a circular dependency:
//   cc-tools would need cc-query, but cc-query already needs cc-tools.
//
// The AgentTool creates a nested query loop with its own context, enabling
// the model to delegate complex work to specialized sub-agents. Each sub-agent:
//   - Runs its own agentic loop
//   - Has access to all tools (except AgentTool itself, preventing infinite recursion)
//   - Returns its final output as the tool result
//
// New capabilities (TS parity):
//   - `isolation: "worktree"` — run the agent in a dedicated git worktree so
//     file edits don't conflict with the parent checkout or sibling agents.
//   - `run_in_background: true` — fire-and-forget; returns agent_id immediately.
//     Use the `monitor` tool to check completion status/output.

use async_trait::async_trait;
use dashmap::DashMap;
use mikmik_api::client::ClientConfig;
use mikmik_api::{AnthropicClient, ModelRegistry, ProviderRegistry};
use mikmik_core::types::Message;
use mikmik_tools::{PermissionLevel, Tool, ToolContext, ToolResult};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info, warn};

use crate::{run_query_loop, QueryConfig, QueryOutcome};

// ---------------------------------------------------------------------------
// Worktree isolation helpers
// ---------------------------------------------------------------------------

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

async fn create_worktree(git_root: &Path, agent_id: &str) -> Option<PathBuf> {
    let worktree_dir = std::env::temp_dir().join(format!("claude-agent-{}", agent_id));
    let output = tokio::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            worktree_dir.to_str().unwrap_or_default(),
            "HEAD",
        ])
        .current_dir(git_root)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(worktree_dir)
    } else {
        warn!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        None
    }
}

async fn remove_worktree(git_root: &Path, worktree_dir: &Path) {
    let _ = tokio::process::Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            worktree_dir.to_str().unwrap_or_default(),
        ])
        .current_dir(git_root)
        .output()
        .await;
}

// ---------------------------------------------------------------------------
// AgentTool
// ---------------------------------------------------------------------------

pub struct AgentTool;

fn build_model_registry() -> ModelRegistry {
    let mut registry = ModelRegistry::new();
    if let Some(cache_dir) = dirs::cache_dir() {
        let cache_path = cache_dir.join("mikmik").join("models_dev.json");
        registry.load_cache(&cache_path);
    }
    registry
}

/// The context a sub-agent runs under.
///
/// A sub-agent has no plan mode of its own, and the approval dialog it could
/// open belongs to the session the user is watching: answering it would move
/// the main session's permission mode on a plan the user never asked for. So
/// the plan approval channel does not come along, which leaves `ExitPlanMode`
/// on its non-blocking path there.
fn subagent_context(parent: &ToolContext, inbox: mikmik_tools::AgentAddress) -> ToolContext {
    let mut ctx = parent.clone();
    ctx.plan_approval_tx = None;
    // A sub-agent shares its parent's session id, so it needs an address of
    // its own or the two would drain the same inbox.
    ctx.inbox = inbox;
    ctx
}

/// How many executors a session may run at once, one semaphore per session.
///
/// Per session rather than per process, because the ACP server holds a
/// `SessionRegistry` and a limit set in one session must not throttle another.
static EXECUTOR_SLOTS: Lazy<DashMap<String, Arc<Semaphore>>> = Lazy::new(DashMap::new);

/// A permit to run as one of a session's concurrent executors.
///
/// `None` unless managed mode is on: with no configured limit there is nothing
/// to bound, and taking a permit would only add a wait to today's behaviour.
///
/// Only an agent that runs *beside* its parent takes one. A foreground
/// sub-agent blocks its parent for its whole run, so it is already alone.
/// Nothing that holds a permit can ask for a second one, because a sub-agent
/// receives no tool that spawns agents; that is what makes this deadlock-free.
///
/// The limit is read when a session first spawns an executor. Changing
/// `concurrent` afterwards reaches the next session, not this one.
async fn executor_permit(ctx: &ToolContext) -> Option<OwnedSemaphorePermit> {
    let limit = ctx
        .managed_agent_config
        .as_ref()
        .filter(|managed| managed.enabled)
        .map(|managed| managed.max_concurrent_executors.max(1) as usize)?;

    // Clone the Arc out from under the shard guard, then await on it: never
    // hold a DashMap lock across an `.await`.
    let slots = EXECUTOR_SLOTS
        .entry(ctx.session_id.clone())
        .or_insert_with(|| Arc::new(Semaphore::new(limit)))
        .clone();

    slots.acquire_owned().await.ok()
}

/// Tools that reach the agent runner.
///
/// Excluding `Agent` alone does not stop a sub-agent from spawning: a team
/// runs through the same `AGENT_RUNNER`, so a sub-agent holding `TeamCreate`
/// opens the door that excluding `Agent` was meant to close, and nothing
/// bounds how deep that goes.
const SPAWNING_TOOLS: &[&str] = &[
    mikmik_core::constants::TOOL_NAME_AGENT,
    mikmik_core::constants::TOOL_NAME_TEAM_CREATE,
];

/// The tools a sub-agent may hold.
///
/// The spawn filter comes first and an allowlist narrows what is left, so a
/// model that asks for a spawning tool by name does not receive one.
fn subagent_tools(allowed: Option<&Vec<String>>) -> Vec<Box<dyn Tool>> {
    mikmik_tools::all_tools()
        .into_iter()
        .filter(|t| !SPAWNING_TOOLS.contains(&t.name()))
        .filter(|t| match allowed {
            Some(names) => names.contains(&t.name().to_string()),
            None => true,
        })
        .collect()
}

/// The model string the subagent's own `QueryConfig` will carry.
///
/// Canonical, because the subagent resolves it against its own config and a
/// bare id there lands on whatever provider that config happens to hold.
///
/// The old rule left any id containing a slash alone, which is right for
/// `"myaccount/haiku"` and wrong for `"meta-llama/Llama-3.3-70B"`: that is one
/// OpenRouter model id, and leaving it unqualified sent the subagent to
/// whichever account was active instead.
fn resolve_subagent_model(params: &AgentInput, ctx: &ToolContext) -> String {
    let chosen = params.model.clone().filter(|m| !m.is_empty()).or_else(|| {
        ctx.managed_agent_config
            .as_ref()
            .map(|c| c.executor_model.clone())
            .filter(|m| !m.is_empty())
    });
    subagent_model_for(&ctx.config, chosen.as_deref())
}

/// Split out from [`resolve_subagent_model`] so it can be tested against a
/// bare `Config`; building a whole `ToolContext` proves nothing about routing.
fn subagent_model_for(config: &mikmik_core::Config, chosen: Option<&str>) -> String {
    let route = match chosen {
        Some(model) => config.resolve_route(model),
        // No override: whatever the parent session resolved to, fallbacks and
        // all. `effective_route` is not `resolve_route(effective_model())`
        // precisely because those fallbacks carry vendor namespaces.
        None => config.effective_route(),
    };

    config.canonical_model(&route.account, &route.model)
}

/// How a finished sub-agent looks to the task registry.
///
/// Classified from the outcome rather than from the text `format_outcome`
/// produced, because a message's wording is not a contract.
fn task_status_for(outcome: &QueryOutcome) -> mikmik_core::tasks::TaskStatus {
    use mikmik_core::tasks::TaskStatus;
    match outcome {
        QueryOutcome::EndTurn { .. } | QueryOutcome::MaxTokens { .. } => TaskStatus::Completed,
        QueryOutcome::Cancelled => TaskStatus::Cancelled,
        QueryOutcome::Error(err) => TaskStatus::Failed(err.to_string()),
        QueryOutcome::BudgetExceeded {
            cost_usd,
            limit_usd,
        } => TaskStatus::Failed(format!(
            "budget limit ${:.4} exceeded (spent ${:.4})",
            limit_usd, cost_usd
        )),
    }
}

/// One word for how a run ended, for a hook payload.
pub(crate) fn outcome_label(outcome: &QueryOutcome) -> &'static str {
    match outcome {
        QueryOutcome::EndTurn { .. } => "end_turn",
        QueryOutcome::MaxTokens { .. } => "max_tokens",
        QueryOutcome::Cancelled => "cancelled",
        QueryOutcome::Error(_) => "error",
        QueryOutcome::BudgetExceeded { .. } => "budget_exceeded",
    }
}

/// Render the `*.md` agent definitions found in `dirs` as prompt sections.
///
/// Returns an empty string when no directory holds one, which is the signal
/// the caller uses to leave the sub-agent's prompt untouched.
fn plugin_agent_definitions(dirs: &[PathBuf]) -> String {
    let mut defs = String::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::debug!(dir = %dir.display(), error = %err, "plugin agents: read_dir failed");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("agent");
                        defs.push_str(&format!("\n\n## Agent: {}\n{}", name, content.trim()));
                    }
                    Err(err) => {
                        tracing::debug!(path = %path.display(), error = %err, "plugin agents: read failed");
                    }
                }
            }
        }
    }
    defs
}

#[derive(Debug, Deserialize)]
struct AgentInput {
    /// Short description of the agent's task (used for logging).
    description: String,
    /// Optional: the name other agents address this one by with `SendMessage`.
    /// Derived from `description` when absent.
    #[serde(default)]
    name: Option<String>,
    /// The complete task prompt to send as the first user message.
    prompt: String,
    /// Optional: which tools to make available (defaults to all minus AgentTool).
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Optional: system prompt override for the sub-agent.
    #[serde(default)]
    system_prompt: Option<String>,
    /// Optional: max turns for the sub-agent (default 10).
    #[serde(default)]
    max_turns: Option<u32>,
    /// Optional: model override for this sub-agent.
    #[serde(default)]
    model: Option<String>,
    /// Optional: reasoning effort for this sub-agent (low, medium, high, ...).
    #[serde(default)]
    effort: Option<String>,
    /// Optional: shared preamble prepended to `prompt`, so several agents can
    /// be given the same background without repeating it in each prompt.
    #[serde(default)]
    context: Option<String>,
    /// Set to "worktree" to run the agent in an isolated git worktree.
    /// Omit (or set to null) for shared working directory.
    #[serde(default)]
    isolation: Option<String>,
    /// If true, start the agent in the background and return agent_id immediately.
    /// Default: false (wait for completion).
    #[serde(default)]
    run_in_background: bool,
}

/// A batch spawn: one shared `context` and a list of task inputs.
///
/// Each task is a raw single-agent input object; it is validated and turned
/// into an `AgentInput` by the same `execute` path, so the batch never
/// duplicates the spawn logic.
#[derive(Debug, Deserialize)]
struct BatchInput {
    /// Prepended to every task's prompt, so the agents share one background.
    #[serde(default)]
    context: Option<String>,
    /// The tasks to spawn, each a single-agent input object.
    tasks: Vec<Value>,
}

/// Put the shared `context` above a task's prompt, when one was given.
fn with_context(context: Option<&str>, prompt: &str) -> String {
    match context.map(str::trim).filter(|c| !c.is_empty()) {
        Some(context) => format!("{context}\n\n{prompt}"),
        None => prompt.to_string(),
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_AGENT
    }

    fn description(&self) -> &str {
        "Launch a new agent to handle complex, multi-step tasks autonomously. \
         The agent runs its own agentic loop with access to tools and returns \
         its final result. Use this to delegate sub-tasks, run parallel \
         workstreams, or handle tasks that require many tool calls."
    }

    fn permission_level(&self) -> PermissionLevel {
        // The agent inherits parent permissions; no extra level required.
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short description of the agent's task (3-5 words)"
                },
                "name": {
                    "type": "string",
                    "description": "The name this agent answers to when another agent sends it a \
                                    message. Defaults to a name derived from the description. A \
                                    name already taken in this session gets a numeric suffix, and \
                                    the name actually assigned comes back in the result."
                },
                "prompt": {
                    "type": "string",
                    "description": "The complete task for the agent to perform"
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of tool names to make available. Defaults to all tools."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Optional system prompt override for the sub-agent"
                },
                "max_turns": {
                    "type": "number",
                    "description": "Maximum number of turns for the sub-agent (default 10)"
                },
                "model": {
                    "type": "string",
                    "description": "Optional model to use for this agent"
                },
                "effort": {
                    "type": "string",
                    "description": "Optional reasoning effort for this agent: low, medium, high, xhigh, max."
                },
                "context": {
                    "type": "string",
                    "description": "Optional shared preamble prepended to the prompt. With batch tasks, it is prepended to every task's prompt."
                },
                "tasks": {
                    "type": "array",
                    "description": "Spawn several agents at once. Each item is a task with its own description, prompt, and optional name, tools, model, effort and isolation. The top-level context, when given, is prepended to every task's prompt. Names must not collide within the batch.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": { "type": "string" },
                            "name": { "type": "string" },
                            "prompt": { "type": "string" },
                            "tools": { "type": "array", "items": { "type": "string" } },
                            "system_prompt": { "type": "string" },
                            "max_turns": { "type": "number" },
                            "model": { "type": "string" },
                            "effort": { "type": "string" },
                            "isolation": { "type": "string", "enum": ["worktree"] }
                        },
                        "required": ["description", "prompt"]
                    }
                },
                "isolation": {
                    "type": "string",
                    "enum": ["worktree"],
                    "description": "Set to \"worktree\" to run the agent in an isolated git worktree. \
                                    Prevents file-edit conflicts when multiple agents run in parallel."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "If true, the agent starts immediately and this call returns an \
                                    agent_id without waiting for completion. Use the monitor tool \
                                    with action=status/output and task_id=agent_id. Default: false."
                }
            },
            "required": ["description", "prompt"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        // A batch call carries `tasks`; each task is spawned as its own single
        // agent, so the whole spawn path below is reused rather than copied.
        if input.get("tasks").is_some() {
            return self.spawn_batch(input, ctx).await;
        }

        let params: AgentInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // The reasoning effort this agent runs at, if one was named. An
        // unknown word is refused rather than silently ignored.
        let effort_level = match params.effort.as_deref() {
            Some(word) => match mikmik_core::effort::EffortLevel::from_str(word) {
                Some(level) => Some(level),
                None => {
                    return ToolResult::error(format!(
                        "unknown effort {word:?}; use low, medium, high, xhigh or max"
                    ))
                }
            },
            None => None,
        };
        // The shared context sits above the prompt, so a batch's agents can
        // carry the same background without repeating it.
        let effective_prompt = with_context(params.context.as_deref(), &params.prompt);

        info!(description = %params.description, "Spawning sub-agent");

        let anthropic_key = ctx.config.resolve_anthropic_api_key().unwrap_or_default();
        let anthropic_base = ctx.config.resolve_anthropic_api_base();
        let client = match AnthropicClient::new(ClientConfig {
            api_key: anthropic_key.clone(),
            api_base: anthropic_base,
            ..Default::default()
        }) {
            Ok(c) => Arc::new(c),
            Err(e) => return ToolResult::error(format!("Failed to create client: {}", e)),
        };

        let provider_registry = ProviderRegistry::from_config(
            &ctx.config,
            ClientConfig {
                api_key: anthropic_key,
                api_base: ctx.config.resolve_anthropic_api_base(),
                ..Default::default()
            },
        );
        let model_registry = Arc::new(build_model_registry());

        let agent_tools = subagent_tools(params.tools.as_ref());

        // Resolve model: explicit override > managed config executor model > provider default.
        let model = resolve_subagent_model(&params, ctx);

        let system_prompt = params.system_prompt.unwrap_or_else(|| {
            let mut prompt = "You are a specialized AI agent helping with a specific sub-task. \
             Complete the task thoroughly and return your findings."
                .to_string();

            // Append plugin-contributed agent definitions so the sub-agent
            // is aware of any specialised agents declared by plugins.
            let agent_defs = mikmik_plugins::global_plugin_registry()
                .map(|registry| plugin_agent_definitions(&registry.all_agent_paths()))
                .unwrap_or_default();
            if !agent_defs.is_empty() {
                prompt.push_str("\n\nThe following specialized agents are available:");
                prompt.push_str(&agent_defs);
            }

            prompt
        });

        // Resolve max_turns: explicit > managed config executor_max_turns > default.
        let resolved_max_turns = params.max_turns.unwrap_or_else(|| {
            ctx.managed_agent_config
                .as_ref()
                .map(|c| c.executor_max_turns)
                .unwrap_or(10)
        });

        // Resolve isolation: explicit param > managed config executor_isolation.
        let resolved_isolation = params.isolation.clone().or_else(|| {
            if ctx
                .managed_agent_config
                .as_ref()
                .map(|c| c.executor_isolation)
                .unwrap_or(false)
            {
                Some("worktree".to_string())
            } else {
                None
            }
        });

        // -----------------------------------------------------------------------
        // Determine working directory - optionally isolate in a git worktree.
        // -----------------------------------------------------------------------
        let use_isolation = resolved_isolation.as_deref() == Some("worktree");
        let agent_id = uuid::Uuid::new_v4().to_string();

        // Claim an address before the loop starts, so a message sent to this
        // agent on its first turn already has somewhere to land. The guard
        // outlives the run and takes the inbox with it when it drops.
        let (agent_name, inbox_guard) = mikmik_tools::register_named(
            &ctx.session_id,
            params.name.as_deref(),
            &params.description,
        );
        let sub_address = mikmik_tools::AgentAddress {
            own: inbox_guard.key().to_string(),
            parent: Some(ctx.inbox.own.clone()),
            name: Some(agent_name.clone()),
            // A foreground sub-agent is awaited by its parent, which therefore
            // takes no turn while this one runs.
            parent_blocked: !params.run_in_background,
        };

        let (working_dir_str, worktree_path, git_root): (String, Option<PathBuf>, Option<PathBuf>) =
            if use_isolation {
                let git_root = find_git_root(&ctx.working_dir);
                if let Some(ref root) = git_root {
                    if let Some(wt) = create_worktree(root, &agent_id).await {
                        let wd = wt.display().to_string();
                        (wd, Some(wt), git_root)
                    } else {
                        warn!(
                            agent_id = %agent_id,
                            "Worktree creation failed; running agent in shared working directory"
                        );
                        (ctx.working_dir.display().to_string(), None, None)
                    }
                } else {
                    warn!(
                        agent_id = %agent_id,
                        "No git root found; isolation=worktree ignored"
                    );
                    (ctx.working_dir.display().to_string(), None, None)
                }
            } else {
                (ctx.working_dir.display().to_string(), None, None)
            };

        let query_config = QueryConfig {
            model,
            max_tokens: mikmik_core::constants::DEFAULT_MAX_TOKENS,
            max_turns: resolved_max_turns,
            // A sub-agent answers its parent, so both settings follow the
            // session it was spawned from.
            degradation_summary: ctx.config.degradation_summary.unwrap_or(true),
            auto_poke: ctx.config.auto_poke.unwrap_or(true),
            // Follows the session too. A sub-agent isolated in a worktree
            // resolves a project root of its own and so reads an empty memory
            // directory; that is the safe direction, since the worktree is a
            // scratch checkout and nothing there should be remembered as the
            // project's own.
            auto_memory_enabled: mikmik_core::memdir::is_auto_memory_enabled(
                ctx.config.auto_memory_enabled,
            ),
            auto_compact: ctx.config.effective_auto_compact(),
            compact_threshold: ctx.config.effective_compact_threshold(),
            system_prompt: Some(system_prompt),
            append_system_prompt: None,
            output_style: ctx.config.effective_output_style(),
            output_style_prompt: ctx.config.resolve_output_style_prompt(),
            // A sub-agent may run in a worktree of its own, so its roots are
            // named from its working directory rather than the parent's.
            workspace_roots: mikmik_core::workspace::generate_root_names(
                std::path::Path::new(&working_dir_str),
                &ctx.config.additional_dirs,
                &ctx.config.workspace_paths,
            )
            .into_iter()
            .map(|(name, path)| (name, path.display().to_string()))
            .collect(),
            working_directory: Some(working_dir_str),
            thinking_budget: None,
            temperature: None,
            tool_result_budget: 50_000,
            effort_level,
            command_queue: None,
            skill_index: None,
            max_budget_usd: None,
            fallback_model: None,
            provider_registry: Some(Arc::new(provider_registry)),
            agent_name: None,
            agent_definition: None,
            model_registry: Some(model_registry),
            managed_agents: None,
            // Progressive tool disclosure (issue #233): the sub-agent's system
            // prompt only needs guideline blocks for the tools it actually has.
            enabled_tools: Some(agent_tools.iter().map(|t| t.name().to_string()).collect()),
            // Sub-agents run to their own completion and never drive goal
            // continuation — stop after one turn like every non-goal run.
            continuation: crate::continuation::ContinuationMode::Default,
            // The companion sits beside the user's input box. A sub-agent has
            // no input box, so describing it would only spend tokens.
            companion_addendum: None,
        };
        // -----------------------------------------------------------------------
        // Background mode: spawn and return agent_id immediately.
        // -----------------------------------------------------------------------
        if params.run_in_background {
            let mut task = mikmik_core::tasks::BackgroundTask::new(format!(
                "subagent: {}",
                params.description
            ));
            task.id = agent_id.clone();
            // Cancellation token shared between the registry and the spawned
            // sub-agent loop: signalling it via TaskRegistry::cancel (e.g. from a
            // monitor cancel) actually stops the loop instead of only relabeling
            // the task (issue #219). Derive it as a CHILD of the parent's token
            // so cancelling the parent query also cancels this sub-agent, while
            // the registry can still cancel this sub-agent independently (#218).
            let cancel = ctx.cancel_token.child_token();
            task.cancel_token = Some(cancel.clone());
            let _ = mikmik_core::tasks::global_registry().register(task);

            // Re-create the tool list inside the closure so it is owned and
            // Send. It honours the caller's allowlist, which the background
            // branch used to ignore.
            let agent_tools_bg = subagent_tools(params.tools.as_ref());

            let client_bg = client.clone();
            let ctx_bg = subagent_context(ctx, sub_address);
            let config_bg = query_config.clone();
            let cost_tracker_bg = ctx.cost_tracker.clone();
            let description_bg = params.description.clone();
            let prompt_bg = effective_prompt.clone();
            let agent_id_bg = agent_id.clone();

            tokio::spawn(async move {
                // Moved in so the address stays claimed for the whole run and
                // is released the moment the task ends, however it ends.
                let _inbox_guard = inbox_guard;
                // Wait for a slot before the first request goes out, so the
                // configured limit bounds spend rather than only wall-clock.
                let _slot = executor_permit(&ctx_bg).await;
                let mut messages = vec![Message::user(prompt_bg)];
                let outcome = run_query_loop(
                    client_bg.as_ref(),
                    &mut messages,
                    &agent_tools_bg,
                    &ctx_bg,
                    &config_bg,
                    cost_tracker_bg,
                    None,
                    cancel,
                    None,
                )
                .await;

                // Cleanup worktree if one was created.
                if let (Some(root), Some(wt)) = (git_root, worktree_path) {
                    remove_worktree(&root, &wt).await;
                }

                // Respect a prior external cancellation mark from monitor cancel.
                let cancelled = matches!(
                    mikmik_core::tasks::global_registry()
                        .get(&agent_id_bg)
                        .map(|t| t.status),
                    Some(mikmik_core::tasks::TaskStatus::Cancelled)
                );

                let status = task_status_for(&outcome);
                let result_text = format_outcome(outcome);
                mikmik_core::tasks::global_registry().append_output(&agent_id_bg, &result_text);

                if !cancelled {
                    mikmik_core::tasks::global_registry().update_status(&agent_id_bg, status);
                }

                debug!(
                    agent_id = %agent_id_bg,
                    description = %description_bg,
                    "Background agent completed"
                );
            });

            return ToolResult::success(
                serde_json::json!({
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "status": "running",
                    "message": format!(
                        "Agent '{}' started in background. Use monitor with action=status/output \
                         and task_id='{}'. Send it a message with SendMessage to='{}'.",
                        params.description, agent_id, agent_name
                    )
                })
                .to_string(),
            );
        }

        // -----------------------------------------------------------------------
        // Synchronous mode: run the sub-agent loop and wait for completion.
        // -----------------------------------------------------------------------
        let mut messages = vec![Message::user(effective_prompt)];
        // Derive the sub-agent's token as a CHILD of the parent's so a parent
        // cancel propagates into this sub-agent's own run_query_loop (issue #218).
        let cancel = ctx.cancel_token.child_token();

        mikmik_plugins::run_global_hook(
            mikmik_plugins::HookEventKind::SubagentStart,
            None,
            serde_json::json!({
                "description": params.description,
                "model": query_config.model,
                "session_id": ctx.session_id,
            }),
        )
        .await;

        // Held until the run returns; dropping it releases the address.
        let _inbox_guard = inbox_guard;
        // Register the run so it appears beside background agents while it is
        // live. Only the background branch used to register, so a foreground
        // sub-agent was invisible for its whole run.
        let mut task =
            mikmik_core::tasks::BackgroundTask::new(format!("subagent: {}", params.description));
        task.id = agent_id.clone();
        let _ = mikmik_core::tasks::global_registry().register(task);

        let sub_ctx = subagent_context(ctx, sub_address);
        let outcome = run_query_loop(
            client.as_ref(),
            &mut messages,
            &agent_tools,
            &sub_ctx,
            &query_config,
            ctx.cost_tracker.clone(),
            None, // no event forwarding for sub-agents
            cancel,
            None, // no pending message queue for sub-agents
        )
        .await;

        mikmik_core::tasks::global_registry().update_status(&agent_id, task_status_for(&outcome));

        // Cleanup worktree if one was created.
        if let (Some(root), Some(wt)) = (git_root, worktree_path) {
            remove_worktree(&root, &wt).await;
        }

        mikmik_plugins::run_global_hook(
            mikmik_plugins::HookEventKind::SubagentStop,
            None,
            serde_json::json!({
                "description": params.description,
                "outcome": outcome_label(&outcome),
                "session_id": ctx.session_id,
            }),
        )
        .await;

        match outcome {
            QueryOutcome::EndTurn { message, usage } => {
                let text = message.get_all_text();
                debug!(
                    description = %params.description,
                    output_tokens = usage.output_tokens,
                    "Sub-agent completed"
                );
                ToolResult::success(text)
            }
            QueryOutcome::MaxTokens {
                partial_message, ..
            } => {
                let text = partial_message.get_all_text();
                ToolResult::success(format!("{}\n\n[Note: Agent hit max_tokens limit]", text))
            }
            QueryOutcome::Cancelled => ToolResult::error("Sub-agent was cancelled".to_string()),
            QueryOutcome::Error(e) => ToolResult::error(format!("Sub-agent error: {}", e)),
            QueryOutcome::BudgetExceeded {
                cost_usd,
                limit_usd,
            } => ToolResult::error(format!(
                "Sub-agent stopped: budget ${:.4} exceeded (limit ${:.4})",
                cost_usd, limit_usd
            )),
        }
    }
}

/// Validate a batch and turn each task into a labelled single-agent input.
///
/// The shared `context` is written onto every task, so the spawn path reads it
/// the same way it reads a single spawn's context. A name given twice is an
/// error, and a task that is not an object is an error, both reported before
/// any agent starts. Each task is returned with the label the result block
/// carries: its name, or its description when it has no name.
fn prepare_batch_tasks(
    context: Option<&str>,
    tasks: Vec<Value>,
) -> Result<Vec<(String, Value)>, String> {
    if tasks.is_empty() {
        return Err("tasks must name at least one agent".to_string());
    }

    // Refuse a name two tasks share, before preparing anything.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for task in &tasks {
        if let Some(name) = task.get("name").and_then(Value::as_str) {
            if !seen.insert(name) {
                return Err(format!(
                    "two tasks share the name {name:?}; names must be unique within a batch"
                ));
            }
        }
    }

    let shared = context.map(str::trim).filter(|context| !context.is_empty());
    let mut prepared: Vec<(String, Value)> = Vec::with_capacity(tasks.len());
    for (index, mut task) in tasks.into_iter().enumerate() {
        let Some(object) = task.as_object_mut() else {
            return Err(format!("task {index} is not an object"));
        };
        // A batch task never nests another batch.
        object.remove("tasks");
        if let Some(shared) = shared {
            object.insert("context".to_string(), Value::String(shared.to_string()));
        }
        let label = object
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| object.get("description").and_then(Value::as_str))
            .unwrap_or("agent")
            .to_string();
        prepared.push((label, task));
    }
    Ok(prepared)
}

impl AgentTool {
    /// Spawn a batch of agents from a single `tasks` array.
    ///
    /// A batch is exactly N single spawns: every task is turned into its own
    /// single-agent input and run through `execute`, with the shared `context`
    /// prepended to each prompt. A name given twice in the batch is refused
    /// before any agent starts, so the caller learns of the collision instead
    /// of getting two agents fighting over one address.
    async fn spawn_batch(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let batch: BatchInput = match serde_json::from_value(input) {
            Ok(batch) => batch,
            Err(error) => return ToolResult::error(format!("Invalid batch input: {error}")),
        };
        let prepared = match prepare_batch_tasks(batch.context.as_deref(), batch.tasks) {
            Ok(prepared) => prepared,
            Err(error) => return ToolResult::error(error),
        };

        // Fan out; each task runs the same single-spawn path concurrently.
        let running = prepared.into_iter().map(|(label, task)| async move {
            let result = self.execute(task, ctx).await;
            (label, result)
        });
        let results = futures::future::join_all(running).await;

        // One block per agent, in task order, labelled by name or description.
        let any_error = results.iter().any(|(_, result)| result.is_error);
        let mut body = String::new();
        for (label, result) in &results {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!("=== {label} ===\n{}", result.content));
        }
        if any_error {
            ToolResult::error(body)
        } else {
            ToolResult::success(body)
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: convert a QueryOutcome into a result string for background agents
// ---------------------------------------------------------------------------

fn format_outcome(outcome: QueryOutcome) -> String {
    match outcome {
        QueryOutcome::EndTurn { message, .. } => message.get_all_text(),
        QueryOutcome::MaxTokens {
            partial_message, ..
        } => format!(
            "{}\n\n[Note: Agent hit max_tokens limit]",
            partial_message.get_all_text()
        ),
        QueryOutcome::Cancelled => "[Agent was cancelled]".to_string(),
        QueryOutcome::Error(e) => format!("[Agent error: {}]", e),
        QueryOutcome::BudgetExceeded {
            cost_usd,
            limit_usd,
        } => format!(
            "[Agent stopped: budget ${:.4} exceeded (limit ${:.4})]",
            cost_usd, limit_usd
        ),
    }
}

// ---------------------------------------------------------------------------
// Team swarm runner injection
// ---------------------------------------------------------------------------
//
// Called once at process startup (e.g. from main.rs) to inject a real agent
// runner into cc-tools so that TeamCreateTool can spawn sub-agents via
// run_query_loop without creating a circular crate dependency.

/// Register the cc-query-backed agent runner with cc-tools.
///
/// After this call, `TeamCreateTool` will actually invoke `run_query_loop` for
/// each agent instead of returning stub output.
///
/// # Panics
/// Panics if the runner was already registered.
pub fn init_team_swarm_runner() {
    let runner: mikmik_tools::AgentRunFn = Arc::new(
        |description: String,
         name: String,
         prompt: String,
         tools: Option<Vec<String>>,
         system: Option<String>,
         max_turns: Option<u32>,
         ctx: Arc<mikmik_tools::ToolContext>| {
            // We must return a Pin<Box<dyn Future<...> + Send>>.
            Box::pin(async move {
                // The team gave this agent a name, so claim it as an address
                // before the loop starts. Teammates run at the same time, which
                // is exactly the case messaging is for.
                let (agent_name, inbox_guard) =
                    mikmik_tools::register_named(&ctx.session_id, Some(&name), &description);
                // Team members run beside each other, so they queue on the
                // session's executor slots exactly as background agents do.
                let _slot = executor_permit(&ctx).await;
                let anthropic_key = ctx.config.resolve_anthropic_api_key().unwrap_or_default();
                let anthropic_base = ctx.config.resolve_anthropic_api_base();
                let client =
                    match mikmik_api::AnthropicClient::new(mikmik_api::client::ClientConfig {
                        api_key: anthropic_key.clone(),
                        api_base: anthropic_base,
                        ..Default::default()
                    }) {
                        Ok(c) => Arc::new(c),
                        Err(e) => {
                            return format!(
                                "[Agent '{}' failed to create client: {}]",
                                description, e
                            )
                        }
                    };

                let provider_registry = ProviderRegistry::from_config(
                    &ctx.config,
                    mikmik_api::client::ClientConfig {
                        api_key: anthropic_key,
                        api_base: ctx.config.resolve_anthropic_api_base(),
                        ..Default::default()
                    },
                );
                let model_registry = Arc::new(build_model_registry());

                let agent_tools = subagent_tools(tools.as_ref());

                let model = resolve_subagent_model(
                    &AgentInput {
                        description: description.clone(),
                        name: Some(agent_name.clone()),
                        prompt: prompt.clone(),
                        tools: tools.clone(),
                        system_prompt: system.clone(),
                        max_turns,
                        model: None,
                        effort: None,
                        context: None,
                        isolation: None,
                        run_in_background: false,
                    },
                    &ctx,
                );

                let system_prompt = system.unwrap_or_else(|| {
                    "You are a specialized AI agent helping with a specific sub-task. \
                     Complete the task thoroughly and return your findings."
                        .to_string()
                });

                let query_config = crate::QueryConfig {
                    model,
                    max_tokens: mikmik_core::constants::DEFAULT_MAX_TOKENS,
                    max_turns: max_turns.unwrap_or(10),
                    system_prompt: Some(system_prompt),
                    working_directory: Some(ctx.working_dir.display().to_string()),
                    output_style: ctx.config.effective_output_style(),
                    output_style_prompt: ctx.config.resolve_output_style_prompt(),
                    provider_registry: Some(Arc::new(provider_registry)),
                    model_registry: Some(model_registry),
                    // Progressive tool disclosure (issue #233): only emit
                    // per-tool guidance for tools this team sub-agent has.
                    enabled_tools: Some(agent_tools.iter().map(|t| t.name().to_string()).collect()),
                    ..Default::default()
                };

                // Child of the parent's token so a parent cancel propagates into
                // this team sub-agent as well (issue #218).
                let cancel = ctx.cancel_token.child_token();
                let mut messages = vec![mikmik_core::types::Message::user(prompt)];
                let sub_ctx = subagent_context(
                    &ctx,
                    mikmik_tools::AgentAddress {
                        own: inbox_guard.key().to_string(),
                        parent: Some(ctx.inbox.own.clone()),
                        name: Some(agent_name),
                        // TeamCreate awaits every agent it started, so the
                        // session that spawned this team cannot answer until
                        // the whole team has finished.
                        parent_blocked: true,
                    },
                );
                let outcome = crate::run_query_loop(
                    client.as_ref(),
                    &mut messages,
                    &agent_tools,
                    &sub_ctx,
                    &query_config,
                    ctx.cost_tracker.clone(),
                    None,
                    cancel,
                    None,
                )
                .await;

                format_outcome(outcome)
            }) as Pin<Box<dyn std::future::Future<Output = String> + Send>>
        },
    );

    mikmik_tools::register_agent_runner(runner);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn names_of(tools: &[Box<dyn Tool>]) -> Vec<&str> {
        tools.iter().map(|t| t.name()).collect()
    }

    /// A context whose session may run `limit` executors at once.
    fn managed_context(session: &str, enabled: bool, limit: u32) -> ToolContext {
        let mut ctx = parent_context();
        ctx.session_id = session.to_string();
        ctx.managed_agent_config = Some(mikmik_core::ManagedAgentConfig {
            enabled,
            manager_model: "anthropic/claude-opus-4-6".to_string(),
            executor_model: "anthropic/claude-sonnet-4-6".to_string(),
            executor_max_turns: 10,
            max_concurrent_executors: limit,
            total_budget_usd: None,
            preset_name: None,
            executor_isolation: false,
        });
        ctx
    }

    /// The limit reached the model as a sentence in its prompt and bound
    /// nothing, so a manager that ignored it spent without a ceiling.
    #[tokio::test]
    async fn a_second_executor_waits_for_the_first_to_finish() {
        let ctx = managed_context("sess-slots-one", true, 1);

        let first = executor_permit(&ctx).await;
        assert!(first.is_some(), "the first executor took a slot");

        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), executor_permit(&ctx)).await;
        assert!(waited.is_err(), "a second executor ran past the limit");

        drop(first);
        let after =
            tokio::time::timeout(std::time::Duration::from_millis(500), executor_permit(&ctx))
                .await;
        assert!(
            matches!(after, Ok(Some(_))),
            "the slot was not released when the first finished"
        );
    }

    #[tokio::test]
    async fn executors_up_to_the_limit_run_together() {
        let ctx = managed_context("sess-slots-three", true, 3);

        let held: Vec<_> = vec![
            executor_permit(&ctx).await,
            executor_permit(&ctx).await,
            executor_permit(&ctx).await,
        ];
        assert!(held.iter().all(Option::is_some));

        let extra =
            tokio::time::timeout(std::time::Duration::from_millis(50), executor_permit(&ctx)).await;
        assert!(extra.is_err(), "a fourth executor ran past a limit of 3");
    }

    /// Without managed mode there is no limit to honour, so taking a permit
    /// would only add a wait to what the tree already does.
    #[tokio::test]
    async fn an_unmanaged_session_takes_no_slot() {
        let ctx = managed_context("sess-slots-off", false, 1);

        assert!(executor_permit(&ctx).await.is_none());
        assert!(executor_permit(&ctx).await.is_none());
    }

    /// One process serves several sessions under ACP, and a limit set in one
    /// must not throttle another.
    #[tokio::test]
    async fn one_session_does_not_hold_up_another() {
        let mine = managed_context("sess-slots-mine", true, 1);
        let theirs = managed_context("sess-slots-theirs", true, 1);

        let _held = executor_permit(&mine).await;
        let other = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            executor_permit(&theirs),
        )
        .await;

        assert!(
            matches!(other, Ok(Some(_))),
            "another session queued behind this one"
        );
    }

    /// A sub-agent that held `TeamCreate` could spawn through the same runner
    /// `Agent` was excluded to close off, and nothing bounds how deep that
    /// goes.
    #[test]
    fn a_sub_agent_holds_no_tool_that_spawns_agents() {
        let tools = subagent_tools(None);
        let names = names_of(&tools);

        assert!(!names.contains(&mikmik_core::constants::TOOL_NAME_AGENT));
        assert!(!names.contains(&mikmik_core::constants::TOOL_NAME_TEAM_CREATE));
    }

    /// Messaging is how an executor reports back, and neither of these spawns
    /// anything, so the filter must not take them.
    #[test]
    fn a_sub_agent_keeps_the_tools_that_only_coordinate() {
        let tools = subagent_tools(None);
        let names = names_of(&tools);

        assert!(names.contains(&"SendMessage"));
        assert!(names.contains(&mikmik_core::constants::TOOL_NAME_TEAM_DELETE));
        assert!(names.contains(&mikmik_core::constants::TOOL_NAME_TASK_STOP));
    }

    /// An allowlist narrows what is left after the spawn filter. Applying it
    /// the other way round would hand back a spawning tool on request.
    #[test]
    fn an_allowlist_cannot_ask_for_a_spawning_tool() {
        let asked = vec![
            mikmik_core::constants::TOOL_NAME_TEAM_CREATE.to_string(),
            mikmik_core::constants::TOOL_NAME_FILE_READ.to_string(),
        ];
        let tools = subagent_tools(Some(&asked));
        let names = names_of(&tools);

        assert_eq!(names, vec![mikmik_core::constants::TOOL_NAME_FILE_READ]);
    }

    #[test]
    fn an_allowlist_still_narrows_the_rest() {
        let asked = vec![mikmik_core::constants::TOOL_NAME_GREP.to_string()];
        let tools = subagent_tools(Some(&asked));

        assert_eq!(
            names_of(&tools),
            vec![mikmik_core::constants::TOOL_NAME_GREP]
        );
    }

    #[test]
    fn an_agent_definition_becomes_a_prompt_section() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("reviewer.md"), "Report only defects.").expect("write");
        std::fs::write(dir.path().join("notes.txt"), "ignored").expect("write");

        let defs = plugin_agent_definitions(&[dir.path().to_path_buf()]);

        assert!(defs.contains("## Agent: reviewer"), "{defs}");
        assert!(defs.contains("Report only defects."), "{defs}");
        assert!(!defs.contains("ignored"), "{defs}");
    }

    #[test]
    fn a_directory_that_does_not_exist_contributes_nothing() {
        let defs = plugin_agent_definitions(&[PathBuf::from("/nonexistent/agents")]);
        assert!(defs.is_empty());
    }

    /// A parent context, permissive and otherwise unremarkable.
    pub(crate) fn parent_context() -> ToolContext {
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
            working_dir: PathBuf::from("/workspace"),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: Arc::new(AllowAll),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "parent-session".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            non_interactive: false,
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

    /// A sub-agent must not be able to open the plan dialog: the session it
    /// would interrupt is the parent's, and the answer would move the parent's
    /// permission mode on a plan the user never asked for.
    #[test]
    fn a_sub_agent_cannot_reach_the_plan_dialog() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut parent = parent_context();
        parent.plan_approval_tx = Some(tx);

        let child = subagent_context(
            &parent,
            mikmik_tools::AgentAddress {
                own: format!("{}:scout", parent.session_id),
                parent: Some(parent.inbox.own.clone()),
                name: Some("scout".to_string()),
                parent_blocked: true,
            },
        );

        assert!(child.plan_approval_tx.is_none());
        // Everything else still comes along, including the session id, which is
        // what the plan file's own numbering has to survive.
        assert_eq!(child.session_id, parent.session_id);
        assert!(parent.plan_approval_tx.is_some(), "the parent kept its own");
    }

    /// The session id is shared, so the address is what tells parent and child
    /// apart. Sharing that too would have them draining one inbox.
    #[test]
    fn a_sub_agent_gets_an_address_of_its_own() {
        let mut parent = parent_context();
        parent.inbox.own = parent.session_id.clone();
        parent.inbox.name = Some(mikmik_tools::MAIN_NAME.to_string());

        let child = subagent_context(
            &parent,
            mikmik_tools::AgentAddress {
                own: format!("{}:scout", parent.session_id),
                parent: Some(parent.inbox.own.clone()),
                name: Some("scout".to_string()),
                parent_blocked: false,
            },
        );

        assert_ne!(child.inbox.own, parent.inbox.own);
        assert_eq!(
            child.inbox.parent.as_deref(),
            Some(parent.inbox.own.as_str())
        );
        assert_eq!(child.inbox.name.as_deref(), Some("scout"));
    }

    fn config_on(account: &str) -> mikmik_core::Config {
        let mut config = mikmik_core::Config {
            provider: Some(account.to_string()),
            ..Default::default()
        };
        config.provider_configs.insert(
            account.to_string(),
            mikmik_core::config::ProviderConfig::default(),
        );
        config
    }

    #[test]
    fn a_subagent_model_names_the_account_it_belongs_to() {
        let config = config_on("my_gateway");
        assert_eq!(
            subagent_model_for(&config, Some("claude-opus-5")),
            "my_gateway/claude-opus-5"
        );
    }

    #[test]
    fn a_models_own_namespace_is_not_read_as_an_account() {
        // The old rule left any id containing a slash alone. That is right for
        // `myaccount/haiku` and wrong here: `meta-llama/Llama-3.3-70B` is one
        // OpenRouter model id, so the subagent went to whichever account
        // happened to be active in its own config instead of this one.
        let config = config_on("openrouter");
        assert_eq!(
            subagent_model_for(&config, Some("meta-llama/Llama-3.3-70B")),
            "openrouter/meta-llama/Llama-3.3-70B"
        );

        let read_back = config.resolve_route(&subagent_model_for(
            &config,
            Some("meta-llama/Llama-3.3-70B"),
        ));
        assert_eq!(read_back.account, "openrouter");
        assert_eq!(read_back.model, "meta-llama/Llama-3.3-70B");
    }

    #[test]
    fn an_explicit_account_prefix_still_wins() {
        let mut config = config_on("my_gateway");
        config.provider_configs.insert(
            "other_gateway".to_string(),
            mikmik_core::config::ProviderConfig::default(),
        );
        assert_eq!(
            subagent_model_for(&config, Some("other_gateway/some-model")),
            "other_gateway/some-model"
        );
    }

    #[test]
    fn no_override_follows_the_parent_session() {
        // And does not go through `resolve_route(effective_model())`, whose
        // OpenRouter fallback is the slashed id `anthropic/claude-sonnet-4`.
        let config = config_on("openrouter");
        assert_eq!(
            subagent_model_for(&config, None),
            "openrouter/anthropic/claude-sonnet-4"
        );
    }

    /// The shared context is the whole point of a batch: it must sit above
    /// each task's prompt without the caller repeating it in every task.
    #[test]
    fn a_shared_context_prefixes_the_prompt() {
        assert_eq!(
            with_context(Some("background"), "do the thing"),
            "background\n\ndo the thing"
        );
    }

    /// No context, or a blank one, leaves the prompt exactly as it was, so an
    /// empty preamble does not push a stray blank line above the task.
    #[test]
    fn no_context_leaves_the_prompt_alone() {
        assert_eq!(with_context(None, "do the thing"), "do the thing");
        assert_eq!(with_context(Some("   "), "do the thing"), "do the thing");
    }

    /// The batch writes the shared context onto every task, so the spawn path
    /// reads it the same way it reads a single spawn's own context.
    #[test]
    fn a_batch_injects_the_shared_context_into_each_task() {
        let tasks = vec![
            json!({ "description": "one", "prompt": "first" }),
            json!({ "description": "two", "prompt": "second" }),
        ];
        let prepared =
            prepare_batch_tasks(Some("shared background"), tasks).expect("batch is valid");

        assert_eq!(prepared.len(), 2);
        for (_, task) in &prepared {
            assert_eq!(
                task.get("context").and_then(Value::as_str),
                Some("shared background")
            );
        }
    }

    /// The label a result block carries is the task's name, or its description
    /// when no name was given, so the caller can tell the agents apart.
    #[test]
    fn a_batch_labels_each_task_by_name_then_description() {
        let tasks = vec![
            json!({ "name": "scout", "description": "one", "prompt": "first" }),
            json!({ "description": "the second job", "prompt": "second" }),
        ];
        let prepared = prepare_batch_tasks(None, tasks).expect("batch is valid");

        assert_eq!(prepared[0].0, "scout");
        assert_eq!(prepared[1].0, "the second job");
    }

    /// Two agents at one address would fight over every message sent to it, so
    /// a name given twice fails the whole batch before any agent starts.
    #[test]
    fn a_batch_refuses_a_repeated_name() {
        let tasks = vec![
            json!({ "name": "scout", "description": "one", "prompt": "first" }),
            json!({ "name": "scout", "description": "two", "prompt": "second" }),
        ];
        let error = prepare_batch_tasks(None, tasks).expect_err("the clash must be refused");

        assert!(error.contains("unique within a batch"), "{error}");
    }

    /// An empty task list is a caller mistake, not an empty success, so it is
    /// reported rather than fanning out to nothing.
    #[test]
    fn a_batch_refuses_an_empty_task_list() {
        let error = prepare_batch_tasks(Some("ctx"), vec![]).expect_err("empty must be refused");
        assert!(error.contains("at least one agent"), "{error}");
    }
}
