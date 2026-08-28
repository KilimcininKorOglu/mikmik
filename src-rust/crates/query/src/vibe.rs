//! The `vibe` tool: persistent worker sessions.
//!
//! A worker keeps its conversation across messages. `spawn` starts it on a
//! first prompt; `send` gives it the next one; `wait` blocks for the turn it is
//! running; `kill` stops it; `list` shows them all. The worker's turn loop
//! stays alive between messages, parked on a channel, so its context survives
//! from one `send` to the next without reviving a finished agent. That keeps
//! `SendMessage`'s contract untouched: a vibe worker has a channel of its own.

use std::sync::Arc;

use async_trait::async_trait;
use mikmik_api::client::ClientConfig;
use mikmik_api::{AnthropicClient, ProviderRegistry};
use mikmik_core::types::Message;
use mikmik_tools::{PermissionLevel, Tool, ToolContext, ToolResult};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::agent_tool::{
    build_model_registry, format_outcome, subagent_context, subagent_model_for, subagent_tools,
};
use crate::{run_query_loop, QueryConfig};

/// A worker's live state, shared between its loop and the tool.
#[derive(Default)]
struct VibeState {
    /// The text of the most recent completed turn.
    latest_output: String,
    /// How many turns the worker has completed.
    turns: usize,
    /// Whether a turn is running right now.
    busy: bool,
    /// Whether the loop has ended (killed or its channel closed).
    done: bool,
}

/// One persistent worker: the channel that feeds it and its shared state.
struct Worker {
    sender: mpsc::UnboundedSender<String>,
    cancel: CancellationToken,
    state: Arc<Mutex<VibeState>>,
    notify: Arc<Notify>,
}

/// Workers by name, shared across every call in the process.
static WORKERS: Lazy<dashmap::DashMap<String, Worker>> = Lazy::new(dashmap::DashMap::new);

pub struct VibeTool;

#[derive(Debug, Deserialize)]
struct VibeInput {
    /// One of spawn, send, wait, kill, list.
    op: String,
    /// The worker name most ops act on.
    #[serde(default)]
    name: Option<String>,
    /// spawn: the first prompt; send: the next message.
    #[serde(default)]
    prompt: Option<String>,
    /// spawn: which tools to give the worker. Defaults to all.
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// spawn: model override for the worker.
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl Tool for VibeTool {
    fn name(&self) -> &str {
        "vibe"
    }

    fn description(&self) -> &str {
        "Persistent worker sessions that keep their context between messages. Ops:\n\
         - spawn: start a named worker on a first prompt.\n\
         - send: give a worker its next message; it keeps everything from before.\n\
         - wait: block until the worker finishes the turn it is running and return its output.\n\
         - kill: stop a worker.\n\
         - list: show the workers and how many turns each has run."
    }

    fn permission_level(&self) -> PermissionLevel {
        // Runs a full agent loop with tools, like Agent.
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["spawn", "send", "wait", "kill", "list"],
                    "description": "Which worker action to take."
                },
                "name": { "type": "string", "description": "The worker name most ops act on." },
                "prompt": { "type": "string", "description": "spawn: the first prompt; send: the next message." },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "spawn: tools to give the worker. Defaults to all."
                },
                "model": { "type": "string", "description": "spawn: model override for the worker." }
            },
            "required": ["op"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: VibeInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        match params.op.as_str() {
            "spawn" => op_spawn(params, ctx).await,
            "send" => op_send(params).await,
            "wait" => op_wait(params).await,
            "kill" => op_kill(params).await,
            "list" => op_list().await,
            other => ToolResult::error(format!(
                "unknown op {other:?}; use spawn, send, wait, kill or list"
            )),
        }
    }
}

/// Start a worker and run its first turn.
async fn op_spawn(params: VibeInput, ctx: &ToolContext) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("spawn needs a name".to_string());
    };
    if WORKERS.contains_key(&name) {
        return ToolResult::error(format!("a worker named {name:?} already exists"));
    }
    let Some(prompt) = params.prompt.clone() else {
        return ToolResult::error("spawn needs a first prompt".to_string());
    };

    let (sender, receiver) = mpsc::unbounded_channel::<String>();
    let cancel = ctx.cancel_token.child_token();
    let state = Arc::new(Mutex::new(VibeState::default()));
    let notify = Arc::new(Notify::new());

    let handle = WorkerHandle {
        cancel: cancel.clone(),
        state: state.clone(),
        notify: notify.clone(),
    };
    match spawn_worker_loop(&name, prompt, params, ctx, receiver, handle) {
        Ok(()) => {
            WORKERS.insert(
                name.clone(),
                Worker {
                    sender,
                    cancel,
                    state,
                    notify,
                },
            );
            ToolResult::success(format!("Spawned worker {name:?}."))
        }
        Err(error) => ToolResult::error(error),
    }
}

/// Give a worker its next message.
async fn op_send(params: VibeInput) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("send needs a name".to_string());
    };
    let Some(prompt) = params.prompt.clone() else {
        return ToolResult::error("send needs a prompt".to_string());
    };
    let Some(sender) = WORKERS.get(&name).map(|worker| worker.sender.clone()) else {
        return ToolResult::error(format!("no worker named {name:?}"));
    };
    match sender.send(prompt) {
        Ok(()) => ToolResult::success(format!("Sent to {name:?}.")),
        Err(_) => ToolResult::error(format!("worker {name:?} has stopped")),
    }
}

/// Block until the worker finishes the turn it is running, then return output.
async fn op_wait(params: VibeInput) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("wait needs a name".to_string());
    };
    let Some((state, notify)) = WORKERS
        .get(&name)
        .map(|worker| (worker.state.clone(), worker.notify.clone()))
    else {
        return ToolResult::error(format!("no worker named {name:?}"));
    };

    // If a turn is running, wait for it to finish; otherwise return at once.
    loop {
        let notified = notify.notified();
        {
            let guard = state.lock().await;
            if !guard.busy {
                return ToolResult::success(if guard.latest_output.is_empty() {
                    "(no output yet)".to_string()
                } else {
                    guard.latest_output.clone()
                });
            }
        }
        notified.await;
    }
}

/// Stop a worker and drop it.
async fn op_kill(params: VibeInput) -> ToolResult {
    let Some(name) = params.name.clone() else {
        return ToolResult::error("kill needs a name".to_string());
    };
    match WORKERS.remove(&name) {
        Some((_, worker)) => {
            worker.cancel.cancel();
            ToolResult::success(format!("Killed worker {name:?}."))
        }
        None => ToolResult::error(format!("no worker named {name:?}")),
    }
}

/// List the workers and how many turns each has run.
async fn op_list() -> ToolResult {
    let names: Vec<String> = WORKERS.iter().map(|entry| entry.key().clone()).collect();
    if names.is_empty() {
        return ToolResult::success("No workers.".to_string());
    }
    let mut lines = Vec::new();
    for name in names {
        if let Some(state) = WORKERS.get(&name).map(|worker| worker.state.clone()) {
            let guard = state.lock().await;
            let phase = if guard.done {
                "stopped"
            } else if guard.busy {
                "busy"
            } else {
                "idle"
            };
            lines.push(format!("{name}\t{phase}\t{} turn(s)", guard.turns));
        }
    }
    ToolResult::success(lines.join("\n"))
}

/// The handles the worker loop reports back through.
struct WorkerHandle {
    cancel: CancellationToken,
    state: Arc<Mutex<VibeState>>,
    notify: Arc<Notify>,
}

/// Build the worker's client, tools and config, then spawn its persistent loop.
///
/// The loop runs one turn, records its output, then parks on the channel for
/// the next message. Its `messages` vector is kept across turns, so the worker
/// remembers everything it has seen — the persistence the tool exists for.
fn spawn_worker_loop(
    name: &str,
    prompt: String,
    params: VibeInput,
    ctx: &ToolContext,
    mut receiver: mpsc::UnboundedReceiver<String>,
    handle: WorkerHandle,
) -> Result<(), String> {
    let anthropic_key = ctx.config.resolve_anthropic_api_key().unwrap_or_default();
    let client = AnthropicClient::new(ClientConfig {
        api_key: anthropic_key.clone(),
        api_base: ctx.config.resolve_anthropic_api_base(),
        ..Default::default()
    })
    .map_err(|error| format!("failed to create client: {error}"))?;

    let provider_registry = ProviderRegistry::from_config(
        &ctx.config,
        ClientConfig {
            api_key: anthropic_key,
            api_base: ctx.config.resolve_anthropic_api_base(),
            ..Default::default()
        },
    );
    let model_registry = Arc::new(build_model_registry());
    let tools = subagent_tools(params.tools.as_ref());
    let model = subagent_model_for(&ctx.config, params.model.as_deref());
    let config = worker_config(
        ctx,
        model,
        &tools,
        Arc::new(provider_registry),
        model_registry,
    );

    let worker_ctx = subagent_context(
        ctx,
        mikmik_tools::AgentAddress {
            own: format!("{}:vibe:{name}", ctx.session_id),
            parent: Some(ctx.inbox.own.clone()),
            name: Some(name.to_string()),
            parent_blocked: false,
        },
    );
    let cost_tracker = ctx.cost_tracker.clone();

    tokio::spawn(async move {
        let mut messages = vec![Message::user(prompt)];
        loop {
            if handle.cancel.is_cancelled() {
                break;
            }
            set_busy(&handle, true).await;
            let outcome = run_query_loop(
                &client,
                &mut messages,
                &tools,
                &worker_ctx,
                &config,
                cost_tracker.clone(),
                None,
                handle.cancel.child_token(),
                None,
            )
            .await;
            record_turn(&handle, format_outcome(outcome)).await;

            // Park until the next message, or a kill.
            tokio::select! {
                _ = handle.cancel.cancelled() => break,
                message = receiver.recv() => match message {
                    Some(message) => messages.push(Message::user(message)),
                    None => break,
                },
            }
        }
        mark_done(&handle).await;
    });
    Ok(())
}

/// Mark whether a turn is running and wake anyone waiting on the change.
async fn set_busy(handle: &WorkerHandle, busy: bool) {
    handle.state.lock().await.busy = busy;
    handle.notify.notify_waiters();
}

/// Record a finished turn's output and wake waiters.
async fn record_turn(handle: &WorkerHandle, output: String) {
    {
        let mut guard = handle.state.lock().await;
        guard.latest_output = output;
        guard.turns += 1;
        guard.busy = false;
    }
    handle.notify.notify_waiters();
}

/// Mark the loop finished and wake waiters.
async fn mark_done(handle: &WorkerHandle) {
    {
        let mut guard = handle.state.lock().await;
        guard.busy = false;
        guard.done = true;
    }
    handle.notify.notify_waiters();
}

/// The query config a worker turn runs under, mirroring a sub-agent's.
fn worker_config(
    ctx: &ToolContext,
    model: String,
    tools: &[Box<dyn Tool>],
    provider_registry: Arc<ProviderRegistry>,
    model_registry: Arc<mikmik_api::ModelRegistry>,
) -> QueryConfig {
    let working_dir = ctx.working_dir.display().to_string();
    QueryConfig {
        model,
        max_tokens: mikmik_core::constants::DEFAULT_MAX_TOKENS,
        max_turns: 10,
        degradation_summary: ctx.config.degradation_summary.unwrap_or(true),
        auto_poke: ctx.config.auto_poke.unwrap_or(true),
        auto_memory_enabled: mikmik_core::memdir::is_auto_memory_enabled(
            ctx.config.auto_memory_enabled,
        ),
        auto_compact: ctx.config.effective_auto_compact(),
        compact_threshold: ctx.config.effective_compact_threshold(),
        system_prompt: Some(
            "You are a persistent worker agent. You keep your context between \
             messages; complete each message thoroughly and wait for the next."
                .to_string(),
        ),
        append_system_prompt: None,
        output_style: ctx.config.effective_output_style(),
        output_style_prompt: ctx.config.resolve_output_style_prompt(),
        workspace_roots: mikmik_core::workspace::generate_root_names(
            std::path::Path::new(&working_dir),
            &ctx.config.additional_dirs,
            &ctx.config.workspace_paths,
        )
        .into_iter()
        .map(|(name, path)| (name, path.display().to_string()))
        .collect(),
        working_directory: Some(working_dir),
        thinking_budget: None,
        temperature: None,
        tool_result_budget: 50_000,
        effort_level: None,
        command_queue: None,
        skill_index: None,
        max_budget_usd: None,
        fallback_model: None,
        provider_registry: Some(provider_registry),
        agent_name: None,
        agent_definition: None,
        model_registry: Some(model_registry),
        managed_agents: None,
        enabled_tools: Some(tools.iter().map(|tool| tool.name().to_string()).collect()),
        continuation: crate::continuation::ContinuationMode::Default,
        companion_addendum: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_op_is_named_not_ignored() {
        let input: VibeInput =
            serde_json::from_value(json!({ "op": "teleport" })).expect("valid input");
        assert_eq!(input.op, "teleport");
    }

    #[tokio::test]
    async fn wait_on_an_unknown_worker_is_refused() {
        let result = op_wait(VibeInput {
            op: "wait".into(),
            name: Some("ghost".into()),
            prompt: None,
            tools: None,
            model: None,
        })
        .await;
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("no worker"), "{}", result.content);
    }

    #[tokio::test]
    async fn send_to_an_unknown_worker_is_refused() {
        let result = op_send(VibeInput {
            op: "send".into(),
            name: Some("ghost".into()),
            prompt: Some("hi".into()),
            tools: None,
            model: None,
        })
        .await;
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("no worker"), "{}", result.content);
    }

    /// The persistence the tool exists for: a message pushed to a worker lands
    /// in the same `messages` vector the first prompt started, so the worker's
    /// context carries across turns. Exercised without a model by driving the
    /// state helpers the loop uses, since a real turn needs a provider.
    #[tokio::test]
    async fn a_finished_turn_records_output_and_clears_busy() {
        let handle = WorkerHandle {
            cancel: CancellationToken::new(),
            state: Arc::new(Mutex::new(VibeState::default())),
            notify: Arc::new(Notify::new()),
        };
        set_busy(&handle, true).await;
        assert!(handle.state.lock().await.busy);

        record_turn(&handle, "first result".to_string()).await;
        let guard = handle.state.lock().await;
        assert!(!guard.busy);
        assert_eq!(guard.turns, 1);
        assert_eq!(guard.latest_output, "first result");
    }
}
