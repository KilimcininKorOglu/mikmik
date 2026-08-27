//! Shared agent runtime owned by the ACP server.
//!
//! Built once on startup and reused for every session. Per-session state
//! (cwd, transcript, cancellation token, permission queue) is layered on
//! top via `sessions::SessionState`.

use std::path::PathBuf;
use std::sync::Arc;

use mikmik_core::config::{Config, Settings};
use mikmik_core::permissions::PermissionManager;
use mikmik_query::QueryConfig;
use mikmik_tools::Tool;

/// An account to log in to.
#[derive(Debug, Clone)]
pub struct LoginRequest {
    /// The provider id the credential belongs to.
    pub provider: String,
    /// Anthropic only: whether to log in through Claude.ai rather than the
    /// Console.
    pub login_with_claude_ai: bool,
    /// A human-friendly name for the resulting account profile.
    pub label: Option<String>,
}

/// Carries out a login and reports what happened, or why it did not.
///
/// The flows open a browser and wait on a loopback redirect, and they live in
/// the binary that owns them, so the runtime is handed one rather than
/// reaching for it. The sender receives the URL as soon as it exists, so a
/// client can show it while the flow is still waiting.
pub type LoginRunner = Arc<
    dyn Fn(
            LoginRequest,
            tokio::sync::mpsc::UnboundedSender<String>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'static>,
        > + Send
        + Sync,
>;

/// Snapshot of the global agent runtime — built at server startup, cloned
/// (cheaply, via Arc) into each session.
#[derive(Clone)]
pub struct AgentRuntime {
    pub config: Config,
    pub settings: Settings,
    pub api_client: Arc<mikmik_api::AnthropicClient>,
    pub provider_registry: Arc<mikmik_api::ProviderRegistry>,
    pub model_registry: Arc<mikmik_api::ModelRegistry>,
    pub tools: Arc<Vec<Box<dyn Tool>>>,
    pub query_config: QueryConfig,
    pub mcp_manager: Option<Arc<mikmik_mcp::McpManager>>,
    pub permission_manager: Arc<std::sync::Mutex<PermissionManager>>,
    pub working_dir: PathBuf,
    /// How `/login` and `/connect` are carried out, when the caller supplied
    /// a way. Without one those commands say so rather than pretending.
    pub login_runner: Option<LoginRunner>,
}

impl AgentRuntime {
    /// Build the runtime from on-disk settings, env vars, and a working
    /// directory. Mirrors the headless startup path but with ACP-specific
    /// defaults (non-interactive, permission decisions routed back to the
    /// connected client).
    pub async fn build(
        working_dir: PathBuf,
        login_runner: Option<LoginRunner>,
    ) -> anyhow::Result<Self> {
        let settings = Settings::load_sync().unwrap_or_default();
        let mut config = settings.effective_config();
        // Plan mode requires interactive UI — fall back to Default so the
        // ACP permission bridge can route decisions to the client.
        if config.permission_mode == mikmik_core::PermissionMode::Plan {
            config.permission_mode = mikmik_core::PermissionMode::Default;
        }
        config.project_dir = Some(working_dir.clone());

        let active_provider = config.selected_provider_id().to_string();
        let (api_key, use_bearer_auth) = if active_provider == "anthropic" {
            config
                .resolve_anthropic_auth_async()
                .await
                .unwrap_or_default()
        } else {
            (String::new(), false)
        };

        let client_config = mikmik_api::client::ClientConfig {
            api_key: api_key.clone(),
            api_base: config.resolve_anthropic_api_base(),
            use_bearer_auth,
            ..Default::default()
        };
        let api_client = Arc::new(mikmik_api::AnthropicClient::new(client_config.clone())?);
        let provider_registry = Arc::new(mikmik_api::ProviderRegistry::from_config(
            &config,
            client_config,
        ));

        let permission_manager = Arc::new(std::sync::Mutex::new(PermissionManager::new(
            config.permission_mode,
            &settings,
        )));

        // MCP servers from settings — connect upfront so their tools are
        // visible to every session. Per-session MCP servers supplied via
        // `session/new` params are additive on top of this (v1: ignored,
        // tracked in plan/migration-todo).
        let mcp_manager = build_mcp_manager(&config, &settings, &working_dir).await;

        // The same roster a terminal session gets, MCP tools included: both
        // read the same settings, so both must end up with the same tools.
        let tools = mikmik_query::build_tool_roster(mcp_manager.clone(), &config, &working_dir);

        // Same catalog the CLI reads, so a session started from an editor
        // resolves the same model as one started from a terminal.
        let model_registry = mikmik_api::model_cache::load_cached_model_registry(&config);

        let mut query_config = QueryConfig::from_config_with_registry(&config, &model_registry);
        query_config.model_registry = Some(model_registry.clone());
        query_config.working_directory = Some(working_dir.display().to_string());
        query_config.workspace_roots = mikmik_core::workspace::generate_root_names(
            &working_dir,
            &config.additional_dirs,
            &config.workspace_paths,
        )
        .into_iter()
        .map(|(name, path)| (name, path.display().to_string()))
        .collect();
        query_config.provider_registry = Some(provider_registry.clone());

        Ok(Self {
            config,
            settings,
            api_client,
            provider_registry,
            model_registry,
            tools,
            query_config,
            mcp_manager,
            permission_manager,
            working_dir,
            login_runner,
        })
    }
}

async fn build_mcp_manager(
    config: &Config,
    settings: &Settings,
    working_dir: &std::path::Path,
) -> Option<Arc<mikmik_mcp::McpManager>> {
    if config.mcp_servers.is_empty() {
        return None;
    }
    // SECURITY (issue #123): never auto-launch project-defined MCP servers in
    // this non-interactive runtime unless they have been trusted. The ACP
    // runtime loads only global settings today (so all servers are user-origin
    // and pass through), but gating here keeps the invariant if project config
    // is ever merged in.
    let project_root = mikmik_core::mcp_trust::project_root_for(working_dir);
    let store = mikmik_core::mcp_trust::McpTrustStore::load();
    let decision = mikmik_core::mcp_trust::partition_mcp_servers(
        &config.mcp_servers,
        project_root.as_deref(),
        settings.trust_project_mcp_servers,
        &std::collections::HashSet::new(),
        &store,
    );
    if !decision.pending.is_empty() {
        let names: Vec<&str> = decision.pending.iter().map(|s| s.name.as_str()).collect();
        tracing::warn!(
            servers = ?names,
            "Skipping untrusted project-defined MCP server(s) in ACP runtime"
        );
    }
    if decision.allowed.is_empty() {
        return None;
    }
    let mgr = Arc::new(mikmik_mcp::McpManager::connect_all(&decision.allowed).await);
    mgr.clone().spawn_notification_poll_loop();
    Some(mgr)
}
