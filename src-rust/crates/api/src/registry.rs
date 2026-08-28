// registry.rs — Registry of all available LLM providers.
//
// Holds an `Arc<dyn LlmProvider>` for each registered provider and exposes
// lookup, health-check, and default-provider helpers.

use std::collections::HashMap;
use std::sync::Arc;

use mikmik_core::ProviderId;

use crate::client::ClientConfig;
use crate::provider::LlmProvider;
use crate::provider_types::ProviderStatus;
use crate::providers::{
    AnthropicProvider, AzureProvider, BedrockProvider, CodexProvider, CohereProvider,
    CopilotProvider, FreeEntry, FreeProvider, GoogleProvider, MinimaxProvider, OpenAiProvider,
    FREE_CATALOG,
};

fn normalize_openai_compat_base(override_base: &str) -> String {
    let trimmed = override_base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{}/v1", trimmed)
    }
}

fn normalize_openai_base(override_base: &str) -> String {
    let trimmed = override_base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.trim_end_matches("/v1").to_string()
    } else {
        trimmed.to_string()
    }
}

fn canonical_local_provider_id(provider_id: &str) -> &str {
    match provider_id {
        "lmstudio" => ProviderId::LM_STUDIO,
        "llamacpp" | "llama-server" => ProviderId::LLAMA_CPP,
        "mlxlm" => ProviderId::MLX_LM,
        _ => provider_id,
    }
}

/// The base URL for `provider_id`, shaped for the implementation that will
/// receive it.
///
/// The shaping follows the account's protocol, not its name. An account the
/// user named matches no vendor id, so keying on the name skipped the step
/// entirely: an `ollama` account called `ev-ollama` reached the provider
/// without the `/v1` its OpenAI-compatible endpoint lives under, and an
/// `openai` one kept a `/v1` that `OpenAiProvider` appends again.
pub fn resolve_provider_api_base(
    config: &mikmik_core::config::Config,
    provider_id: &str,
) -> Option<String> {
    let base = config.resolve_provider_api_base(provider_id)?;
    let vendor = config.vendor_id_for_account(provider_id);
    let protocol = normalize_protocol(&vendor);
    if protocol == ProviderId::OPENAI {
        Some(normalize_openai_base(&base))
    } else if crate::providers::openai_compat_providers::provider_for_id(protocol).is_some() {
        Some(normalize_openai_compat_base(&base))
    } else {
        Some(base)
    }
}

/// Registry of all available LLM providers.
/// Holds `Arc<dyn LlmProvider>` for each registered provider.
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<dyn LlmProvider>>,
    default_provider_id: ProviderId,
}

fn provider_from_key(provider_id: &str, key: String) -> Option<Arc<dyn LlmProvider>> {
    use crate::providers::openai_compat_providers as p;

    if let Some(provider) = p::provider_for_id(provider_id) {
        return Some(Arc::new(provider.with_api_key(key)));
    }

    match provider_id {
        "anthropic" => Some(Arc::new(AnthropicProvider::from_config(ClientConfig {
            api_key: key,
            ..Default::default()
        }))),
        "minimax" => Some(Arc::new(MinimaxProvider::new(key))),
        "openai" => Some(Arc::new(OpenAiProvider::new(key))),
        "google" => Some(Arc::new(GoogleProvider::new(key))),
        "github-copilot" => Some(Arc::new(CopilotProvider::new(key))),
        "codex" | "openai-codex" => {
            // The Codex provider is OAuth-based; the `key` field is not used.
            // Load from the stored token file instead.
            CodexProvider::from_stored().map(|p| Arc::new(p) as Arc<dyn LlmProvider>)
        }
        "cohere" => Some(Arc::new(CohereProvider::new(key))),
        "custom-openai" => Some(Arc::new(p::custom_openai().with_api_key(key))),
        // The base URL comes from settings, so the key alone cannot build it.
        "custom-anthropic" => custom_anthropic_provider(),
        // "free" needs two keys (Zen + OpenRouter) — single-key path doesn't
        // apply.  The auth-store-aware path `runtime_provider_for` handles it.
        "free" => build_free_provider(),
        _ => None,
    }
}

/// Build a [`FreeProvider`] by walking [`FREE_CATALOG`] and pulling any keys
/// the user has stored in the auth store. Each catalog entry whose upstream
/// has a key becomes one link in the fallback chain.
///
/// Returns `None` only if *no* catalog entry has a configured key — a single
/// key is enough to run, and more is better.
pub fn build_free_provider() -> Option<Arc<dyn LlmProvider>> {
    let auth_store = mikmik_core::AuthStore::load();
    let mut chain: Vec<FreeEntry> = Vec::new();

    for upstream in FREE_CATALOG {
        let key = match upstream.id {
            // OpenCode Zen and Go share `OPENCODE_API_KEY`; accept either slot.
            "opencode-zen" => auth_store
                .api_key_for(mikmik_core::ProviderId::OPENCODE_ZEN)
                .or_else(|| auth_store.api_key_for(mikmik_core::ProviderId::OPENCODE_GO)),
            other => auth_store.api_key_for(other),
        }
        .filter(|k| !k.trim().is_empty());

        let Some(key) = key else {
            continue;
        };

        let provider: Option<Arc<dyn LlmProvider>> = match upstream.id {
            "google" => Some(Arc::new(GoogleProvider::new(key))),
            "cohere" => Some(Arc::new(CohereProvider::new(key))),
            id => crate::providers::openai_compat_providers::provider_for_id(id)
                .map(|p| Arc::new(p.with_api_key(key)) as Arc<dyn LlmProvider>),
        };

        if let Some(provider) = provider {
            chain.push(FreeEntry {
                upstream: *upstream,
                provider,
            });
        }
    }

    if chain.is_empty() {
        return None;
    }
    Some(Arc::new(FreeProvider::new(chain)) as Arc<dyn LlmProvider>)
}

/// Build the user-supplied Anthropic-compatible provider from settings.
///
/// Needs a base URL: without one there is nothing to point at, and falling
/// back to the real Anthropic endpoint would make this a confusing duplicate.
/// The key may be empty, because a self-hosted gateway can be unauthenticated.
pub fn custom_anthropic_provider() -> Option<Arc<dyn LlmProvider>> {
    anthropic_account_provider(ProviderId::CUSTOM_ANTHROPIC)
}

/// Build the account `account_id` as an Anthropic-wire-format provider.
///
/// Any account may speak this protocol, not just the one named
/// `custom-anthropic`: a user can register two gateways under names of their
/// own choosing and both are built here.
///
/// An account with a base URL points at that endpoint. One without a base URL
/// is only built when it holds Anthropic OAuth tokens, because then it is a
/// login to the real Anthropic rather than a gateway with nothing to point at.
pub fn anthropic_account_provider(account_id: &str) -> Option<Arc<dyn LlmProvider>> {
    let settings = mikmik_core::config::Settings::load_sync().unwrap_or_default();
    let entry = settings.providers.get(account_id);
    if entry.is_some_and(|provider| !provider.enabled) {
        return None;
    }

    let store = mikmik_core::AuthStore::load();
    let base_url = entry
        .and_then(|provider| provider.api_base.clone())
        .filter(|url| !url.trim().is_empty())
        .or_else(|| {
            mikmik_core::config::api_base_env_var_for_provider(account_id)
                .and_then(|name| std::env::var(name).ok())
        })
        .filter(|url| !url.trim().is_empty());

    // A subscription token is presented as a Bearer; a console account and a
    // gateway both present a key.
    let tokens = store.anthropic_tokens(account_id);
    let use_bearer_auth = tokens.is_some_and(|tokens| tokens.uses_bearer_auth());

    let api_key = entry
        .and_then(|provider| provider.api_key.clone())
        .filter(|key| !key.trim().is_empty())
        .or_else(|| store.api_key_for_protocol(account_id, ProviderId::ANTHROPIC))
        .unwrap_or_default();

    let base_url = match base_url {
        Some(url) => url,
        // No endpoint of its own: only an OAuth login belongs on Anthropic's.
        None if tokens.is_some() => {
            mikmik_core::config::default_api_base_for_provider(ProviderId::ANTHROPIC)?.to_string()
        }
        None => return None,
    };

    Some(Arc::new(AnthropicProvider::from_config_with_id(
        ClientConfig {
            api_key,
            api_base: base_url,
            use_bearer_auth,
            ..Default::default()
        },
        account_id,
    )))
}

/// Every configured account that speaks a non-default protocol, i.e. whose
/// `protocol` field does not simply repeat its own name.
///
/// These are the accounts the built-in per-vendor registration cannot know
/// about, because they are named by the user.
pub fn user_named_accounts() -> Vec<(String, String)> {
    let settings = mikmik_core::config::Settings::load_sync().unwrap_or_default();
    settings
        .providers
        .iter()
        .filter(|(_, entry)| entry.enabled)
        .filter_map(|(id, entry)| {
            let protocol = entry.protocol.as_deref()?.trim();
            (!protocol.is_empty() && protocol != id)
                .then(|| (id.clone(), normalize_protocol(protocol).to_string()))
        })
        .collect()
}

/// Accept the wire-format spellings a user may reasonably write.
fn normalize_protocol(protocol: &str) -> &str {
    match protocol {
        "anthropic-messages" | "messages" => ProviderId::ANTHROPIC,
        "openai-chat" | "chat-completions" => ProviderId::OPENAI,
        other => other,
    }
}

pub fn provider_from_config(
    config: &mikmik_core::config::Config,
    provider_id: &str,
) -> Option<Arc<dyn LlmProvider>> {
    let provider_cfg = config.provider_configs.get(provider_id);
    if provider_cfg.is_some_and(|provider| !provider.enabled) {
        return None;
    }

    let api_key = config.resolve_provider_api_key(provider_id);
    let api_base = resolve_provider_api_base(config, provider_id).filter(|base| !base.is_empty());

    use crate::providers;

    // Dispatch on the wire format, not on the account's name. An account named
    // by the user carries its protocol explicitly; one named after its vendor
    // falls back to that name, which is how every pre-existing settings file
    // keeps resolving to the same implementation it always did.
    let protocol = provider_cfg
        .map(|entry| entry.protocol_or(provider_id))
        .map(|protocol| normalize_protocol(&protocol).to_string())
        .unwrap_or_else(|| provider_id.to_string());

    match protocol.as_str() {
        // The account literally named `anthropic` is served by the pre-built
        // raw client, which the turn loop holds already.
        "anthropic" if provider_id == ProviderId::ANTHROPIC => None,
        // Built from settings rather than the resolved key, because it needs a
        // base URL and speaks the Anthropic wire format, not OpenAI's.
        // A Cloudflare AI Gateway with the Anthropic route speaks the Anthropic
        // wire; the user supplies its templated base URL as `api_base`.
        "anthropic" | "custom-anthropic" | "cloudflare-ai-gateway" => {
            anthropic_account_provider(provider_id)
        }
        // Composite "Free" provider — two keys are pulled internally from the
        // auth store; the `api_key` resolved above is ignored.
        "free" => build_free_provider(),
        "openai" => {
            let mut provider = OpenAiProvider::new(api_key.unwrap_or_default());
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "google" => api_key.map(|key| Arc::new(GoogleProvider::new(key)) as Arc<dyn LlmProvider>),
        "minimax" => api_key.map(|key| {
            let mut provider = MinimaxProvider::new(key);
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            if let Some(service_tier) = provider_cfg
                .and_then(|config| config.options.get("service_tier"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
            {
                provider = provider.with_service_tier(service_tier);
            }
            Arc::new(provider) as Arc<dyn LlmProvider>
        }),
        "azure" => {
            let resource_name = provider_cfg
                .and_then(|provider| provider.options.get("resource_name"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    std::env::var("AZURE_RESOURCE_NAME")
                        .ok()
                        .filter(|value| !value.is_empty())
                });

            match (resource_name, api_key) {
                (Some(resource_name), Some(key)) => {
                    Some(Arc::new(AzureProvider::new(resource_name, key)) as Arc<dyn LlmProvider>)
                }
                _ => None,
            }
        }
        "ollama" => {
            let mut provider = providers::ollama();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "lmstudio" | "lm-studio" => {
            let mut provider = providers::lm_studio();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "llamacpp" | "llama-cpp" | "llama-server" => {
            let mut provider = providers::llama_cpp();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "mlxlm" | "mlx-lm" => {
            let mut provider = providers::mlx_lm();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "deepseek" => {
            let mut provider = providers::deepseek();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "groq" => {
            let mut provider = providers::groq();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "xai" => {
            let mut provider = providers::xai();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "openrouter" => {
            let mut provider = providers::openrouter();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "cohere" => api_key.map(|key| Arc::new(CohereProvider::new(key)) as Arc<dyn LlmProvider>),
        "github-copilot" => {
            api_key.map(|key| Arc::new(CopilotProvider::new(key)) as Arc<dyn LlmProvider>)
        }
        // Read the named account's own tokens. `from_stored` would hand every
        // Codex account the active account's token.
        "codex" | "openai-codex" => CodexProvider::from_account(provider_id)
            .map(|provider| Arc::new(provider) as Arc<dyn LlmProvider>),
        _ => api_key.and_then(|key| provider_from_key(provider_id, key)),
    }
}

pub fn runtime_provider_for(provider_id: &str) -> Option<Arc<dyn LlmProvider>> {
    use crate::providers::openai_compat_providers as p;

    // Local providers never require an API key — build them directly so that
    // the auth-store bypass below doesn't silently drop them.
    // Accept both the hyphenated canonical IDs ("llama-cpp", "lm-studio") and
    // the non-hyphenated aliases ("llamacpp", "lmstudio") used throughout the
    // TUI / connect dialog.
    match provider_id {
        "ollama" => return Some(Arc::new(p::ollama())),
        "lmstudio" | "lm-studio" => return Some(Arc::new(p::lm_studio())),
        // "llama-server" is the binary name for the modern llama.cpp server.
        "llamacpp" | "llama-cpp" | "llama-server" => return Some(Arc::new(p::llama_cpp())),
        "mlxlm" | "mlx-lm" => return Some(Arc::new(p::mlx_lm())),
        "codex" | "openai-codex" => {
            return CodexProvider::from_stored().map(|p| Arc::new(p) as Arc<dyn LlmProvider>);
        }
        // "free" pulls two keys (Zen + OpenRouter) from the auth store and
        // wraps them in a fallback composite — handled here so the generic
        // single-key path below doesn't short-circuit on a missing key.
        "free" => return build_free_provider(),
        _ => {}
    }

    let auth_store = mikmik_core::AuthStore::load();
    let key = auth_store.api_key_for(provider_id)?;
    if key.is_empty() {
        return None;
    }
    provider_from_key(provider_id, key)
}

impl ProviderRegistry {
    /// Create an empty registry with Anthropic as the default provider ID.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            default_provider_id: ProviderId::new(ProviderId::ANTHROPIC),
        }
    }

    /// Register a provider. Returns `&mut self` for builder chaining.
    pub fn register(&mut self, provider: Arc<dyn LlmProvider>) -> &mut Self {
        let id = provider.id().clone();
        self.providers.insert(id, provider);
        self
    }

    /// Register a provider under an account name instead of its vendor id.
    ///
    /// A user-named account is addressed by that name everywhere else:
    /// `"<account>/<model>"`, `settings.provider`, the `/model` picker. Filing
    /// it under the implementation's own id hides it from all three, and two
    /// accounts of the same vendor collapse into one entry.
    pub fn register_as(&mut self, account_id: &str, provider: Arc<dyn LlmProvider>) -> &mut Self {
        self.providers.insert(ProviderId::new(account_id), provider);
        self
    }

    /// Set the default provider by ID.
    ///
    /// # Panics
    /// Panics if no provider with that ID has been registered.
    pub fn set_default(&mut self, id: ProviderId) -> &mut Self {
        let canonical_id = ProviderId::new(canonical_local_provider_id(&id));
        assert!(
            self.providers.contains_key(&canonical_id),
            "set_default: provider '{}' is not registered",
            id,
        );
        self.default_provider_id = canonical_id;
        self
    }

    /// Get a provider by ID.
    pub fn get(&self, id: &ProviderId) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(id).or_else(|| {
            let canonical_id = canonical_local_provider_id(id);
            (canonical_id != &**id)
                .then(|| self.providers.get(&ProviderId::new(canonical_id)))
                .flatten()
        })
    }

    /// Get the default provider.
    pub fn default_provider(&self) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(&self.default_provider_id)
    }

    /// Get the default provider ID.
    pub fn default_provider_id(&self) -> &ProviderId {
        &self.default_provider_id
    }

    /// List all registered provider IDs.
    pub fn provider_ids(&self) -> Vec<&ProviderId> {
        self.providers.keys().collect()
    }

    /// Check health of all providers sequentially.
    /// Returns `(provider_id, status)` pairs.
    pub async fn check_all_health(&self) -> Vec<(ProviderId, ProviderStatus)> {
        let mut results = Vec::new();
        for (id, provider) in &self.providers {
            let status = provider
                .health_check()
                .await
                .unwrap_or(ProviderStatus::Unavailable {
                    reason: "health check failed".to_string(),
                });
            results.push((id.clone(), status));
        }
        results
    }

    /// Convenience: build a registry with just Anthropic registered as the
    /// default provider.  Takes the same [`ClientConfig`] that
    /// [`AnthropicClient`] takes.
    ///
    /// [`AnthropicClient`]: crate::client::AnthropicClient
    pub fn with_anthropic(config: ClientConfig) -> Self {
        let mut registry = Self::new();
        let provider = Arc::new(AnthropicProvider::from_config(config));
        registry.register(provider);
        registry
    }

    pub fn from_config(
        config: &mikmik_core::config::Config,
        anthropic_config: ClientConfig,
    ) -> Self {
        // Apply the user-configured request timeout (issue #175) before any
        // provider HTTP clients are built, so they all honour it. Uses the
        // active provider's resolved value (per-provider override or global).
        crate::set_request_timeout_secs(
            config.resolve_request_timeout_secs(config.selected_provider_id()),
        );
        let mut registry = Self::from_environment_with_auth_store(anthropic_config);
        let active_provider = config.selected_provider_id();

        let mut configured_provider_ids: Vec<String> =
            config.provider_configs.keys().cloned().collect();
        if configured_provider_ids
            .iter()
            .all(|id| id != active_provider)
        {
            configured_provider_ids.push(active_provider.to_string());
        }

        for provider_id in configured_provider_ids {
            if let Some(provider) = provider_from_config(config, &provider_id) {
                registry.register_as(&provider_id, provider);
            }
        }

        let default_provider_id = ProviderId::new(active_provider);
        if registry.get(&default_provider_id).is_some() {
            registry.set_default(default_provider_id);
        }

        registry
    }

    /// Register [`GoogleProvider`] if `GOOGLE_API_KEY` or
    /// `GOOGLE_GENERATIVE_AI_API_KEY` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_google_if_key_set(&mut self) -> &mut Self {
        let key = std::env::var("GOOGLE_API_KEY")
            .or_else(|_| std::env::var("GOOGLE_GENERATIVE_AI_API_KEY"));
        if let Ok(key) = key {
            let provider = Arc::new(GoogleProvider::new(key));
            self.register(provider);
        }
        self
    }

    /// Register [`OpenAiProvider`] if `OPENAI_API_KEY` is set in the
    /// environment.  Returns `&mut self` for builder chaining.
    pub fn with_openai_if_key_set(&mut self) -> &mut Self {
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            let provider = Arc::new(OpenAiProvider::new(key));
            self.register(provider);
        }
        self
    }

    /// Register [`AzureProvider`] if `AZURE_API_KEY` and `AZURE_RESOURCE_NAME`
    /// are set in the environment.  Returns `&mut self` for builder chaining.
    pub fn with_azure_if_configured(&mut self) -> &mut Self {
        if let Some(p) = AzureProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`BedrockProvider`] if AWS credentials are available in the
    /// environment (`AWS_ACCESS_KEY_ID`+`AWS_SECRET_ACCESS_KEY` or
    /// `AWS_BEARER_TOKEN_BEDROCK`).  Returns `&mut self` for builder chaining.
    pub fn with_bedrock_if_configured(&mut self) -> &mut Self {
        if let Some(p) = BedrockProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CopilotProvider`] if `GITHUB_TOKEN` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_copilot_if_configured(&mut self) -> &mut Self {
        if let Some(p) = CopilotProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CodexProvider`] if stored Codex OAuth tokens are available in
    /// `~/.config/mikmik/codex_tokens.json`.  Returns `&mut self` for builder chaining.
    pub fn with_codex_if_configured(&mut self) -> &mut Self {
        if let Some(p) = CodexProvider::from_stored() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CohereProvider`] if `COHERE_API_KEY` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_cohere_if_key_set(&mut self) -> &mut Self {
        if let Some(p) = CohereProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register the user-supplied Anthropic-compatible endpoint when one is
    /// configured.
    ///
    /// Registered under its own id so it sits alongside the real Anthropic
    /// provider; overriding `providers.anthropic.api_base` would replace it.
    pub fn with_custom_anthropic_if_configured(&mut self) -> &mut Self {
        if let Some(provider) = custom_anthropic_provider() {
            self.register(provider);
        }
        self
    }

    /// Build a registry with **all** providers that have credentials configured
    /// in the environment.  Anthropic is always the default provider.
    ///
    /// This is the recommended constructor for production use.
    pub fn from_environment(anthropic_config: ClientConfig) -> Self {
        let mut registry = Self::with_anthropic(anthropic_config);
        registry
            .with_openai_if_key_set()
            .with_google_if_key_set()
            .with_azure_if_configured()
            .with_bedrock_if_configured()
            .with_copilot_if_configured()
            .with_codex_if_configured()
            .with_cohere_if_key_set()
            .with_custom_anthropic_if_configured()
            .with_available_providers();
        registry
    }

    /// Build a registry that checks **both** environment variables and the
    /// persistent [`AuthStore`] (`~/.config/mikmik/auth.json`) for credentials.
    ///
    /// This ensures that API keys stored via `/connect` or `mikmik auth` are
    /// picked up at startup, not just env vars.  Falls back to
    /// `from_environment` for providers that only support env-var config, and
    /// adds any extra providers that have keys in the auth store.
    ///
    /// [`AuthStore`]: mikmik_core::AuthStore
    pub fn from_environment_with_auth_store(anthropic_config: ClientConfig) -> Self {
        // Start with env-based registration.
        let mut registry = Self::from_environment(anthropic_config);

        // Now check the auth store for providers that weren't registered from
        // env vars.
        let auth_store = mikmik_core::AuthStore::load();

        for provider_id in auth_store.credentials.keys() {
            let pid = mikmik_core::ProviderId::new(provider_id.as_str());
            // Skip if already registered from env vars.
            if registry.get(&pid).is_some() {
                continue;
            }
            // Try to get a usable key from the auth store.
            if let Some(key) = auth_store.api_key_for(provider_id) {
                if key.is_empty() {
                    continue;
                }
                let provider = provider_from_key(provider_id, key);
                if let Some(p) = provider {
                    registry.register(p);
                }
            }
        }

        registry
    }

    /// Register all providers that have environment variable credentials set.
    ///
    /// Local providers (Ollama, LM Studio, llama.cpp) are always registered
    /// regardless of credentials — `health_check()` will report them as
    /// unavailable if the server is not running.
    ///
    /// Remote API-key providers are only registered when their respective
    /// environment variables are set (non-empty).
    ///
    /// Returns `&mut self` for builder chaining.
    pub fn with_available_providers(&mut self) -> &mut Self {
        use crate::providers::openai_compat_providers as p;

        // Accounts the user named themselves. Registered first so a later
        // per-vendor registration cannot claim the same id, and so an account
        // pointing at a gateway is reachable by the name the user gave it.
        //
        // Built through `provider_from_config` rather than by hand, so every
        // protocol it already knows works here too. Hand-rolling one protocol
        // silently dropped the rest, and an account that is never registered
        // cannot be asked what models it serves.
        let account_config = mikmik_core::config::Settings::load_sync()
            .unwrap_or_default()
            .effective_config();
        for (account_id, _protocol) in user_named_accounts() {
            if let Some(provider) = provider_from_config(&account_config, &account_id) {
                self.register(provider);
            }
        }

        // Local providers — always try to register.
        self.register(Arc::new(p::ollama()));
        self.register(Arc::new(p::lm_studio()));
        self.register(Arc::new(p::llama_cpp()));
        self.register(Arc::new(p::mlx_lm()));

        // Remote providers — only register when an API key is present.
        if std::env::var("DEEPSEEK_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::deepseek()));
        }
        if std::env::var("GROQ_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::groq()));
        }
        if std::env::var("XAI_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::xai()));
        }
        if std::env::var("OPENROUTER_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::openrouter()));
        }
        if std::env::var("TOGETHER_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::together_ai()));
        }
        if std::env::var("PERPLEXITY_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::perplexity()));
        }
        if std::env::var("CEREBRAS_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::cerebras()));
        }
        if std::env::var("DEEPINFRA_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::deepinfra()));
        }
        if std::env::var("VENICE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::venice()));
        }
        if std::env::var("DASHSCOPE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::qwen()));
        }
        if std::env::var("MISTRAL_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::mistral()));
        }
        if std::env::var("SAMBANOVA_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::sambanova()));
        }
        if std::env::var("HF_TOKEN")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::huggingface()));
        }
        if std::env::var("MINIMAX_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            let key = std::env::var("MINIMAX_API_KEY").unwrap_or_default();
            self.register(Arc::new(MinimaxProvider::new(key)));
        }
        if std::env::var("NVIDIA_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::nvidia()));
        }
        if std::env::var("SILICONFLOW_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::siliconflow()));
        }
        if std::env::var("MOONSHOT_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::moonshot()));
        }
        if std::env::var("ZHIPU_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::zhipu()));
        }
        if std::env::var("ZAI_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::zai()));
        }
        if std::env::var("NEBIUS_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::nebius()));
        }
        if std::env::var("NOVITA_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::novita()));
        }
        if std::env::var("OVHCLOUD_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::ovhcloud()));
        }
        if std::env::var("SCALEWAY_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::scaleway()));
        }
        if std::env::var("VULTR_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::vultr_ai()));
        }
        if std::env::var("BASETEN_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::baseten()));
        }
        if std::env::var("FRIENDLI_TOKEN")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::friendli()));
        }
        if std::env::var("UPSTAGE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::upstage()));
        }
        if std::env::var("STEPFUN_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::stepfun()));
        }
        if std::env::var("FIREWORKS_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::fireworks()));
        }
        if std::env::var("OPENCODE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::opencode_go()));
        }
        self
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Every spelling a provider is registered under, so a lookup by the id the
/// user typed still finds it. `ProviderRegistry::get` already canonicalises
/// local runtimes; this covers the hyphenation variants of hosted providers.
pub fn provider_lookup_ids(provider_id: &str) -> Vec<&str> {
    match provider_id {
        "togetherai" | "together-ai" => vec!["togetherai", "together-ai"],
        "lmstudio" | "lm-studio" => vec!["lmstudio", "lm-studio"],
        "llamacpp" | "llama-cpp" | "llama-server" => {
            vec!["llamacpp", "llama-cpp", "llama-server"]
        }
        "mlxlm" | "mlx-lm" => vec!["mlxlm", "mlx-lm"],
        "moonshot" | "moonshotai" => vec!["moonshot", "moonshotai"],
        "zhipu" | "zhipuai" => vec!["zhipu", "zhipuai"],
        "vultr" | "vultr-ai" => vec!["vultr", "vultr-ai"],
        "google" | "google-vertex" => vec!["google", "google-vertex"],
        _ => vec![provider_id],
    }
}

/// Build a registry from `config` and hand back the provider registered under
/// `provider_id`, or `None` when that provider has no usable credentials.
///
/// Anthropic auth is resolved first because the registry needs it to construct
/// the Anthropic client even when the caller wants a different provider.
pub async fn provider_by_id(
    config: &mikmik_core::config::Config,
    provider_id: &str,
) -> Option<Arc<dyn LlmProvider>> {
    let anthropic_auth = config.resolve_anthropic_auth_async().await;
    let registry = ProviderRegistry::from_config(
        config,
        crate::client::ClientConfig {
            api_key: anthropic_auth
                .as_ref()
                .map(|(credential, _)| credential.clone())
                .unwrap_or_default(),
            api_base: config.resolve_anthropic_api_base(),
            use_bearer_auth: anthropic_auth
                .as_ref()
                .is_some_and(|(_, use_bearer)| *use_bearer),
            ..Default::default()
        },
    );

    provider_lookup_ids(provider_id)
        .into_iter()
        .find_map(|lookup_id| registry.get(&ProviderId::new(lookup_id)).cloned())
}

/// Why a provider could not be resolved.
///
/// `provider_by_id` collapses every failure into `None`. Naming an account
/// adds failures the caller has to tell apart, because "this provider has no
/// accounts" and "this account does not exist" call for different advice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderResolveError {
    /// No account of that name is stored, or it has no usable credentials.
    AccountNotFound {
        account_id: String,
        available: Vec<String>,
    },
    /// The account is stored but its credential is missing or unusable.
    AccountCredentialsMissing { account_id: String },
}

impl std::fmt::Display for ProviderResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccountNotFound {
                account_id,
                available,
            } => {
                write!(f, "no account named '{account_id}'")?;
                if available.is_empty() {
                    write!(f, "; none are stored")
                } else {
                    write!(f, "; stored accounts: {}", available.join(", "))
                }
            }
            Self::AccountCredentialsMissing { account_id } => {
                write!(f, "the account '{account_id}' has no usable credentials")
            }
        }
    }
}

impl std::error::Error for ProviderResolveError {}

/// Hand back the provider for one named account.
///
/// An OAuth account is built directly from its stored tokens rather than
/// through the registry, because that path refreshes an expired token and
/// authenticates as this account instead of the active one. Everything else
/// goes through the ordinary lookup.
pub async fn provider_for_account(
    config: &mikmik_core::config::Config,
    account_id: &str,
) -> Result<Arc<dyn LlmProvider>, ProviderResolveError> {
    let store = mikmik_core::AuthStore::load();
    let missing = || ProviderResolveError::AccountCredentialsMissing {
        account_id: account_id.to_string(),
    };

    if store.anthropic_tokens(account_id).is_some() {
        let (credential, use_bearer_auth) =
            mikmik_core::oauth::resolve_auth_for_account(account_id)
                .await
                .ok_or_else(missing)?;
        return Ok(Arc::new(AnthropicProvider::from_config(
            crate::client::ClientConfig {
                api_key: credential,
                api_base: config
                    .resolve_provider_api_base(account_id)
                    .unwrap_or_else(|| config.resolve_anthropic_api_base()),
                use_bearer_auth,
                ..Default::default()
            },
        )));
    }

    if store.codex_tokens(account_id).is_some() {
        return CodexProvider::from_account(account_id)
            .map(|provider| Arc::new(provider) as Arc<dyn LlmProvider>)
            .ok_or_else(missing);
    }

    provider_by_id(config, account_id).await.ok_or_else(|| {
        let mut available: Vec<String> = store.credentials.keys().cloned().collect();
        available.sort();
        ProviderResolveError::AccountNotFound {
            account_id: account_id.to_string(),
            available,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers;

    #[test]
    fn local_provider_aliases_resolve_to_canonical_registrations() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(providers::lm_studio()));
        registry.register(Arc::new(providers::llama_cpp()));
        registry.register(Arc::new(providers::mlx_lm()));

        let lm_studio = registry
            .get(&ProviderId::new("lmstudio"))
            .expect("lmstudio alias should resolve");
        let llama_cpp = registry
            .get(&ProviderId::new("llamacpp"))
            .expect("llamacpp alias should resolve");
        let mlx_lm = registry
            .get(&ProviderId::new("mlxlm"))
            .expect("mlxlm alias should resolve");

        assert_eq!(&**lm_studio.id(), ProviderId::LM_STUDIO);
        assert_eq!(&**llama_cpp.id(), ProviderId::LLAMA_CPP);
        assert_eq!(&**mlx_lm.id(), ProviderId::MLX_LM);
    }

    #[test]
    fn alias_can_select_canonical_default_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(providers::lm_studio()));
        registry.set_default(ProviderId::new("lmstudio"));

        assert_eq!(&**registry.default_provider_id(), ProviderId::LM_STUDIO);
    }

    /// A config carrying one account, as `settings.json` would define it.
    fn config_with_account(
        account: &str,
        protocol: Option<&str>,
        api_base: &str,
    ) -> mikmik_core::config::Config {
        let mut config = mikmik_core::config::Config::default();
        config.provider_configs.insert(
            account.to_string(),
            mikmik_core::config::ProviderConfig {
                api_base: Some(api_base.to_string()),
                protocol: protocol.map(str::to_string),
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn a_vendor_named_openai_compat_account_gets_the_v1_suffix() {
        let config = config_with_account("ollama", None, "http://192.0.2.10:11434");
        assert_eq!(
            resolve_provider_api_base(&config, "ollama").as_deref(),
            Some("http://192.0.2.10:11434/v1")
        );
    }

    #[test]
    fn a_user_named_account_is_shaped_by_its_protocol() {
        // The account matches no vendor id, so only its `protocol` says the
        // endpoint lives under /v1.
        let config = config_with_account("ev-ollama", Some("ollama"), "http://192.0.2.10:11434");
        assert_eq!(
            resolve_provider_api_base(&config, "ev-ollama").as_deref(),
            Some("http://192.0.2.10:11434/v1")
        );
    }

    #[test]
    fn a_base_that_already_ends_in_v1_is_left_alone() {
        let config =
            config_with_account("ev-ollama", Some("ollama"), "http://192.0.2.10:11434/v1/");
        assert_eq!(
            resolve_provider_api_base(&config, "ev-ollama").as_deref(),
            Some("http://192.0.2.10:11434/v1")
        );
    }

    #[test]
    fn a_user_named_openai_account_loses_the_v1_the_provider_appends() {
        // OpenAiProvider builds "{base}/v1/chat/completions" itself, so a base
        // that keeps its own /v1 would send the segment twice.
        let config = config_with_account("gw", Some("openai"), "https://gw.example/v1");
        assert_eq!(
            resolve_provider_api_base(&config, "gw").as_deref(),
            Some("https://gw.example")
        );
    }

    #[test]
    fn an_account_speaking_a_wire_format_alias_is_shaped_like_openai() {
        let config = config_with_account("gw", Some("chat-completions"), "https://gw.example/v1");
        assert_eq!(
            resolve_provider_api_base(&config, "gw").as_deref(),
            Some("https://gw.example")
        );
    }

    #[test]
    fn an_anthropic_account_keeps_its_base_url_untouched() {
        let config = config_with_account("gw", Some("anthropic"), "https://gw.example/anthropic");
        assert_eq!(
            resolve_provider_api_base(&config, "gw").as_deref(),
            Some("https://gw.example/anthropic")
        );
    }
}

#[cfg(test)]
mod profile_resolution_tests {
    //! Naming an account must fail with a reason the caller can act on, and it
    //! must never fall back to the active account.
    use super::*;
    const PROVIDER_ANTHROPIC: &str = ProviderId::ANTHROPIC;
    // Held across awaits, so it has to be the async-aware lock.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    /// Store an Anthropic account holding `access_token`.
    ///
    /// An account exists exactly when it holds a credential, so there is no
    /// registration step separate from storing one.
    fn register(id: &str, access_token: &str) {
        let mut store = mikmik_core::AuthStore::load();
        store.set_anthropic_tokens(
            id,
            mikmik_core::oauth::OAuthTokens {
                access_token: access_token.to_string(),
                scopes: vec![mikmik_core::oauth::CLAUDE_AI_INFERENCE_SCOPE.to_string()],
                ..Default::default()
            },
        );
    }

    #[tokio::test]
    async fn a_provider_without_accounts_rejects_a_profile() {
        let _lock = ENV_LOCK.lock().await;
        let _home = HomeGuard::new();
        let config = mikmik_core::config::Config::default();

        let error = provider_for_account(&config, "personal")
            .await
            .err()
            .expect("no such openai account");

        match error {
            ProviderResolveError::AccountNotFound { available, .. } => {
                assert!(available.is_empty(), "nothing is stored for openai");
            }
            other => panic!("expected ProfileNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_account_lists_the_stored_ones() {
        let _lock = ENV_LOCK.lock().await;
        let _home = HomeGuard::new();
        register("work", "work-token");
        register("personal", "personal-token");
        let config = mikmik_core::config::Config::default();

        let error = provider_for_account(&config, "missing")
            .await
            .err()
            .expect("no such account");

        match error {
            ProviderResolveError::AccountNotFound { available, .. } => {
                assert_eq!(available, vec!["personal".to_string(), "work".to_string()]);
            }
            other => panic!("expected ProfileNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_account_whose_credential_presents_nothing_is_reported_as_such() {
        let _lock = ENV_LOCK.lock().await;
        let _home = HomeGuard::new();
        // Stored, but the token is empty, so there is nothing to present.
        register("work", "");
        let config = mikmik_core::config::Config::default();

        let error = provider_for_account(&config, "work")
            .await
            .err()
            .expect("no usable credential");

        assert_eq!(
            error,
            ProviderResolveError::AccountCredentialsMissing {
                account_id: "work".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn an_account_resolves_to_a_provider_carrying_its_own_credential() {
        let _lock = ENV_LOCK.lock().await;
        let _home = HomeGuard::new();
        register("personal", "personal-token");
        let config = mikmik_core::config::Config::default();

        let provider = provider_for_account(&config, "personal")
            .await
            .expect("resolves");

        assert_eq!(provider.id(), &ProviderId::new(PROVIDER_ANTHROPIC));
    }

    #[tokio::test]
    async fn an_unknown_name_is_an_unknown_account() {
        let _lock = ENV_LOCK.lock().await;
        let _home = HomeGuard::new();
        let config = mikmik_core::config::Config::default();

        // There is one kind of failure now, because there is one kind of
        // thing to name.
        let error = provider_for_account(&config, "not-a-provider")
            .await
            .err()
            .expect("unknown account");

        assert_eq!(
            error,
            ProviderResolveError::AccountNotFound {
                account_id: "not-a-provider".to_string(),
                available: Vec::new(),
            }
        );
    }
}

#[cfg(test)]
mod custom_anthropic_tests {
    //! `custom-anthropic` is a second Anthropic-format endpoint. It has to sit
    //! next to the real provider rather than replace it, which is what
    //! overriding `providers.anthropic.api_base` would do.
    use super::*;
    use std::sync::Mutex as StdMutex;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    struct HomeGuard {
        saved_home: Option<std::ffi::OsString>,
        saved_base: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl HomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let saved_home = std::env::var_os("MIKMIK_HOME");
            let saved_base = std::env::var_os("CUSTOM_ANTHROPIC_BASE_URL");
            std::env::set_var("MIKMIK_HOME", dir.path());
            std::env::remove_var("CUSTOM_ANTHROPIC_BASE_URL");
            Self {
                saved_home,
                saved_base,
                _dir: dir,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.saved_home {
                Some(value) => std::env::set_var("MIKMIK_HOME", value),
                None => std::env::remove_var("MIKMIK_HOME"),
            }
            match &self.saved_base {
                Some(value) => std::env::set_var("CUSTOM_ANTHROPIC_BASE_URL", value),
                None => std::env::remove_var("CUSTOM_ANTHROPIC_BASE_URL"),
            }
        }
    }

    fn write_settings(body: &str) {
        let path = mikmik_core::config::Settings::global_settings_path();
        std::fs::create_dir_all(path.parent().expect("settings dir")).expect("mkdir");
        std::fs::write(&path, body).expect("write settings");
    }

    #[test]
    fn without_a_base_url_there_is_nothing_to_register() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _home = HomeGuard::new();

        assert!(
            custom_anthropic_provider().is_none(),
            "falling back to the real Anthropic endpoint would be a confusing duplicate"
        );
    }

    #[test]
    fn a_base_url_in_settings_builds_the_provider() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _home = HomeGuard::new();
        write_settings(
            r#"{"providers":{"custom-anthropic":{"api_base":"https://gateway.example/v1"}}}"#,
        );

        let provider = custom_anthropic_provider().expect("configured");

        assert_eq!(
            provider.id(),
            &ProviderId::new(ProviderId::CUSTOM_ANTHROPIC),
            "registering under the anthropic id would replace the real provider"
        );
    }

    #[test]
    fn an_env_base_url_is_accepted_without_settings() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _home = HomeGuard::new();
        std::env::set_var("CUSTOM_ANTHROPIC_BASE_URL", "https://gateway.example/v1");

        assert!(custom_anthropic_provider().is_some());
    }

    #[test]
    fn a_disabled_entry_is_skipped() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _home = HomeGuard::new();
        write_settings(
            r#"{"providers":{"custom-anthropic":{"api_base":"https://gateway.example/v1","enabled":false}}}"#,
        );

        assert!(custom_anthropic_provider().is_none());
    }

    #[test]
    fn it_registers_alongside_the_real_anthropic_provider() {
        let _lock = ENV_LOCK.lock().expect("lock");
        let _home = HomeGuard::new();
        write_settings(
            r#"{"providers":{"custom-anthropic":{"api_base":"https://gateway.example/v1"}}}"#,
        );

        let mut registry = ProviderRegistry::with_anthropic(ClientConfig::default());
        registry.with_custom_anthropic_if_configured();

        assert!(registry
            .get(&ProviderId::new(ProviderId::ANTHROPIC))
            .is_some());
        assert!(registry
            .get(&ProviderId::new(ProviderId::CUSTOM_ANTHROPIC))
            .is_some());
    }
}

#[cfg(test)]
mod account_registration_tests {
    //! An account is addressed by its own name. The registry has to key on
    //! that name, or the `/model` picker and `"<account>/<model>"` routing
    //! never see accounts whose implementation reports a vendor id.
    use super::*;
    use mikmik_core::config::{Config, ProviderConfig};
    use std::sync::Mutex as StdMutex;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

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

    fn account(protocol: &str) -> ProviderConfig {
        ProviderConfig {
            protocol: Some(protocol.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_codex_account_is_filed_under_its_own_name() {
        let lock = ENV_LOCK.lock();
        let _lock = match lock {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        let tokens = mikmik_core::oauth_config::CodexTokens {
            access_token: "codex-token".to_string(),
            ..Default::default()
        };
        mikmik_core::oauth_config::save_codex_tokens_for_account(&tokens, "chatgpt")
            .expect("store tokens");

        let mut config = Config::default();
        config
            .provider_configs
            .insert("chatgpt".to_string(), account("codex"));

        let registry = ProviderRegistry::from_config(&config, ClientConfig::default());

        assert!(
            registry.get(&ProviderId::new("chatgpt")).is_some(),
            "the account name must be the key, got {:?}",
            registry.provider_ids()
        );
    }

    #[test]
    fn two_accounts_of_one_vendor_stay_separate() {
        let lock = ENV_LOCK.lock();
        let _lock = match lock {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _home = HomeGuard::new();

        let mut store = mikmik_core::AuthStore::load();
        for (account_id, key) in [("day-job", "gho_day"), ("side-project", "gho_side")] {
            store.set(
                account_id,
                mikmik_core::auth_store::StoredCredential::ApiKey {
                    key: key.to_string(),
                },
            );
        }

        let mut config = Config::default();
        config
            .provider_configs
            .insert("day-job".to_string(), account("github-copilot"));
        config
            .provider_configs
            .insert("side-project".to_string(), account("github-copilot"));

        let registry = ProviderRegistry::from_config(&config, ClientConfig::default());

        assert!(registry.get(&ProviderId::new("day-job")).is_some());
        assert!(registry.get(&ProviderId::new("side-project")).is_some());
    }
}
