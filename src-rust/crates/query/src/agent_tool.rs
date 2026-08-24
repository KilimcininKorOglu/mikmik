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
use mikmik_api::client::ClientConfig;
use mikmik_api::{AnthropicClient, ModelRegistry, ProviderRegistry};
use mikmik_core::types::Message;
use mikmik_tools::{PermissionLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
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
    /// Set to "worktree" to run the agent in an isolated git worktree.
    /// Omit (or set to null) for shared working directory.
    #[serde(default)]
    isolation: Option<String>,
    /// If true, start the agent in the background and return agent_id immediately.
    /// Default: false (wait for completion).
    #[serde(default)]
    run_in_background: bool,
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
        let params: AgentInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

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
            effort_level: None,
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
            let prompt_bg = params.prompt.clone();
            let agent_id_bg = agent_id.clone();

            tokio::spawn(async move {
                // Moved in so the address stays claimed for the whole run and
                // is released the moment the task ends, however it ends.
                let _inbox_guard = inbox_guard;
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

                let result_text = format_outcome(outcome);
                mikmik_core::tasks::global_registry().append_output(&agent_id_bg, &result_text);

                if !cancelled {
                    let status = if result_text.starts_with("[Agent error:")
                        || result_text.starts_with("[Agent stopped:")
                    {
                        mikmik_core::tasks::TaskStatus::Failed(result_text.clone())
                    } else {
                        mikmik_core::tasks::TaskStatus::Completed
                    };
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
        let mut messages = vec![Message::user(params.prompt)];
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
}
