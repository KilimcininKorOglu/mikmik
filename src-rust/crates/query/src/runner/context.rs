//! Context-window accounting and compaction, run once per request.
//!
//! Compaction used to sit at the end of a turn inside the raw Anthropic
//! dispatch arm. That put it in the wrong place twice over: every provider
//! reached through the `LlmProvider` registry ran uncompacted and without a
//! token warning, and the user waited on a 20k-token summary call after their
//! answer had already finished streaming.
//!
//! It belongs at the request boundary instead, beside `sanitize_history`,
//! which the loop already calls "the single choke point covering BOTH the
//! legacy Anthropic path and the modern provider path". One call there reaches
//! both arms, and the work happens in front of the request that would
//! otherwise overflow rather than behind the one that just finished.

use mikmik_core::types::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::compact::{self, CompactBackend};
use crate::runner::apply_compact_result;
use crate::{QueryConfig, QueryEvent};

/// Whether this turn goes through the provider registry rather than the raw
/// Anthropic client.
///
/// The question is which wire format the account speaks, not what it is
/// called: an OAuth login named after its owner still belongs on the raw
/// client, which is the arm that refreshes an expired token. Anthropic itself
/// moves to the provider arm when the pre-built client has no key, which is
/// the case of a session that started without `ANTHROPIC_API_KEY` and gained
/// one through `/connect`.
///
/// The compaction pass and the dispatch arm both ask this, so a turn cannot be
/// summarised by one endpoint and answered by another.
pub fn dispatches_through_provider(
    account: &str,
    config: &mikmik_core::Config,
    client: &mikmik_api::AnthropicClient,
) -> bool {
    config.vendor_id_for_account(account) != mikmik_core::ProviderId::ANTHROPIC
        || client.api_key_is_empty()
}

/// Resolve the provider that will serve this turn's account.
///
/// Both the dispatch arm and the compaction pass in front of it need the same
/// handle, and picking it twice by hand is how the two would come to disagree
/// about which endpoint a turn belongs to.
pub fn provider_for_turn(
    registry: &mikmik_api::ProviderRegistry,
    config: &mikmik_core::Config,
    account: &str,
) -> Option<std::sync::Arc<dyn mikmik_api::provider::LlmProvider>> {
    let pid = mikmik_core::provider_id::ProviderId::new(account);

    // Always prefer a fresh provider built from the auth_store so that keys
    // added at runtime via /connect are picked up immediately, even when the
    // provider was pre-registered at startup with a stale or missing key.
    let runtime_provider = mikmik_api::registry::runtime_provider_for(account);
    let registry_provider = if runtime_provider.is_some() {
        None
    } else {
        registry.get(&pid).cloned()
    };
    let mut provider = runtime_provider.or(registry_provider);

    // Rebuild through the unified base resolver so overrides from settings,
    // env and defaults apply consistently.
    if mikmik_api::registry::resolve_provider_api_base(config, account).is_some() {
        if let Some(overridden) = mikmik_api::registry::provider_from_config(config, account) {
            provider = Some(overridden);
        }
    }

    provider
}

/// The endpoint that serves one route, as something compaction can call.
///
/// The same pair the dispatch arm asks, so a turn cannot be summarised by one
/// endpoint and answered by another, and the compact model reaches its own
/// account rather than the session's.
pub fn backend_for<'a>(
    route: &mikmik_core::config::Route,
    registry: Option<&mikmik_api::ProviderRegistry>,
    core_config: &mikmik_core::Config,
    client: &'a mikmik_api::AnthropicClient,
) -> Box<dyn CompactBackend + 'a> {
    match registry
        .filter(|_| dispatches_through_provider(&route.account, core_config, client))
        .and_then(|registry| provider_for_turn(registry, core_config, &route.account))
    {
        Some(provider) => Box::new(compact::ProviderBackend(provider)),
        None => Box::new(compact::AnthropicBackend(client)),
    }
}

/// Summarise a conversation on demand, honouring the compact model.
///
/// `/compact` and the ACP command both do exactly this and nothing else, so
/// they share it: two surfaces writing the same wiring twice is how one of them
/// ends up not honouring the setting. The turn loop keeps its own wiring
/// because it needs the turn's backend for micro-compaction as well.
pub async fn compact_on_demand(
    turn: &mikmik_core::config::Route,
    config: &mikmik_core::Config,
    registry: Option<&mikmik_api::ProviderRegistry>,
    client: &mikmik_api::AnthropicClient,
    messages: &[Message],
    instruction: Option<&str>,
    session_id: &str,
) -> compact::CompactionRun {
    let compact_route = config.resolve_compact_route(turn);
    let turn_backend = backend_for(turn, registry, config, client);

    // An unreachable summariser is not an error here: it is the case the
    // fallback exists for, so it has to reach `compact_with_fallback` as a
    // failing backend rather than short-circuit into the turn's model silently.
    let unreachable = compact::UnreachableBackend;
    let compact_backend: Box<dyn CompactBackend> =
        if config.reject_unserved_model(&compact_route).is_none() {
            backend_for(&compact_route, registry, config, client)
        } else {
            Box::new(unreachable)
        };

    let uses_a_separate_summariser = compact_route != *turn;
    compact::compact_with_fallback(
        compact::Summariser {
            backend: if uses_a_separate_summariser {
                compact_backend.as_ref()
            } else {
                turn_backend.as_ref()
            },
            route: &compact_route,
        },
        uses_a_separate_summariser.then_some(compact::Summariser {
            backend: turn_backend.as_ref(),
            route: turn,
        }),
        messages,
        instruction,
        session_id,
    )
    .await
}

/// Book a finished turn's usage: its cost, its price on the message, and the
/// prompt size the next request boundary will size the context from.
///
/// One function because the two dispatch arms each finish their own turn, and
/// three separate copies of this is how they came to disagree: the provider
/// arm recorded neither the cost nor, later, the prompt size, so a session
/// served through the registry priced itself at zero and then sized every
/// context from a chars/4 estimate that ignores the system prompt, the tool
/// schemas and the cache. Measured: it never compacted at all.
///
/// `model` must be `effective_model`, which follows an agent override and a
/// fallback switch; `config.model` does not.
pub(crate) fn record_turn_usage(
    assistant_msg: &mut Message,
    model: &str,
    pricing: mikmik_core::cost::ModelPricing,
    usage: &mikmik_core::types::UsageInfo,
    cost_tracker: &mikmik_core::cost::CostTracker,
    session_id: &str,
) {
    cost_tracker.add_usage(
        model,
        pricing,
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
    );
    assistant_msg.cost = Some(crate::cost_of_turn(model, pricing, usage));
    // `total_input()` is input + cache-read + cache-creation: what the model
    // actually saw. Session-scoped, not loop-scoped, because every user message
    // starts a fresh turn loop and a local would be zero at every boundary.
    compact::record_context_tokens(session_id, usage.total_input());
}

/// What this turn's model costs, from the catalogue where there is one.
///
/// The name heuristic behind `ModelPricing::for_model` reads a model id for
/// `opus`, `haiku` or `free` and prices everything else as Claude Sonnet, so a
/// session on any other vendor was billed at Anthropic's list price. It stays
/// only as the answer for a turn with no registry loaded.
pub(crate) fn pricing_for_turn(
    config: &QueryConfig,
    core_config: &mikmik_core::Config,
    route: &mikmik_core::config::Route,
) -> mikmik_core::cost::ModelPricing {
    match config.model_registry.as_deref() {
        Some(registry) => mikmik_api::pricing_for_route(core_config, registry, route),
        None => mikmik_core::cost::ModelPricing::for_model(route.model.as_str()),
    }
}

/// What one pass over the context boundary did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextPass {
    /// Message count before compaction; equal to `after` when nothing ran.
    pub before: usize,
    /// Message count afterwards.
    pub after: usize,
    /// The context size the next request will carry, in tokens.
    pub tokens_after: u64,
    /// Whether the conversation was actually replaced.
    pub compacted: bool,
}

/// Everything the pass needs about the turn it is fronting.
pub(crate) struct ContextPassInput<'a> {
    /// Where this turn is going, as `Config::resolve_route` resolved it from
    /// `effective_model`. The context window and the token warning are both
    /// sized from this one.
    ///
    /// The whole `Route` and not a bare model string, because a composite such
    /// as `"myaccount/some-model"` reaches the dispatch arm already split, and
    /// anything handed the unsplit string addresses a model the account does
    /// not serve.
    pub route: &'a mikmik_core::config::Route,
    /// The turn's own endpoint, and the fallback when the chosen summariser
    /// cannot be reached.
    pub turn_backend: &'a dyn CompactBackend,
    /// Who writes the summary, from `Config::resolve_compact_route`. Equal to
    /// `route` unless the user chose a compact model.
    pub compact_route: &'a mikmik_core::config::Route,
    /// The chosen summariser's endpoint, or `None` when its account has no
    /// usable credential. `None` is not a failure: the turn's own model
    /// writes the summary instead and the user is told.
    pub compact_backend: Option<&'a dyn CompactBackend>,
    /// The session, which owns the circuit breaker and the last prompt size.
    pub session_id: &'a str,
}

impl ContextPassInput<'_> {
    /// Whether the summary is going somewhere other than the turn.
    fn uses_a_separate_summariser(&self) -> bool {
        self.compact_route != self.route
    }
}

/// Size the context, warn about it, and compact when it is nearly full.
///
/// Call this immediately before `sanitize_history`: a cut can strand a
/// `tool_result` whose `tool_use` was summarised away, and the sanitiser
/// standing right behind it repairs exactly that.
pub(crate) async fn compact_before_request(
    messages: &mut Vec<Message>,
    config: &QueryConfig,
    input: ContextPassInput<'_>,
    event_tx: Option<&mpsc::UnboundedSender<QueryEvent>>,
    cancel_token: &CancellationToken,
) -> ContextPass {
    // Prefer the models.dev-backed registry value (correct for every provider:
    // 1M Gemini/GPT windows, 32k local models) and fall back to the
    // Claude-centric heuristic only when the registry has no usable entry.
    // (#216)
    let context_window = compact::resolve_context_window(
        config.model_registry.as_deref(),
        &input.route.account,
        input.route.model.as_str(),
    );

    // Prefer the REAL context-token count the provider reported for the last
    // turn (input + cache-read + cache-creation = what the model saw) over the
    // chars/4 estimate. With prompt caching the bare `input_tokens` field
    // undercounts badly. Estimate only before the first response. (#231)
    let last_reported = compact::compact_state_for(input.session_id).last_context_tokens;
    let context_tokens =
        compact::estimate_context_tokens(messages, (last_reported > 0).then_some(last_reported));

    let before = messages.len();
    let mut pass = ContextPass {
        before,
        after: before,
        tokens_after: context_tokens,
        compacted: false,
    };

    if context_window == 0 {
        return pass;
    }

    // The warning is not the compaction: it tells the user where they stand,
    // and it goes out even when auto-compact is switched off.
    let warning_state =
        compact::calculate_token_warning_state_for_window(context_tokens, context_window);
    if warning_state != compact::TokenWarningState::Ok {
        if let Some(tx) = event_tx {
            let _ = tx.send(QueryEvent::TokenWarning {
                state: warning_state,
                pct_used: context_tokens as f64 / context_window as f64,
            });
        }
    }

    // `autoCompact: false` means the user keeps the whole conversation and
    // accepts the consequence. They still get told how full it is.
    if !config.auto_compact {
        return pass;
    }

    // Reactive compact (T1-1) replaces the proactive path when its gate is set;
    // it fires from usage rather than from a finished turn and adds a 97%
    // emergency collapse. Off by default.
    if mikmik_core::feature_gates::is_feature_enabled("reactive_compact") {
        run_reactive(
            messages,
            config,
            &input,
            context_tokens,
            context_window,
            event_tx,
            cancel_token,
            &mut pass,
        )
        .await;
        return pass;
    }

    if !compact::should_compact_now(
        context_tokens,
        context_window,
        config.compact_threshold,
        input.session_id,
    ) {
        return pass;
    }

    if let Some(new_msgs) = summarise_with_fallback(&input, messages, event_tx).await {
        // A conversation already inside the keep-recent budget comes back
        // unchanged: the threshold was crossed by a prompt whose bulk is the
        // system prompt and the tool schemas, not by the turns. Reporting that
        // as a compaction would put "Compacted 0 messages" on screen and reset
        // the recorded size to an estimate that ignores everything the
        // provider counted.
        if new_msgs.len() != messages.len() {
            pass.after = new_msgs.len();
            pass.tokens_after = compact::estimate_tokens_for_messages(&new_msgs) as u64;
            pass.compacted = true;
            *messages = new_msgs;
            // The recorded size described the conversation that was just
            // replaced, so leaving it would compact again on the next request.
            compact::record_context_tokens(input.session_id, pass.tokens_after);
        }
    }

    pass
}

/// Write the summary, on the chosen model where that works and on the turn's
/// own where it does not.
///
/// The policy itself lives in `compact::compact_with_fallback`, because
/// `/compact` and the ACP command arm face the same decision. This adds the
/// only part that belongs to the turn loop: the substitution reaches the user
/// as a `Status` event.
async fn summarise_with_fallback(
    input: &ContextPassInput<'_>,
    messages: &[Message],
    event_tx: Option<&mpsc::UnboundedSender<QueryEvent>>,
) -> Option<Vec<Message>> {
    // An account with no usable credential is a summariser that will fail,
    // so it is expressed as one: a backend that answers with why.
    let unreachable = compact::UnreachableBackend;
    let chosen = compact::Summariser {
        backend: match input.compact_backend {
            Some(backend) => backend,
            None if input.uses_a_separate_summariser() => &unreachable,
            None => input.turn_backend,
        },
        route: input.compact_route,
    };
    let fallback = input
        .uses_a_separate_summariser()
        .then_some(compact::Summariser {
            backend: input.turn_backend,
            route: input.route,
        });

    let run =
        compact::compact_with_fallback(chosen, fallback, messages, None, input.session_id).await;

    if let (Some(note), Some(tx)) = (run.note, event_tx) {
        let _ = tx.send(QueryEvent::Status(note));
    }
    run.result.ok()
}

/// The gated reactive path: emergency collapse first, then a normal compact.
#[allow(clippy::too_many_arguments)]
async fn run_reactive(
    messages: &mut Vec<Message>,
    config: &QueryConfig,
    input: &ContextPassInput<'_>,
    context_tokens: u64,
    context_window: u64,
    event_tx: Option<&mpsc::UnboundedSender<QueryEvent>>,
    cancel_token: &CancellationToken,
    pass: &mut ContextPass,
) {
    // Both calls take a clone, and `apply_compact_result` only overwrites
    // `*messages` on success, so a failed compaction cannot wipe the live
    // conversation (#213).
    let (label, outcome) = if compact::should_context_collapse(context_tokens, context_window) {
        if let Some(tx) = event_tx {
            let _ = tx.send(QueryEvent::Status(
                "Compacting context... (emergency collapse)".to_string(),
            ));
        }
        (
            "Context-collapse",
            compact::context_collapse(messages.clone(), input.turn_backend, &input.route.model)
                .await,
        )
    } else if compact::should_compact(context_tokens, context_window, config.compact_threshold) {
        if let Some(tx) = event_tx {
            let _ = tx.send(QueryEvent::Status("Compacting context...".to_string()));
        }
        (
            "Reactive compact",
            compact::reactive_compact(
                messages.clone(),
                input.turn_backend,
                &input.route.model,
                cancel_token.clone(),
                &[],
            )
            .await,
        )
    } else {
        return;
    };

    match apply_compact_result(messages, outcome) {
        Ok(tokens_freed) => {
            info!(tokens_freed, "{label} complete");
            pass.after = messages.len();
            pass.tokens_after = compact::estimate_tokens_for_messages(messages) as u64;
            pass.compacted = true;
            compact::record_context_tokens(input.session_id, pass.tokens_after);
        }
        Err(mikmik_core::error::ClaudeError::Cancelled) => {
            warn!("{label} was cancelled; conversation preserved");
        }
        Err(e) => {
            warn!(error = %e, "{label} failed; conversation preserved");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::error::ClaudeError;

    /// A summariser that answers with a fixed string and remembers the model
    /// it was asked to use.
    struct StubBackend {
        reply: Result<String, String>,
        model_seen: parking_lot::Mutex<Option<String>>,
    }

    impl StubBackend {
        fn answering(reply: &str) -> Self {
            Self {
                reply: Ok(reply.to_string()),
                model_seen: parking_lot::Mutex::new(None),
            }
        }

        fn failing() -> Self {
            Self {
                reply: Err("the summariser is down".to_string()),
                model_seen: parking_lot::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl CompactBackend for StubBackend {
        async fn summarise(
            &self,
            _system: &str,
            _user: &str,
            model: &mikmik_core::config::WireModel,
            _max_tokens: u32,
        ) -> Result<String, ClaudeError> {
            *self.model_seen.lock() = Some(model.to_string());
            self.reply
                .clone()
                .map_err(|e| ClaudeError::Other(e.to_string()))
        }
    }

    /// A threshold `a_cuttable_conversation` crosses, so these tests exercise
    /// which model writes the summary rather than whether one is written.
    fn at_a_low_threshold() -> QueryConfig {
        QueryConfig {
            compact_threshold: 20,
            ..QueryConfig::default()
        }
    }

    /// A pass whose summary is written somewhere other than the turn.
    fn split_input<'a>(
        turn_backend: &'a StubBackend,
        route: &'a mikmik_core::config::Route,
        compact_backend: Option<&'a StubBackend>,
        compact_route: &'a mikmik_core::config::Route,
        session_id: &'a str,
    ) -> ContextPassInput<'a> {
        ContextPassInput {
            route,
            turn_backend,
            compact_route,
            compact_backend: compact_backend.map(|b| b as &dyn CompactBackend),
            session_id,
        }
    }

    /// A conversation past the trigger threshold of a 200k window.
    fn a_full_conversation() -> Vec<Message> {
        vec![
            Message::user("x".repeat(400_000)),
            Message::assistant("y".repeat(400_000)),
            Message::user("and now the next thing"),
        ]
    }

    /// Over the keep-recent budget so a cut really happens, but far under the
    /// window by the chars/4 estimate, so only a reported figure can push it
    /// past the threshold.
    fn a_cuttable_conversation() -> Vec<Message> {
        vec![
            Message::user("x".repeat(60_000)),
            Message::assistant("y".repeat(60_000)),
            Message::user("and now the next thing"),
        ]
    }

    /// A route as `Config::resolve_route` would hand it over.
    fn route(model: &str) -> mikmik_core::config::Route {
        mikmik_core::config::Config::default().route_for_account("anthropic", model)
    }

    fn input<'a>(
        backend: &'a StubBackend,
        route: &'a mikmik_core::config::Route,
        session_id: &'a str,
    ) -> ContextPassInput<'a> {
        ContextPassInput {
            route,
            turn_backend: backend,
            compact_route: route,
            compact_backend: None,
            session_id,
        }
    }

    async fn run(messages: &mut Vec<Message>, backend: &StubBackend, model: &str) -> ContextPass {
        let config = QueryConfig::default();
        let route = route(model);
        compact_before_request(
            messages,
            &config,
            input(backend, &route, "context-pass-tests"),
            None,
            &CancellationToken::new(),
        )
        .await
    }

    /// The pass acts on a full window, whichever arm supplied the backend.
    #[tokio::test]
    async fn a_full_window_is_compacted_at_the_request_boundary() {
        compact::forget_compact_state("context-pass-tests");
        let mut messages = a_full_conversation();
        let backend = StubBackend::answering("What went before, in short.");

        let pass = run(&mut messages, &backend, "claude-opus-4-5").await;

        assert!(pass.compacted, "a full window compacts");
        assert!(pass.after < pass.before, "the head was replaced");
        assert_eq!(messages.len(), pass.after);
        assert!(
            pass.tokens_after < 800_000,
            "the reported size follows the shortened conversation"
        );
        compact::forget_compact_state("context-pass-tests");
    }

    /// The summariser is asked for the model that ran, not the session model.
    #[tokio::test]
    async fn the_summariser_is_given_the_model_that_ran() {
        compact::forget_compact_state("context-pass-tests");
        let mut messages = a_full_conversation();
        let backend = StubBackend::answering("Short.");

        run(&mut messages, &backend, "some-agent-override-model").await;

        assert_eq!(
            backend.model_seen.lock().as_deref(),
            Some("some-agent-override-model")
        );
        compact::forget_compact_state("context-pass-tests");
    }

    /// The summariser names the model exactly as the turn does.
    ///
    /// `Route` carries the account and the wire model already split, and the
    /// pass takes the whole `Route` so a caller cannot hand it an
    /// `"<account>/<model>"` composite by accident. The turn goes out with
    /// `route.model`; a summary addressed to `"myaccount/some-model"` would
    /// name a model the account does not serve.
    #[tokio::test]
    async fn the_summariser_names_the_wire_model_from_the_route() {
        let session = "context-pass-route";
        compact::forget_compact_state(session);
        let mut messages = a_full_conversation();
        let backend = StubBackend::answering("Short.");
        let composite = mikmik_core::config::Config {
            provider_configs: std::iter::once((
                "myaccount".to_string(),
                mikmik_core::config::ProviderConfig::default(),
            ))
            .collect(),
            ..Default::default()
        }
        .resolve_route("myaccount/some-model");

        assert_eq!(composite.account, "myaccount");
        assert_eq!(composite.model, "some-model");

        compact_before_request(
            &mut messages,
            &QueryConfig::default(),
            input(&backend, &composite, session),
            None,
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            backend.model_seen.lock().as_deref(),
            Some("some-model"),
            "the account prefix must not travel as part of the model id"
        );
        compact::forget_compact_state(session);
    }

    /// A conversation nowhere near the threshold is left exactly as it was.
    #[tokio::test]
    async fn a_short_conversation_is_left_alone() {
        compact::forget_compact_state("context-pass-tests");
        let mut messages = vec![Message::user("hello"), Message::assistant("hi")];
        let backend = StubBackend::answering("never asked for");

        let pass = run(&mut messages, &backend, "claude-opus-4-5").await;

        assert!(!pass.compacted);
        assert_eq!(pass.before, pass.after);
        assert_eq!(messages.len(), 2);
        assert!(backend.model_seen.lock().is_none(), "no call was made");
        compact::forget_compact_state("context-pass-tests");
    }

    /// `autoCompact: false` keeps the conversation whole. The setting used to
    /// be written, saved and read by nobody.
    #[tokio::test]
    async fn auto_compact_off_leaves_a_full_window_alone() {
        compact::forget_compact_state("context-pass-tests");
        let mut messages = a_full_conversation();
        let backend = StubBackend::answering("never asked for");
        let config = QueryConfig {
            auto_compact: false,
            ..QueryConfig::default()
        };

        let pass = compact_before_request(
            &mut messages,
            &config,
            input(&backend, &route("claude-opus-4-5"), "context-pass-tests"),
            None,
            &CancellationToken::new(),
        )
        .await;

        assert!(!pass.compacted);
        assert_eq!(messages.len(), 3);
        assert!(backend.model_seen.lock().is_none(), "no call was made");
        compact::forget_compact_state("context-pass-tests");
    }

    /// A lower `compactThreshold` compacts a conversation the default would
    /// have left alone.
    #[tokio::test]
    async fn a_lower_threshold_compacts_sooner() {
        compact::forget_compact_state("context-pass-threshold");
        // ~50k tokens: a quarter of a 200k window, well under the default 90%.
        let mut messages = vec![
            Message::user("x".repeat(100_000)),
            Message::assistant("y".repeat(100_000)),
            Message::user("and now the next thing"),
        ];

        let route = route("claude-opus-4-5");
        let backend = StubBackend::answering("short");

        let at_default = compact_before_request(
            &mut messages.clone(),
            &QueryConfig::default(),
            input(&backend, &route, "context-pass-threshold"),
            None,
            &CancellationToken::new(),
        )
        .await;
        assert!(!at_default.compacted, "the default leaves this alone");

        let config = QueryConfig {
            compact_threshold: 20,
            ..QueryConfig::default()
        };
        let lowered = compact_before_request(
            &mut messages,
            &config,
            input(&backend, &route, "context-pass-threshold"),
            None,
            &CancellationToken::new(),
        )
        .await;
        assert!(lowered.compacted, "a threshold of 20 acts on the same size");
        compact::forget_compact_state("context-pass-threshold");
    }

    /// The provider's reported prompt size survives from one user message to
    /// the next.
    ///
    /// Measured against a stub reporting 180k of a 200k window: with the
    /// figure held in a turn-loop local it was zero at every boundary that
    /// mattered, because each user message starts a fresh loop. The threshold
    /// then fell back to the chars/4 estimate of three short strings and
    /// nothing was ever compacted.
    #[tokio::test]
    async fn a_reported_prompt_size_reaches_the_next_prompts_boundary() {
        let session = "context-pass-carry";
        compact::forget_compact_state(session);

        // The turn loop's record, made at the end of one user message.
        compact::record_context_tokens(session, 180_000);

        // Big enough to cut, but only ~16% of the window by the chars/4
        // estimate, so nothing but the reported figure can trigger this.
        let mut messages = a_cuttable_conversation();
        let backend = StubBackend::answering("Short.");

        let pass = compact_before_request(
            &mut messages,
            &QueryConfig::default(),
            input(&backend, &route("claude-opus-4-5"), session),
            None,
            &CancellationToken::new(),
        )
        .await;

        assert!(
            pass.compacted,
            "the reported 180k of a 200k window is over the threshold"
        );
        compact::forget_compact_state(session);
    }

    /// A compaction replaces the recorded size too, or the very next request
    /// would compact the summary it just wrote.
    #[tokio::test]
    async fn compacting_replaces_the_recorded_size() {
        let session = "context-pass-rerecord";
        compact::forget_compact_state(session);
        compact::record_context_tokens(session, 190_000);

        let mut messages = a_cuttable_conversation();
        let backend = StubBackend::answering("Short.");
        let route = route("claude-opus-4-5");
        let input = || input(&backend, &route, session);

        let first = compact_before_request(
            &mut messages,
            &QueryConfig::default(),
            input(),
            None,
            &CancellationToken::new(),
        )
        .await;
        assert!(first.compacted);

        let second = compact_before_request(
            &mut messages,
            &QueryConfig::default(),
            input(),
            None,
            &CancellationToken::new(),
        )
        .await;
        assert!(
            !second.compacted,
            "the stale 190k figure did not survive the compaction"
        );
        compact::forget_compact_state(session);
    }

    /// Booking a turn feeds all three consumers, so a provider arm that
    /// forgets one of them cannot exist: there is only the one call.
    #[test]
    fn booking_a_turn_prices_it_and_records_its_prompt_size() {
        let session = "record-turn-usage";
        compact::forget_compact_state(session);

        let mut msg = Message::assistant("done");
        let usage = mikmik_core::types::UsageInfo {
            input_tokens: 30_000,
            output_tokens: 500,
            cache_read_input_tokens: 120_000,
            cache_creation_input_tokens: 10_000,
        };
        let tracker = mikmik_core::cost::CostTracker::new();

        record_turn_usage(
            &mut msg,
            "claude-opus-4-5",
            mikmik_core::cost::ModelPricing::OPUS,
            &usage,
            &tracker,
            session,
        );

        assert!(msg.cost.is_some(), "the turn is priced on the message");
        assert!(tracker.total_tokens() > 0, "the tracker saw the turn");
        assert_eq!(
            compact::compact_state_for(session).last_context_tokens,
            160_000,
            "input + cache-read + cache-creation, not the output"
        );
        compact::forget_compact_state(session);
    }

    /// Every dispatch path books its turn through that one function.
    ///
    /// The provider arm silently skipped it once already, which is how a
    /// registry-served session came to run uncompacted while the Anthropic one
    /// behaved. There are three mutually exclusive per-turn paths: the Cursor
    /// agent-executor, the general provider registry, and the raw Anthropic
    /// client. A grep is the only thing that can tell the call sites apart
    /// without running a live turn against each.
    #[test]
    fn both_dispatch_arms_book_their_turn() {
        const LOOP_SRC: &str = include_str!("../lib.rs");
        let calls = LOOP_SRC.matches("runner::record_turn_usage(").count();
        assert_eq!(
            calls, 3,
            "the Cursor, general-provider and raw Anthropic paths each book exactly once"
        );
    }

    /// A summariser that fails leaves the conversation whole (#213).
    #[tokio::test]
    async fn a_failed_summary_leaves_the_conversation_intact() {
        compact::forget_compact_state("context-pass-failure");
        let mut messages = a_full_conversation();
        let before = messages.clone();
        let backend = StubBackend::failing();

        let pass = compact_before_request(
            &mut messages,
            &QueryConfig::default(),
            input(&backend, &route("claude-opus-4-5"), "context-pass-failure"),
            None,
            &CancellationToken::new(),
        )
        .await;

        assert!(!pass.compacted);
        assert_eq!(messages.len(), before.len());
        assert_eq!(messages[0].get_all_text(), before[0].get_all_text());
        compact::forget_compact_state("context-pass-failure");
    }

    // ---- the compact model --------------------------------------------------

    #[tokio::test]
    async fn a_chosen_compact_model_writes_the_summary() {
        let session = "compact-model-chosen";
        compact::forget_compact_state(session);
        let mut messages = a_cuttable_conversation();

        let turn_backend = StubBackend::answering("the turn should not write this");
        let compact_backend = StubBackend::answering("Short.");
        let turn = route("big-expensive-model");
        let compact = mikmik_core::config::Config::default()
            .route_for_account("cheap_account", "small-model");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pass = compact_before_request(
            &mut messages,
            &at_a_low_threshold(),
            split_input(
                &turn_backend,
                &turn,
                Some(&compact_backend),
                &compact,
                session,
            ),
            Some(&tx),
            &CancellationToken::new(),
        )
        .await;

        assert!(pass.compacted, "the conversation should have been cut");
        assert_eq!(
            compact_backend.model_seen.lock().as_deref(),
            Some("small-model")
        );
        assert_eq!(
            turn_backend.model_seen.lock().as_deref(),
            None,
            "the turn's model should never have been asked"
        );
        drop(tx);
        let statuses: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                QueryEvent::Status(text) => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            statuses.is_empty(),
            "nothing went wrong, so nothing to report: {statuses:?}"
        );
        compact::forget_compact_state(session);
    }

    #[tokio::test]
    async fn an_unreachable_compact_model_falls_back_and_says_so() {
        let session = "compact-model-unreachable";
        compact::forget_compact_state(session);
        let mut messages = a_cuttable_conversation();

        let turn_backend = StubBackend::answering("Short.");
        let turn = route("big-expensive-model");
        let compact = mikmik_core::config::Config::default()
            .route_for_account("cheap_account", "small-model");

        let (tx, mut rx) = mpsc::unbounded_channel();
        // `None`: the chosen account has no usable credential.
        let pass = compact_before_request(
            &mut messages,
            &at_a_low_threshold(),
            split_input(&turn_backend, &turn, None, &compact, session),
            Some(&tx),
            &CancellationToken::new(),
        )
        .await;

        assert!(
            pass.compacted,
            "a mistyped setting must not stop the context coming down"
        );
        assert_eq!(
            turn_backend.model_seen.lock().as_deref(),
            Some("big-expensive-model")
        );

        drop(tx);
        let told: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                QueryEvent::Status(text) => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            told.iter()
                .any(|text| text.contains("small-model") && text.contains("big-expensive-model")),
            "the user was not told which model wrote the summary: {told:?}"
        );
        compact::forget_compact_state(session);
    }

    #[tokio::test]
    async fn a_compact_model_that_errors_falls_back_to_the_turn() {
        let session = "compact-model-errors";
        compact::forget_compact_state(session);
        let mut messages = a_cuttable_conversation();

        let turn_backend = StubBackend::answering("Short.");
        let compact_backend = StubBackend::failing();
        let turn = route("big-expensive-model");
        let compact = mikmik_core::config::Config::default()
            .route_for_account("cheap_account", "small-model");

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pass = compact_before_request(
            &mut messages,
            &at_a_low_threshold(),
            split_input(
                &turn_backend,
                &turn,
                Some(&compact_backend),
                &compact,
                session,
            ),
            Some(&tx),
            &CancellationToken::new(),
        )
        .await;

        assert!(pass.compacted);
        assert_eq!(
            compact_backend.model_seen.lock().as_deref(),
            Some("small-model"),
            "the chosen model should have been tried first"
        );
        assert_eq!(
            turn_backend.model_seen.lock().as_deref(),
            Some("big-expensive-model")
        );

        drop(tx);
        let told: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|event| match event {
                QueryEvent::Status(text) => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            told.iter().any(|text| text.contains("small-model")),
            "the fallback was silent: {told:?}"
        );
        compact::forget_compact_state(session);
    }
}
