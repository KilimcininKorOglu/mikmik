// cc-bridge: Remote control bridge implementation.
//
// The bridge connects the local MikMik CLI to the claude.ai web UI,
// enabling mobile/web-initiated sessions. This module implements:
//
// - Bridge configuration management (env-var and defaults)
// - Device fingerprinting for trusted-device identification
// - JWT decode/expiry utilities (client-side, no signature verification)
// - Session lifecycle (register, poll, upload events, deregister)
// - Message and event protocol types for bidirectional communication
// - Long-polling loop with exponential backoff and cancellation
// - Public `start_bridge` API that spawns background task and returns channels
//
// Architecture mirrors the TypeScript bridge (bridgeMain.ts / bridgeApi.ts),
// adapted to idiomatic Rust async with tokio channels and reqwest.

#![warn(clippy::all)]

use anyhow::Context;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use mikmik_core::timeline::TimelineRow;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// JWT utilities
// ---------------------------------------------------------------------------

/// Decoded claims from a session-ingress JWT.
///
/// Parsed client-side without signature verification — used only for
/// expiry checks and display, never for authorization decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Subject (usually user / device identifier).
    pub sub: Option<String>,
    /// Expiry Unix timestamp (seconds).
    pub exp: Option<i64>,
    /// Issued-at Unix timestamp (seconds).
    pub iat: Option<i64>,
    /// Trusted-device identifier embedded by the server.
    pub device_id: Option<String>,
    /// Session identifier embedded by the server.
    pub session_id: Option<String>,
}

impl JwtClaims {
    /// Decode a JWT payload segment without verifying the signature.
    ///
    /// Strips the `sk-ant-si-` session-ingress prefix if present, then
    /// base64url-decodes the second `.`-separated segment and JSON-parses it.
    /// Returns an error if the token is malformed or the JSON is invalid.
    pub fn decode(token: &str) -> anyhow::Result<Self> {
        // Strip session-ingress prefix used by Anthropic's ingress tokens.
        let jwt = token.strip_prefix("sk-ant-si-").unwrap_or(token);

        let parts: Vec<&str> = jwt.split('.').collect();
        if parts.len() < 2 {
            anyhow::bail!("Invalid JWT: expected at least 2 dot-separated segments");
        }

        let raw = URL_SAFE_NO_PAD
            .decode(parts[1])
            .context("JWT payload is not valid base64url")?;

        serde_json::from_slice::<Self>(&raw)
            .context("JWT payload is not valid JSON matching JwtClaims")
    }

    /// Returns `true` if the `exp` claim is in the past.
    ///
    /// When `exp` is absent the token is treated as non-expired (permissive
    /// default), matching the TypeScript behaviour in `jwtUtils.ts`.
    pub fn is_expired(&self) -> bool {
        if let Some(exp) = self.exp {
            let now = chrono::Utc::now().timestamp();
            exp < now
        } else {
            false
        }
    }

    /// Remaining lifetime in seconds, or `None` if no `exp` claim or already
    /// expired.
    pub fn remaining_secs(&self) -> Option<i64> {
        let exp = self.exp?;
        let now = chrono::Utc::now().timestamp();
        let diff = exp - now;
        if diff > 0 {
            Some(diff)
        } else {
            None
        }
    }
}

/// Decode just the expiry timestamp from a raw JWT string.
/// Returns `None` if the token is malformed or has no `exp` claim.
pub fn decode_jwt_expiry(token: &str) -> Option<i64> {
    JwtClaims::decode(token).ok()?.exp
}

/// Returns `true` if the token is expired (or unparseable).
pub fn jwt_is_expired(token: &str) -> bool {
    JwtClaims::decode(token)
        .map(|c| c.is_expired())
        .unwrap_or(true)
}

// ---------------------------------------------------------------------------
// Device fingerprint
// ---------------------------------------------------------------------------

/// This machine's hostname, when the system will tell us.
///
/// Used as the default session label so a remote client lists something a
/// person recognises.
pub fn machine_hostname() -> Option<String> {
    hostname::get()
        .ok()
        .map(|host| host.to_string_lossy().into_owned())
        .filter(|host| !host.is_empty())
}

/// Compute a stable device fingerprint from machine-local information.
///
/// Combines hostname, login user name, and home directory path, then SHA-256
/// hashes them and returns the full hex digest. Matching the TypeScript
/// `trustedDevice.ts` algorithm so fingerprints are consistent across the
/// two implementations.
pub fn device_fingerprint() -> String {
    let mut input = String::with_capacity(128);

    if let Ok(host) = hostname::get() {
        input.push_str(&host.to_string_lossy());
    }
    input.push(':');

    if let Ok(user) = std::env::var("USER").or_else(|_| std::env::var("USERNAME")) {
        input.push_str(&user);
    }
    input.push(':');

    if let Some(home) = dirs::home_dir() {
        input.push_str(&home.display().to_string());
    }

    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Bridge configuration
// ---------------------------------------------------------------------------

/// Runtime configuration for the bridge subsystem.
///
/// Built either from env vars via [`BridgeConfig::from_env`] or manually
/// by the caller. The bridge is only active when both `enabled` is `true`
/// **and** a `session_token` is present (see [`BridgeConfig::is_active`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// Whether the bridge feature is turned on.
    pub enabled: bool,
    /// Base URL for bridge API calls (e.g. `https://claude.ai`).
    pub server_url: String,
    /// Stable device identifier (SHA-256 fingerprint or custom value).
    pub device_id: String,
    /// Bearer token (OAuth access token or session-ingress JWT).
    pub session_token: Option<String>,
    /// How long to wait between poll cycles (milliseconds).
    pub polling_interval_ms: u64,
    /// Maximum successive failed polls before the loop gives up.
    pub max_reconnect_attempts: u32,
    /// Per-session inactivity timeout in milliseconds (default 24 h).
    pub session_timeout_ms: u64,
    /// Runner version string sent on API calls for server-side diagnostics.
    pub runner_version: String,
    /// Human-readable name for this machine, sent at registration.
    ///
    /// Without it a remote client can only list opaque session ids, which
    /// makes picking between two open sessions guesswork.
    pub label: Option<String>,
    /// Working directory of the session, used as a fallback label.
    pub cwd: Option<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: "https://claude.ai".to_string(),
            device_id: device_fingerprint(),
            session_token: None,
            polling_interval_ms: 1_000,
            max_reconnect_attempts: 10,
            session_timeout_ms: 24 * 60 * 60 * 1_000,
            runner_version: env!("CARGO_PKG_VERSION").to_string(),
            label: None,
            cwd: None,
        }
    }
}

impl BridgeConfig {
    /// Build config from environment variables.
    ///
    /// Recognised variables:
    /// - `MIKMIK_BRIDGE_URL` — overrides `server_url` and sets `enabled = true`
    /// - `MIKMIK_BRIDGE_TOKEN` / `CLAUDE_BRIDGE_OAUTH_TOKEN` — sets `session_token`
    /// - `CLAUDE_BRIDGE_BASE_URL` — alternative URL override (ant-only dev override)
    pub fn from_env() -> Self {
        let mut config = Self::default();

        // URL override (sets enabled implicitly)
        if let Ok(url) =
            std::env::var("MIKMIK_BRIDGE_URL").or_else(|_| std::env::var("CLAUDE_BRIDGE_BASE_URL"))
        {
            if !url.is_empty() {
                config.server_url = url;
                config.enabled = true;
            }
        }

        // Token override
        if let Ok(token) = std::env::var("MIKMIK_BRIDGE_TOKEN")
            .or_else(|_| std::env::var("CLAUDE_BRIDGE_OAUTH_TOKEN"))
        {
            if !token.is_empty() {
                config.session_token = Some(token);
            }
        }

        config
    }

    /// Returns `true` only when the bridge is both enabled and has a token.
    pub fn is_active(&self) -> bool {
        self.enabled && self.session_token.is_some()
    }

    /// Validate that a server-provided ID is safe to interpolate into a URL
    /// path segment. Prevents path traversal (e.g. `../../admin`).
    ///
    /// Mirrors `validateBridgeId()` in `bridgeApi.ts`.
    pub fn validate_id<'a>(id: &'a str, label: &str) -> anyhow::Result<&'a str> {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9_-]+$").unwrap());
        if id.is_empty() || !re.is_match(id) {
            anyhow::bail!("Invalid {}: contains unsafe characters", label);
        }
        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// Permission decision
// ---------------------------------------------------------------------------

/// A tool-use permission decision sent by the web UI back to the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    AllowPermanently,
    Deny,
    DenyPermanently,
}

/// What a client decided about a project-defined MCP server.
///
/// Mirrors `mikmik_tui::dialogs::McpApprovalChoice`, which this crate cannot
/// name: `tui` and `bridge` are siblings. The CLI translates between the two,
/// the same way it does for `PermissionDecision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpApprovalDecision {
    /// Run the server for this session only.
    AllowSession,
    /// Persist the approval so it survives a restart.
    AllowAlways,
    /// Leave the server unlaunched.
    Deny,
}

// ---------------------------------------------------------------------------
// Bridge message types (web UI → CLI)
// ---------------------------------------------------------------------------

/// A file attachment bundled with an inbound user message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeAttachment {
    /// Display name (filename or label).
    pub name: String,
    /// Raw text or base64-encoded content.
    pub content: String,
    /// MIME type, e.g. `"text/plain"`.
    pub mime_type: Option<String>,
}

/// Messages flowing from the web UI into the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeMessage {
    /// A new user prompt from the web UI.
    UserMessage {
        content: String,
        session_id: String,
        message_id: String,
        #[serde(default)]
        attachments: Vec<BridgeAttachment>,
    },
    /// The web UI has responded to a permission request.
    PermissionResponse {
        request_id: String,
        tool_use_id: Option<String>,
        decision: PermissionDecision,
    },
    /// The web UI has answered an `AskUserQuestion` prompt.
    QuestionResponse {
        question_id: String,
        /// The chosen option, or free text. Empty means "dismissed".
        answer: String,
    },
    /// The web UI decided whether to trust a project MCP server.
    McpApprovalResponse {
        request_id: String,
        decision: McpApprovalDecision,
    },
    /// The web UI answered the warning about running without permission
    /// prompts.
    BypassResponse {
        request_id: String,
        /// `true` keeps the session in bypass mode.
        accept: bool,
    },
    /// The web UI gave the session a new title.
    RenameSession { title: String },
    /// A client opened the event stream.
    ///
    /// The relay sends this, not the client. Nothing else tells the runner
    /// that someone is watching, so without it a client attaching late sees
    /// only what the relay's ring buffer still holds.
    ClientAttached,
    /// Cancel the in-progress operation for a session.
    Cancel {
        session_id: String,
        reason: Option<String>,
    },
    /// Keepalive — the CLI should respond with a `Pong` event.
    Ping,
    /// A message this build does not know.
    ///
    /// Without it one unrecognised `type` fails the whole poll body and takes
    /// the channel down with it, so a relay newer than the CLI could not talk
    /// to it at all.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Bridge event types (CLI → web UI)
// ---------------------------------------------------------------------------

/// One turn of the conversation that happened before a remote client attached.
///
/// Text only, plus the names of any tools the turn called. The full tool input
/// and output are deliberately left out: the backfill exists so a client can
/// see what the session has been doing, not to reproduce the transcript byte
/// for byte, and a long run would otherwise be megabytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeHistoryEntry {
    /// `user` or `assistant`.
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

/// Token-budget / cost summary attached to `TurnComplete`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Kept apart from `cache_read_tokens` because the two are priced
    /// differently, even though a client may show them as one figure.
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Cost of this turn alone.
    pub cost_usd: Option<f64>,
    /// Cost of the whole session up to and including this turn.
    #[serde(default)]
    pub session_cost_usd: Option<f64>,
}

/// Whether the session is working, broadcast to the web UI.
///
/// Deliberately narrow. A client cannot observe a session before it registers,
/// so there is no "connecting" it could ever receive; a failure already
/// travels as [`BridgeEvent::Error`]; and a runner cannot report its own
/// departure, because the event would be uploaded to a session the relay is
/// about to delete. The client works out that the machine is gone by asking
/// whether the session is still listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeSessionState {
    Idle,
    Processing,
}

/// Events flowing from the CLI up to the web UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeEvent {
    /// Streaming text delta for the current assistant turn.
    TextDelta {
        text: String,
        message_id: String,
        index: Option<usize>,
    },
    /// A tool call has started executing.
    ToolStart {
        tool_name: String,
        tool_id: String,
        input_preview: Option<String>,
    },
    /// A tool call has finished.
    ToolEnd {
        tool_name: String,
        tool_id: String,
        result: String,
        is_error: bool,
        /// How long the tool's own work took, in milliseconds. Absent for a
        /// call that was blocked or cancelled before it ran, and for one
        /// recorded before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// The CLI needs the web UI to approve a tool use.
    PermissionRequest {
        request_id: String,
        tool_use_id: String,
        tool_name: String,
        description: String,
        options: Vec<String>,
    },
    /// A project-defined MCP server is waiting to be trusted.
    ///
    /// Its own event rather than a `UserQuestion`: approving one launches a
    /// command on the operator's machine, and a client has to be able to
    /// present that differently from the model asking something.
    McpApprovalRequest {
        request_id: String,
        server_name: String,
        /// The command the server would run, when it is a stdio server.
        command: Option<String>,
        /// The endpoint it would talk to, when it is an HTTP server.
        url: Option<String>,
    },
    /// The session is about to run without asking permission for anything.
    ///
    /// Its own event for the same reason as `McpApprovalRequest`: what it
    /// grants is wider than any single tool call, so a client has to be able
    /// to present it as a warning rather than as one more prompt.
    BypassWarning {
        request_id: String,
        message: String,
        /// The two answers, accept first.
        options: Vec<String>,
    },
    /// The model asked the user a question and the turn is waiting on it.
    UserQuestion {
        question_id: String,
        question: String,
        /// Predefined choices. Empty means free text only.
        options: Vec<String>,
    },
    /// Extended-thinking text for the current turn.
    ThinkingDelta { text: String, message_id: String },
    /// A transient status line, the same text the TUI shows while working.
    Status { message: String },
    /// The context window is filling up.
    TokenWarning {
        /// `warning` at 80 %, `critical` at 95 %.
        level: String,
        pct_used: f64,
    },
    /// The conversation so far, sent once when the bridge connects.
    History {
        entries: Vec<BridgeHistoryEntry>,
        /// Earlier turns left out of `entries`, so the client can say the
        /// transcript is partial rather than imply it starts here.
        omitted: usize,
    },
    /// The current turn has completed.
    TurnComplete {
        message_id: String,
        stop_reason: String,
        usage: Option<BridgeUsage>,
    },
    /// The outcome of a slash command, kept in the transcript.
    ///
    /// Distinct from `Status`, which is transient and replaced by the next
    /// one: a client that ran a command needs the answer to stay put.
    Notice {
        message: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A non-fatal diagnostic or user-visible error message.
    Error {
        message: String,
        code: Option<String>,
    },
    /// One row of the live execution timeline, new or updated.
    ///
    /// A row arrives once when it opens and again each time it changes, and
    /// `row.id` identifies which one: a client replaces the row it already
    /// holds under that id rather than appending a second copy.
    ///
    /// The timings are the ones the machine measured. A long poll can hold a
    /// batch for up to its full interval, so a client that stamped its own
    /// arrival times would report durations that are pure transport delay.
    TimelineRow { row: TimelineRow },
    /// Response to a `Ping` message.
    Pong { server_time: Option<u64> },
    /// Session lifecycle state change.
    SessionState {
        session_id: String,
        state: BridgeSessionState,
    },
}

// ---------------------------------------------------------------------------
// Bridge session state (internal)
// ---------------------------------------------------------------------------

/// Internal connection state of a [`BridgeSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeState {
    Disconnected,
    Connecting,
    Connected,
    Running,
    Error(String),
}

// ---------------------------------------------------------------------------
// Bridge session
// ---------------------------------------------------------------------------

/// Session facts that change while it runs, sent by re-registering.
///
/// Separate from [`BridgeConfig`], which is fixed when the bridge starts:
/// the model and the permission mode can change mid-session, and the cost
/// changes every turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub cost_usd: Option<f64>,
    /// Session title, which a client may also set.
    pub title: Option<String>,
}

/// The body of a registration POST.
///
/// Split out from the request so the shape can be asserted without a server.
fn registration_body(
    config: &BridgeConfig,
    session_id: &str,
    info: Option<&SessionInfo>,
) -> serde_json::Value {
    // `label` and `cwd` are additions on top of the original payload; extra
    // JSON keys are ignored by servers that do not know them.
    //
    // The hostname is the fallback because an unlabelled session shows up on
    // the phone as a bare uuid, which is unusable once two machines are
    // connected.
    let label = config.label.clone().or_else(machine_hostname);
    let mut body = serde_json::json!({
        "session_id": session_id,
        "device_id": config.device_id,
        "client_version": config.runner_version,
        "label": label,
        "cwd": config.cwd,
    });

    if let (Some(info), Some(map)) = (info, body.as_object_mut()) {
        // Absent rather than null when unknown, so a re-registration cannot
        // erase a value the relay already holds.
        if let Some(model) = &info.model {
            map.insert("model".into(), serde_json::Value::from(model.clone()));
        }
        if let Some(mode) = &info.permission_mode {
            map.insert(
                "permission_mode".into(),
                serde_json::Value::from(mode.clone()),
            );
        }
        if let Some(cost) = info.cost_usd {
            map.insert("cost_usd".into(), serde_json::Value::from(cost));
        }
        if let Some(title) = &info.title {
            map.insert("title".into(), serde_json::Value::from(title.clone()));
        }
    }
    body
}

/// POST a registration and hand back the status code.
///
/// A free function because `run_bridge_loop` moves the [`BridgeSession`] into
/// the poll task and still has to re-register when the session facts change.
/// One place builds the body, so the two callers cannot drift.
async fn post_registration(
    http: &reqwest::Client,
    config: &BridgeConfig,
    session_id: &str,
    info: Option<&SessionInfo>,
) -> anyhow::Result<u16> {
    let token = config
        .session_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Bridge register: no session token"))?;

    let url = format!("{}/api/claude_code/sessions", config.server_url);
    debug!(session_id = %session_id, url = %url, "Registering bridge session");

    let resp = http
        .post(&url)
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header("x-environment-runner-version", &config.runner_version)
        .json(&registration_body(config, session_id, info))
        .send()
        .await
        .context("Bridge register: HTTP send failed")?;

    Ok(resp.status().as_u16())
}

/// Active bridge session: owns the HTTP client, session credentials, and
/// state. Runs the poll loop in a background tokio task.
pub struct BridgeSession {
    config: BridgeConfig,
    session_id: String,
    state: Arc<RwLock<BridgeState>>,
    http: reqwest::Client,
    reconnect_count: u32,
    #[allow(dead_code)]
    last_ping: Option<std::time::Instant>,
}

impl BridgeSession {
    /// Create a new bridge session; generates a fresh UUID for `session_id`.
    pub fn new(config: BridgeConfig) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(format!("claude-code-rust/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("Failed to build reqwest client");

        Self {
            config,
            session_id,
            state: Arc::new(RwLock::new(BridgeState::Connecting)),
            http,
            reconnect_count: 0,
            last_ping: None,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn current_state(&self) -> BridgeState {
        self.state.read().clone()
    }

    fn set_state(&self, s: BridgeState) {
        *self.state.write() = s;
    }

    // -----------------------------------------------------------------------
    // Session registration / deregistration
    // -----------------------------------------------------------------------

    /// Register this bridge session with the CCR server.
    ///
    /// POST `/api/claude_code/sessions` — mirrors the TypeScript
    /// `registerBridgeEnvironment` call in `bridgeApi.ts`.
    pub async fn register(&mut self) -> anyhow::Result<()> {
        let status = post_registration(&self.http, &self.config, &self.session_id, None).await?;
        match status {
            200 | 201 => {
                self.set_state(BridgeState::Connected);
                info!(session_id = %self.session_id, "Bridge session registered");
                Ok(())
            }
            401 | 403 => {
                self.set_state(BridgeState::Error(format!("Auth error: {status}")));
                anyhow::bail!("Bridge register: auth error ({})", status)
            }
            _ => {
                anyhow::bail!("Bridge register: server returned {}", status)
            }
        }
    }

    /// Deregister the session on clean shutdown.
    ///
    /// DELETE `/api/claude_code/sessions/{id}` — best-effort; errors are
    /// logged and swallowed so they don't block process exit.
    pub async fn deregister(&self) {
        let Some(token) = self.config.session_token.as_deref() else {
            return;
        };

        let url = format!(
            "{}/api/claude_code/sessions/{}",
            self.config.server_url, self.session_id
        );

        debug!(session_id = %self.session_id, "Deregistering bridge session");

        match self.http.delete(&url).bearer_auth(token).send().await {
            Ok(r) if r.status().is_success() => {
                info!(session_id = %self.session_id, "Bridge session deregistered");
            }
            Ok(r) => {
                warn!(
                    session_id = %self.session_id,
                    status = %r.status(),
                    "Bridge deregister returned non-success (ignored)"
                );
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Bridge deregister HTTP error (ignored)"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Polling
    // -----------------------------------------------------------------------

    /// Long-poll for incoming messages from the web UI.
    ///
    /// GET `/api/claude_code/sessions/{id}/poll`
    ///
    /// - `200` → JSON array of [`BridgeMessage`]; may be empty.
    /// - `204` → No messages; returns empty vec.
    /// - `401`/`403` → Auth failure; sets state to `Disconnected` and errors.
    async fn poll_messages(&self) -> anyhow::Result<Vec<BridgeMessage>> {
        let token = self
            .config
            .session_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Poll: no token"))?;

        let url = format!(
            "{}/api/claude_code/sessions/{}/poll",
            self.config.server_url, self.session_id
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(35))
            .send()
            .await
            .context("Bridge poll: HTTP send failed")?;

        let status = resp.status().as_u16();
        match status {
            200 => {
                let text = resp.text().await.context("Bridge poll: reading body")?;
                if text.trim().is_empty() || text.trim() == "[]" {
                    return Ok(vec![]);
                }
                let msgs: Vec<BridgeMessage> =
                    serde_json::from_str(&text).context("Bridge poll: JSON parse")?;
                Ok(msgs)
            }
            204 => Ok(vec![]),
            401 | 403 => {
                self.set_state(BridgeState::Error(format!("Auth error: {status}")));
                anyhow::bail!("Bridge poll: auth error ({})", status)
            }
            _ => {
                anyhow::bail!("Bridge poll: server returned {}", status)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Event upload
    // -----------------------------------------------------------------------

    /// Everything the upload task needs, owned.
    ///
    /// The session itself stays with the poll task, so the uploader cannot
    /// borrow from it.
    fn event_uploader(&self) -> EventUploader {
        EventUploader {
            http: self.http.clone(),
            server_url: self.config.server_url.clone(),
            session_id: self.session_id.clone(),
            token: self.config.session_token.clone(),
        }
    }

    // -----------------------------------------------------------------------
    // Main poll loop
    // -----------------------------------------------------------------------

    /// Run the bridge poll loop until `cancel` is triggered or a fatal error
    /// occurs.
    ///
    /// Outgoing events leave through a task of their own, spawned here, so a
    /// poll held open by the server cannot delay them.
    ///
    /// On each iteration:
    /// 1. Long-poll for incoming messages and forward them to `msg_tx`.
    /// 2. Back off exponentially on consecutive errors; give up after
    ///    `config.max_reconnect_attempts`.
    /// 3. Sleep `polling_interval_ms` between successful cycles.
    pub async fn run_poll_loop(
        mut self,
        msg_tx: mpsc::Sender<BridgeMessage>,
        event_rx: mpsc::Receiver<BridgeEvent>,
        cancel: CancellationToken,
    ) {
        info!(session_id = %self.session_id, "Bridge poll loop started");

        // A child token, so every way out of the loop below can stop the
        // uploader. Waiting on the caller's token alone would hang the join on
        // an exit the caller did not ask for, such as the message receiver
        // going away.
        let upload_cancel = cancel.child_token();
        let uploads = tokio::spawn(run_upload_loop(
            self.event_uploader(),
            event_rx,
            upload_cancel.clone(),
        ));

        let base_interval =
            std::time::Duration::from_millis(self.config.polling_interval_ms.max(500));
        let max_backoff = std::time::Duration::from_secs(60);

        loop {
            // Respect cancellation at the top of every iteration.
            if cancel.is_cancelled() {
                info!(session_id = %self.session_id, "Bridge poll loop cancelled");
                break;
            }

            // --- Poll for incoming messages ---
            match self.poll_messages().await {
                Ok(messages) => {
                    // Successful poll — reset reconnect counter.
                    self.reconnect_count = 0;

                    let mut receiver_gone = false;
                    for msg in messages {
                        if msg_tx.send(msg).await.is_err() {
                            debug!(
                                session_id = %self.session_id,
                                "Incoming message channel closed; stopping poll loop"
                            );
                            receiver_gone = true;
                            break;
                        }
                    }
                    // Leaves through the shutdown path rather than returning,
                    // so the session is still deregistered and the upload task
                    // is still joined.
                    if receiver_gone {
                        break;
                    }
                }
                Err(e) => {
                    warn!(
                        session_id = %self.session_id,
                        error = %e,
                        reconnect_count = self.reconnect_count,
                        "Bridge poll error"
                    );

                    self.reconnect_count += 1;

                    if self.config.max_reconnect_attempts > 0
                        && self.reconnect_count >= self.config.max_reconnect_attempts
                    {
                        error!(
                            session_id = %self.session_id,
                            "Max bridge reconnect attempts ({}) reached; stopping",
                            self.config.max_reconnect_attempts
                        );
                        self.set_state(BridgeState::Error("max reconnects exceeded".into()));
                        break;
                    }

                    // Exponential backoff capped at `max_backoff`.
                    let backoff = (base_interval
                        * 2u32.pow(self.reconnect_count.saturating_sub(1).min(5)))
                    .min(max_backoff);

                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => {
                            info!(
                                session_id = %self.session_id,
                                "Bridge cancelled during backoff sleep"
                            );
                            break;
                        }
                    }
                    continue;
                }
            }

            // --- Wait for the next poll cycle ---
            tokio::select! {
                _ = tokio::time::sleep(base_interval) => {}
                _ = cancel.cancelled() => {
                    info!(
                        session_id = %self.session_id,
                        "Bridge cancelled during idle sleep"
                    );
                    break;
                }
            }
        }

        // Before deregistering, not after: the relay drops the session and
        // everything buffered for it, so an event still waiting to upload
        // would die with it.
        upload_cancel.cancel();
        let _ = uploads.await;

        // Best-effort deregister on shutdown.
        self.deregister().await;
        info!(session_id = %self.session_id, "Bridge poll loop terminated");
    }
}

/// Everything the upload task needs to reach the relay, owned.
struct EventUploader {
    http: reqwest::Client,
    server_url: String,
    session_id: String,
    token: Option<String>,
}

impl EventUploader {
    /// Batch-upload outgoing events to the web UI.
    ///
    /// POST `/api/claude_code/sessions/{id}/events`
    async fn post(&self, events: Vec<BridgeEvent>) -> anyhow::Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let token = self
            .token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Upload: no token"))?;

        let url = format!(
            "{}/api/claude_code/sessions/{}/events",
            self.server_url, self.session_id
        );

        let body = serde_json::json!({ "events": events });

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("Bridge upload: HTTP send failed")?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            warn!(
                session_id = %self.session_id,
                status,
                count = events.len(),
                "Bridge event upload failed"
            );
            anyhow::bail!("Bridge upload: server returned {}", status);
        }

        debug!(
            session_id = %self.session_id,
            count = events.len(),
            "Bridge events uploaded"
        );
        Ok(())
    }
}

/// Upload outgoing events as they are produced.
///
/// Its own task, because the poll it would otherwise share a loop with holds
/// its request open for as long as the server chooses. Streaming text produced
/// mid-poll used to wait that long before anyone remote could see it.
///
/// Batching survives the split: a wait for one event is followed by a drain of
/// whatever else is ready, so a busy turn still uploads in batches while an
/// idle session sends one event on its own.
async fn run_upload_loop(
    uploader: EventUploader,
    mut event_rx: mpsc::Receiver<BridgeEvent>,
    cancel: CancellationToken,
) {
    loop {
        let first = tokio::select! {
            event = event_rx.recv() => event,
            _ = cancel.cancelled() => break,
        };
        let Some(first) = first else { break };

        let mut batch = vec![first];
        while let Ok(event) = event_rx.try_recv() {
            batch.push(event);
        }

        // A failed batch is dropped rather than retried, so a relay that is
        // down cannot build a backlog that arrives out of order later.
        if let Err(e) = uploader.post(batch).await {
            warn!(session_id = %uploader.session_id, error = %e, "Event upload error");
        }
    }

    // Cancellation arrives while a turn may still be finishing, so take one
    // last look before the session is deregistered.
    let mut tail = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        tail.push(event);
    }
    if !tail.is_empty() {
        if let Err(e) = uploader.post(tail).await {
            warn!(session_id = %uploader.session_id, error = %e, "Final event upload error");
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge manager
// ---------------------------------------------------------------------------

/// High-level manager wrapping configuration and a shared HTTP client.
///
/// Prefer [`start_bridge`] for the simple one-shot API.
pub struct BridgeManager {
    config: BridgeConfig,
    http: reqwest::Client,
}

impl BridgeManager {
    pub fn new(config: BridgeConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(format!("claude-code-rust/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("BridgeManager: failed to build HTTP client")?;
        Ok(Self { config, http })
    }

    /// Start the bridge polling loop, returning channel endpoints and the
    /// session ID.
    ///
    /// The background task runs until `cancel` is triggered.
    pub async fn start(
        &self,
        cancel: CancellationToken,
    ) -> anyhow::Result<(
        mpsc::Receiver<BridgeMessage>,
        mpsc::Sender<BridgeEvent>,
        String,
    )> {
        start_bridge_with_client(self.config.clone(), self.http.clone(), cancel).await
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the bridge subsystem in a background task.
///
/// Registers a new session with the CCR server, then spawns a tokio task
/// running the poll loop. Returns:
/// - `msg_rx` — incoming messages from the web UI (e.g. user prompts).
/// - `event_tx` — sender for outgoing events (e.g. text deltas, tool calls).
/// - `session_id` — the UUID assigned to this session.
///
/// The background task runs until `cancel` is triggered or too many
/// consecutive errors occur. On shutdown the session is deregistered.
pub async fn start_bridge(
    config: BridgeConfig,
    cancel: CancellationToken,
) -> anyhow::Result<(
    mpsc::Receiver<BridgeMessage>,
    mpsc::Sender<BridgeEvent>,
    String,
)> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(format!("claude-code-rust/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("start_bridge: failed to build HTTP client")?;

    start_bridge_with_client(config, http, cancel).await
}

async fn start_bridge_with_client(
    config: BridgeConfig,
    _http: reqwest::Client,
    cancel: CancellationToken,
) -> anyhow::Result<(
    mpsc::Receiver<BridgeMessage>,
    mpsc::Sender<BridgeEvent>,
    String,
)> {
    if !config.is_active() {
        anyhow::bail!(
            "start_bridge: bridge is not active (enabled={}, token={})",
            config.enabled,
            config.session_token.is_some()
        );
    }

    let mut session = BridgeSession::new(config);
    session
        .register()
        .await
        .context("start_bridge: session registration failed")?;

    let session_id = session.session_id().to_string();

    // Bounded channels — back-pressure prevents unbounded memory growth on a
    // slow consumer.
    let (msg_tx, msg_rx) = mpsc::channel::<BridgeMessage>(64);
    let (event_tx, event_rx) = mpsc::channel::<BridgeEvent>(256);

    tokio::spawn(async move {
        session.run_poll_loop(msg_tx, event_rx, cancel).await;
    });

    info!(session_id = %session_id, "Bridge started");
    Ok((msg_rx, event_tx, session_id))
}

// ---------------------------------------------------------------------------
// High-level session API (start_bridge_session / poll / respond)
// ---------------------------------------------------------------------------

/// Information about a newly registered bridge session, returned by
/// [`start_bridge_session`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeSessionInfo {
    /// UUID assigned to this session.
    pub session_id: String,
    /// Full URL that can be shared with others to open the session in a browser.
    pub session_url: String,
    /// The auth token used for this session (redacted in Display).
    pub token: String,
}

impl std::fmt::Display for BridgeSessionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BridgeSessionInfo {{ session_id: {}, session_url: {} }}",
            self.session_id, self.session_url
        )
    }
}

/// A message returned by [`poll_bridge_messages`]: an inbound item from the
/// remote peer identified by a string `id`, `role`, and `content`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleMessage {
    /// Server-assigned message identifier.
    pub id: String,
    /// Sender role (`"user"` or `"assistant"`).
    pub role: String,
    /// Message text content.
    pub content: String,
}

/// Start a bridge session: generate a session ID, register it with the
/// Anthropic API, and return session info including the shareable URL.
///
/// # Authentication
///
/// Reads the bearer token from (in order of precedence):
/// 1. `MIKMIK_BRIDGE_TOKEN` environment variable
/// 2. `CLAUDE_BRIDGE_OAUTH_TOKEN` environment variable
///
/// If no token is found, returns an informative error.
///
/// # Errors
///
/// Returns an error if:
/// - No auth token is available
/// - The HTTP POST fails or the server returns a non-2xx status
/// - The server URL is not configured
///
/// # Example
///
/// ```rust,no_run
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// match mikmik_bridge::start_bridge_session(None).await {
///     Ok(info) => println!("Session URL: {}", info.session_url),
///     Err(e) => eprintln!("Could not start bridge: {e}"),
/// }
/// # });
/// ```
pub async fn start_bridge_session(
    token_override: Option<String>,
) -> anyhow::Result<BridgeSessionInfo> {
    // Resolve auth token.
    let token = token_override
        .or_else(|| std::env::var("MIKMIK_BRIDGE_TOKEN").ok())
        .or_else(|| std::env::var("CLAUDE_BRIDGE_OAUTH_TOKEN").ok())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Remote Control requires a session token.\n\
                 Set MIKMIK_BRIDGE_TOKEN=<your-token> to enable.\n\
                 Get a token from https://claude.ai (Settings → Remote Control).\n\
                 Note: Remote Control is only available with claude.ai subscriptions."
            )
        })?;

    // Resolve server base URL.
    let server_url = std::env::var("MIKMIK_BRIDGE_URL")
        .or_else(|_| std::env::var("CLAUDE_BRIDGE_BASE_URL"))
        .unwrap_or_else(|_| "https://claude.ai".to_string());

    let session_id = uuid::Uuid::new_v4().to_string();

    let hostname = {
        hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string())
    };

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(format!("claude-code-rust/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("start_bridge_session: failed to build HTTP client")?;

    let register_url = format!("{}/api/bridge/sessions", server_url);

    debug!(
        session_id = %session_id,
        url = %register_url,
        "Registering new bridge session"
    );

    let body = serde_json::json!({
        "session_id": session_id,
        "hostname": hostname,
        "client_version": env!("CARGO_PKG_VERSION"),
        "device_id": device_fingerprint(),
    });

    let resp = http
        .post(&register_url)
        .bearer_auth(&token)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "environments-2025-11-01")
        .json(&body)
        .send()
        .await
        .context("start_bridge_session: HTTP POST failed")?;

    let status = resp.status().as_u16();

    match status {
        200 | 201 => {
            info!(session_id = %session_id, "Bridge session registered successfully");
        }
        401 | 403 => {
            anyhow::bail!(
                "Bridge session registration failed: authentication error (HTTP {}).\n\
                 Your token may be invalid or expired.\n\
                 Get a new token from https://claude.ai (Settings → Remote Control).",
                status
            );
        }
        404 => {
            // The /api/bridge/sessions endpoint may not exist in all deployments.
            // Fall through to synthetic session URL (best-effort mode).
            warn!(
                session_id = %session_id,
                "Bridge registration endpoint not found (HTTP 404) — \
                 using local session ID without server validation"
            );
        }
        _ => {
            let body_text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Bridge session registration failed: server returned HTTP {}. {}",
                status,
                if body_text.is_empty() {
                    String::new()
                } else {
                    format!("Response: {}", &body_text[..body_text.len().min(200)])
                }
            );
        }
    }

    // Build the shareable session URL.
    let session_url = format!("{}/code/sessions/{}", server_url, session_id);

    Ok(BridgeSessionInfo {
        session_id,
        session_url,
        token,
    })
}

/// Poll for incoming messages on an active bridge session.
///
/// GETs `/api/bridge/sessions/<id>/messages?since=<last_msg_id>` and returns
/// the batch of new messages. Uses a 30-second HTTP timeout. On HTTP 429
/// (rate-limited) the function sleeps with exponential back-off before
/// retrying (up to 3 attempts).
///
/// Returns an empty `Vec` when there are no new messages (HTTP 204 or empty
/// body).
pub async fn poll_bridge_messages(
    info: &BridgeSessionInfo,
    since_id: Option<&str>,
) -> anyhow::Result<Vec<SimpleMessage>> {
    let server_url = std::env::var("MIKMIK_BRIDGE_URL")
        .or_else(|_| std::env::var("CLAUDE_BRIDGE_BASE_URL"))
        .unwrap_or_else(|_| "https://claude.ai".to_string());

    // Validate session_id before interpolating into URL.
    BridgeConfig::validate_id(&info.session_id, "session_id")?;

    let base_url = format!(
        "{}/api/bridge/sessions/{}/messages",
        server_url, info.session_id
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(35))
        .user_agent(format!("claude-code-rust/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("poll_bridge_messages: failed to build HTTP client")?;

    // Retry loop for 429 back-off.
    let max_retries = 3u32;
    let mut attempt = 0u32;
    loop {
        let mut request = http
            .get(&base_url)
            .bearer_auth(&info.token)
            .header("anthropic-version", "2023-06-01");

        if let Some(since) = since_id {
            request = request.query(&[("since", since)]);
        }

        let resp = request
            .send()
            .await
            .context("poll_bridge_messages: HTTP GET failed")?;

        let status = resp.status().as_u16();
        match status {
            200 => {
                let text = resp
                    .text()
                    .await
                    .context("poll_bridge_messages: reading body")?;
                if text.trim().is_empty() || text.trim() == "[]" {
                    return Ok(vec![]);
                }
                let msgs: Vec<SimpleMessage> =
                    serde_json::from_str(&text).context("poll_bridge_messages: JSON parse")?;
                return Ok(msgs);
            }
            204 => return Ok(vec![]),
            429 => {
                attempt += 1;
                if attempt > max_retries {
                    anyhow::bail!(
                        "poll_bridge_messages: rate-limited (HTTP 429) after {} retries",
                        max_retries
                    );
                }
                let backoff = std::time::Duration::from_millis(1_000 * 2u64.pow(attempt - 1));
                warn!(
                    attempt,
                    "Bridge poll rate-limited; backing off {:?}", backoff
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
            401 | 403 => {
                anyhow::bail!("poll_bridge_messages: auth error (HTTP {})", status);
            }
            _ => {
                anyhow::bail!("poll_bridge_messages: server returned HTTP {}", status);
            }
        }
    }
}

/// Post a response to a specific incoming message on an active bridge session.
///
/// PUTs `/api/bridge/sessions/<session_id>/messages/<msg_id>/response` with
/// a JSON body `{"content": "<response>", "done": true}`.
pub async fn post_bridge_response(
    info: &BridgeSessionInfo,
    msg_id: &str,
    content: &str,
    done: bool,
) -> anyhow::Result<()> {
    let server_url = std::env::var("MIKMIK_BRIDGE_URL")
        .or_else(|_| std::env::var("CLAUDE_BRIDGE_BASE_URL"))
        .unwrap_or_else(|_| "https://claude.ai".to_string());

    // Validate IDs before URL interpolation.
    BridgeConfig::validate_id(&info.session_id, "session_id")?;
    BridgeConfig::validate_id(msg_id, "msg_id")?;

    let url = format!(
        "{}/api/bridge/sessions/{}/messages/{}/response",
        server_url, info.session_id, msg_id
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(format!("claude-code-rust/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("post_bridge_response: failed to build HTTP client")?;

    let body = serde_json::json!({
        "content": content,
        "done": done,
    });

    debug!(
        session_id = %info.session_id,
        msg_id = %msg_id,
        done = done,
        "Posting bridge response"
    );

    let resp = http
        .put(&url)
        .bearer_auth(&info.token)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("post_bridge_response: HTTP PUT failed")?;

    let status = resp.status().as_u16();
    if resp.status().is_success() {
        debug!(session_id = %info.session_id, msg_id = %msg_id, "Bridge response posted");
        Ok(())
    } else {
        anyhow::bail!(
            "post_bridge_response: server returned HTTP {} for msg {}",
            status,
            msg_id
        )
    }
}

/// Post a single streaming tool/text event to the bridge server (non-blocking,
/// best-effort).
///
/// POSTs `{"event": <payload>, "ts": <unix_ms>}` to
/// `/api/bridge/sessions/<session_id>/events`.
///
/// Errors are returned to the caller, who should treat them as transient and
/// ignore them so the query loop is never blocked.
pub async fn post_bridge_event(info: &BridgeSessionInfo, payload: String) -> anyhow::Result<()> {
    let server_url = std::env::var("MIKMIK_BRIDGE_URL")
        .or_else(|_| std::env::var("CLAUDE_BRIDGE_BASE_URL"))
        .unwrap_or_else(|_| "https://claude.ai".to_string());

    // Validate session_id before URL interpolation.
    BridgeConfig::validate_id(&info.session_id, "session_id")?;

    let url = format!(
        "{}/api/bridge/sessions/{}/events",
        server_url, info.session_id
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent(format!("claude-code-rust/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("post_bridge_event: failed to build HTTP client")?;

    let body = serde_json::json!({
        "event": payload,
        "ts": chrono::Utc::now().timestamp_millis(),
    });

    debug!(
        session_id = %info.session_id,
        "Posting bridge event"
    );

    let resp = http
        .post(&url)
        .bearer_auth(&info.token)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("post_bridge_event: HTTP POST failed")?;

    let status = resp.status().as_u16();
    if resp.status().is_success() {
        debug!(session_id = %info.session_id, "Bridge event posted");
        Ok(())
    } else {
        anyhow::bail!("post_bridge_event: server returned HTTP {}", status)
    }
}

// ---------------------------------------------------------------------------
// TUI-facing bridge event types (bridge → TUI state machine)
// ---------------------------------------------------------------------------

/// How the remote UI responded to a permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionResponseKind {
    Allow,
    Deny,
    AllowSession,
}

/// Internal events sent from the bridge loop to the TUI / main event loop.
///
/// These are *not* the same as [`BridgeEvent`] (which flows CLI → web UI).
/// `TuiBridgeEvent` flows from the bridge worker task into the main loop so
/// the TUI can update connection state, inject prompts, etc.
#[derive(Debug, Clone)]
pub enum TuiBridgeEvent {
    /// The bridge registered successfully and is now polling.
    Connected {
        session_url: String,
        session_id: String,
    },
    /// The connection was lost (cleanly or due to error).
    Disconnected { reason: Option<String> },
    /// Attempting to reconnect after a failure.
    Reconnecting { attempt: u32 },
    /// The web UI sent a new user prompt.
    InboundPrompt {
        content: String,
        /// Files bundled with the prompt. Dropping these would silently lose
        /// the screenshot a user attached.
        attachments: Vec<BridgeAttachment>,
        sender_id: Option<String>,
    },
    /// The web UI asked to cancel the in-progress operation.
    Cancelled,
    /// The web UI responded to a pending permission request.
    PermissionResponse {
        tool_use_id: String,
        response: PermissionResponseKind,
    },
    /// The web UI answered a pending `AskUserQuestion` prompt.
    QuestionAnswer { question_id: String, answer: String },
    /// The web UI decided whether to trust a project MCP server.
    McpApproval {
        request_id: String,
        decision: McpApprovalDecision,
    },
    /// The web UI answered the bypass-permissions warning.
    BypassResponse { request_id: String, accept: bool },
    /// The web UI gave the session a new title.
    SessionRename { title: String },
    /// A client opened the event stream and needs the session as it stands.
    ClientAttached,
    /// A non-fatal diagnostic from the bridge worker.
    Error(String),
    /// Keepalive ping — no TUI action required.
    Ping,
}

// ---------------------------------------------------------------------------
// Outbound event types (query loop → bridge → web UI)
// ---------------------------------------------------------------------------

/// Events from the query/tool loop forwarded outbound to the web UI via the
/// bridge upload channel. The bridge worker serialises these into
/// [`BridgeEvent`] values and POSTs them to the server.
#[derive(Debug, Clone)]
pub enum BridgeOutbound {
    TextDelta {
        delta: String,
        message_id: String,
    },
    ToolStart {
        id: String,
        name: String,
        input_preview: Option<String>,
    },
    ToolEnd {
        id: String,
        output: String,
        is_error: bool,
        /// How long the tool's own work took, in milliseconds. `None` for a
        /// call that was blocked or cancelled before it ran.
        duration_ms: Option<u64>,
    },
    TurnComplete {
        message_id: String,
        stop_reason: String,
        /// Absent for a turn that spent no tokens, such as the reply to a
        /// slash command.
        usage: Option<BridgeUsage>,
    },
    Error {
        message: String,
    },
    /// The outcome of a slash command, for the transcript rather than the
    /// transient status line.
    Notice {
        message: String,
        is_error: bool,
    },
    /// Session facts worth showing before a client attaches.
    ///
    /// Re-registers rather than joining the event stream: the relay keeps
    /// events opaque on purpose, and folding these in would make it parse
    /// them.
    SessionInfo(SessionInfo),
    /// A tool is waiting on approval that the remote client may give.
    ///
    /// Without this the remote client never learns a prompt is pending, so a
    /// remotely-driven session stalls on its first tool call.
    PermissionRequest {
        request_id: String,
        tool_use_id: String,
        tool_name: String,
        description: String,
        options: Vec<String>,
    },
    /// The model called `AskUserQuestion` and the turn is waiting on an answer.
    ///
    /// Same reasoning as `PermissionRequest`: the tool blocks on a channel with
    /// no timeout, so without this the session stalls with nobody able to see
    /// why.
    UserQuestion {
        question_id: String,
        question: String,
        options: Vec<String>,
    },
    /// A project-defined MCP server is waiting to be trusted.
    McpApprovalRequest {
        request_id: String,
        server_name: String,
        command: Option<String>,
        url: Option<String>,
    },
    /// The session is about to run without asking permission for anything.
    ///
    /// The TUI shows this as a full-screen warning and will not start a turn
    /// until it is answered. A remote client that never saw it would watch a
    /// session that looks idle, and the operator sitting at the terminal would
    /// be the only one who could clear it.
    BypassWarning {
        request_id: String,
        message: String,
        /// The two answers, accept first, in the order a client should show
        /// them.
        options: Vec<String>,
    },
    /// Extended-thinking text, kept separate from the answer.
    ///
    /// The TUI renders it; without this a remote screen sits blank for as long
    /// as the model reasons, which reads as a hung session.
    ThinkingDelta {
        delta: String,
        message_id: String,
    },
    /// A transient status line.
    Status {
        message: String,
    },
    /// The context window is filling up.
    TokenWarning {
        level: String,
        pct_used: f64,
    },
    /// Whether a turn is running, so a client can show a busy indicator.
    SessionBusy {
        busy: bool,
    },
    /// One row of the live execution timeline, new or updated.
    ///
    /// The terminal builds the row and hands over a copy, so both screens show
    /// the same step with the same timings.
    TimelineRow(TimelineRow),
    /// The conversation that happened before the bridge connected.
    ///
    /// Without it a client attaching to a session already in progress sees an
    /// empty screen and cannot tell what the machine has been doing.
    History {
        entries: Vec<BridgeHistoryEntry>,
        omitted: usize,
    },
}

// ---------------------------------------------------------------------------
// run_bridge_loop — high-level bridge task entry point
// ---------------------------------------------------------------------------

/// Translate a remote client's decision into the local permission outcome.
///
/// `AllowPermanently` becomes a session-scoped allow rather than a persistent
/// rule: a tap on a phone should not write a permanent entry into the
/// machine's settings. Collapsing it into a one-shot allow instead would
/// re-prompt on the very next tool call, which is not what was chosen.
fn response_kind_for(decision: PermissionDecision) -> PermissionResponseKind {
    match decision {
        PermissionDecision::Allow => PermissionResponseKind::Allow,
        PermissionDecision::AllowPermanently => PermissionResponseKind::AllowSession,
        PermissionDecision::Deny | PermissionDecision::DenyPermanently => {
            PermissionResponseKind::Deny
        }
    }
}

/// Run the bridge subsystem as a background task, translating low-level
/// [`BridgeMessage`] poll results into [`TuiBridgeEvent`] values and
/// forwarding [`BridgeOutbound`] events to the server.
///
/// # Parameters
/// - `config` — bridge configuration (must be active: `enabled == true` and
///   `session_token` is `Some`).
/// - `tui_tx` — channel used to send state-change events to the TUI / main
///   loop.
/// - `outbound_rx` — channel for receiving outbound events from the query
///   loop to upload to the bridge server.
/// - `cancel` — token that triggers a clean shutdown of the loop.
pub async fn run_bridge_loop(
    config: BridgeConfig,
    tui_tx: mpsc::Sender<TuiBridgeEvent>,
    mut outbound_rx: mpsc::Receiver<BridgeOutbound>,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    if !config.is_active() {
        anyhow::bail!(
            "run_bridge_loop: bridge is not active (enabled={}, token={})",
            config.enabled,
            config.session_token.is_some()
        );
    }

    // Build a BridgeSession and register with the server.
    let mut session = BridgeSession::new(config.clone());

    // Attempt initial registration; retry with back-off on transient errors.
    let base_backoff = std::time::Duration::from_millis(1_000);
    let max_backoff = std::time::Duration::from_secs(30);
    let mut reg_attempts = 0u32;

    loop {
        match session.register().await {
            Ok(()) => break,
            Err(e) => {
                reg_attempts += 1;
                warn!(
                    attempt = reg_attempts,
                    error = %e,
                    "Bridge registration failed"
                );

                // Auth errors are fatal — don't retry.
                let msg = e.to_string();
                if msg.contains("auth error") || msg.contains("401") || msg.contains("403") {
                    let _ = tui_tx
                        .send(TuiBridgeEvent::Error(format!("Bridge auth failed: {}", e)))
                        .await;
                    return Err(e);
                }

                if reg_attempts >= config.max_reconnect_attempts.max(1) {
                    let _ = tui_tx
                        .send(TuiBridgeEvent::Error(format!(
                            "Bridge registration failed after {} attempts: {}",
                            reg_attempts, e
                        )))
                        .await;
                    return Err(e);
                }

                let backoff = (base_backoff * 2u32.pow(reg_attempts.min(5))).min(max_backoff);
                let _ = tui_tx
                    .send(TuiBridgeEvent::Reconnecting {
                        attempt: reg_attempts,
                    })
                    .await;

                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => {
                        return Ok(());
                    }
                }
            }
        }
    }

    // Build the session URL from server_url + session_id.
    let session_url = format!(
        "{}/remote?session={}",
        config.server_url,
        session.session_id()
    );
    let session_id = session.session_id().to_string();

    let _ = tui_tx
        .send(TuiBridgeEvent::Connected {
            session_url: session_url.clone(),
            session_id: session_id.clone(),
        })
        .await;

    // Build outgoing BridgeEvent channel for the poll loop.
    let (bridge_ev_tx, bridge_ev_rx) = mpsc::channel::<BridgeEvent>(256);

    // Build incoming message channel.
    let (msg_tx, mut msg_rx) = mpsc::channel::<BridgeMessage>(64);

    // Kept before the session moves into the poll task, so a later
    // re-registration reuses the same connection pool.
    let meta_http = session.http.clone();

    // Spawn the low-level poll loop in its own task.
    let poll_cancel = cancel.clone();
    tokio::spawn(async move {
        session
            .run_poll_loop(msg_tx, bridge_ev_rx, poll_cancel)
            .await;
    });

    // Message ID counter for outbound text deltas.
    let mut msg_counter = 0u64;

    let poll_interval = std::time::Duration::from_millis(config.polling_interval_ms.max(50));

    loop {
        tokio::select! {
            // Handle cancellation.
            _ = cancel.cancelled() => {
                let _ = tui_tx.send(TuiBridgeEvent::Disconnected { reason: None }).await;
                break;
            }

            // Convert inbound BridgeMessage → TuiBridgeEvent.
            msg = msg_rx.recv() => {
                match msg {
                    None => {
                        // Poll loop shut down.
                        let _ = tui_tx
                            .send(TuiBridgeEvent::Disconnected {
                                reason: Some("Bridge poll loop terminated".to_string()),
                            })
                            .await;
                        break;
                    }
                    Some(BridgeMessage::UserMessage {
                        content,
                        attachments,
                        ..
                    }) => {
                        let _ = tui_tx
                            .send(TuiBridgeEvent::InboundPrompt {
                                content,
                                attachments,
                                sender_id: None,
                            })
                            .await;
                    }
                    Some(BridgeMessage::PermissionResponse { tool_use_id, decision, .. }) => {
                        let kind = response_kind_for(decision);
                        let tuid = tool_use_id.unwrap_or_default();
                        if !tuid.is_empty() {
                            let _ = tui_tx
                                .send(TuiBridgeEvent::PermissionResponse {
                                    tool_use_id: tuid,
                                    response: kind,
                                })
                                .await;
                        }
                    }
                    Some(BridgeMessage::QuestionResponse {
                        question_id,
                        answer,
                    }) => {
                        let _ = tui_tx
                            .send(TuiBridgeEvent::QuestionAnswer {
                                question_id,
                                answer,
                            })
                            .await;
                    }
                    Some(BridgeMessage::McpApprovalResponse {
                        request_id,
                        decision,
                    }) => {
                        let _ = tui_tx
                            .send(TuiBridgeEvent::McpApproval {
                                request_id,
                                decision,
                            })
                            .await;
                    }
                    Some(BridgeMessage::BypassResponse { request_id, accept }) => {
                        let _ = tui_tx
                            .send(TuiBridgeEvent::BypassResponse { request_id, accept })
                            .await;
                    }
                    Some(BridgeMessage::RenameSession { title }) => {
                        let _ = tui_tx.send(TuiBridgeEvent::SessionRename { title }).await;
                    }
                    Some(BridgeMessage::ClientAttached) => {
                        let _ = tui_tx.send(TuiBridgeEvent::ClientAttached).await;
                    }
                    Some(BridgeMessage::Unknown) => {
                        debug!(
                            session_id = %session_id,
                            "ignoring a bridge message this build does not know"
                        );
                    }
                    Some(BridgeMessage::Cancel { .. }) => {
                        let _ = tui_tx.send(TuiBridgeEvent::Cancelled).await;
                    }
                    Some(BridgeMessage::Ping) => {
                        let _ = tui_tx.send(TuiBridgeEvent::Ping).await;
                        // Also respond with a Pong to the server.
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::Pong {
                                server_time: Some(chrono::Utc::now().timestamp() as u64),
                            })
                            .await;
                    }
                }
            }

            // Forward outbound events from query loop → bridge server.
            outbound = outbound_rx.recv() => {
                match outbound {
                    None => {
                        // Sender dropped; nothing to forward.
                    }
                    Some(BridgeOutbound::TextDelta { delta, message_id }) => {
                        msg_counter += 1;
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::TextDelta {
                                text: delta,
                                message_id,
                                index: Some(msg_counter as usize),
                            })
                            .await;
                    }
                    Some(BridgeOutbound::ToolStart { id, name, input_preview }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::ToolStart {
                                tool_name: name,
                                tool_id: id,
                                input_preview,
                            })
                            .await;
                    }
                    Some(BridgeOutbound::ToolEnd {
                        id,
                        output,
                        is_error,
                        duration_ms,
                    }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::ToolEnd {
                                tool_name: String::new(),
                                tool_id: id,
                                result: output,
                                is_error,
                                duration_ms,
                            })
                            .await;
                    }
                    Some(BridgeOutbound::TurnComplete {
                        message_id,
                        stop_reason,
                        usage,
                    }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::TurnComplete {
                                message_id,
                                stop_reason,
                                usage,
                            })
                            .await;
                    }
                    Some(BridgeOutbound::Error { message }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::Error {
                                message,
                                code: None,
                            })
                            .await;
                    }
                    Some(BridgeOutbound::SessionInfo(info)) => {
                        // Not an event: it updates the session record so a
                        // client can tell two sessions apart in the list,
                        // before attaching to either.
                        if let Err(e) =
                            post_registration(&meta_http, &config, &session_id, Some(&info)).await
                        {
                            debug!(error = %e, "Session info update failed (ignored)");
                        }
                    }
                    Some(BridgeOutbound::Notice { message, is_error }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::Notice { message, is_error })
                            .await;
                    }
                    Some(BridgeOutbound::PermissionRequest {
                        request_id,
                        tool_use_id,
                        tool_name,
                        description,
                        options,
                    }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::PermissionRequest {
                                request_id,
                                tool_use_id,
                                tool_name,
                                description,
                                options,
                            })
                            .await;
                    }
                    Some(BridgeOutbound::Status { message }) => {
                        let _ = bridge_ev_tx.send(BridgeEvent::Status { message }).await;
                    }
                    Some(BridgeOutbound::TimelineRow(row)) => {
                        let _ = bridge_ev_tx.send(BridgeEvent::TimelineRow { row }).await;
                    }
                    Some(BridgeOutbound::TokenWarning { level, pct_used }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::TokenWarning { level, pct_used })
                            .await;
                    }
                    Some(BridgeOutbound::SessionBusy { busy }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::SessionState {
                                session_id: session_id.clone(),
                                state: if busy {
                                    BridgeSessionState::Processing
                                } else {
                                    BridgeSessionState::Idle
                                },
                            })
                            .await;
                    }
                    Some(BridgeOutbound::ThinkingDelta { delta, message_id }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::ThinkingDelta {
                                text: delta,
                                message_id,
                            })
                            .await;
                    }
                    Some(BridgeOutbound::History { entries, omitted }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::History { entries, omitted })
                            .await;
                    }
                    Some(BridgeOutbound::UserQuestion {
                        question_id,
                        question,
                        options,
                    }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::UserQuestion {
                                question_id,
                                question,
                                options,
                            })
                            .await;
                    }
                    Some(BridgeOutbound::McpApprovalRequest {
                        request_id,
                        server_name,
                        command,
                        url,
                    }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::McpApprovalRequest {
                                request_id,
                                server_name,
                                command,
                                url,
                            })
                            .await;
                    }
                    Some(BridgeOutbound::BypassWarning {
                        request_id,
                        message,
                        options,
                    }) => {
                        let _ = bridge_ev_tx
                            .send(BridgeEvent::BypassWarning {
                                request_id,
                                message,
                                options,
                            })
                            .await;
                    }
                }
            }

            // Yield briefly to avoid busy-polling.
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Trusted device module (re-exported for external callers)
// ---------------------------------------------------------------------------

pub mod trusted_device {
    /// Re-export the crate-level device fingerprint function.
    pub use super::device_fingerprint;
}

// ---------------------------------------------------------------------------
// JWT module (re-exported for external callers)
// ---------------------------------------------------------------------------

pub mod jwt {
    pub use super::{decode_jwt_expiry, jwt_is_expired, JwtClaims};
}

// ---------------------------------------------------------------------------
// Re-exports
// ---------------------------------------------------------------------------

// Allow downstream crates to use reqwest types without a direct dep.
pub use reqwest;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A relay that holds every poll open, and reports when an upload lands.
    ///
    /// Hand-rolled because the workspace carries no HTTP mocking crate;
    /// `crates/api/src/providers/minimax.rs` stubs a provider the same way.
    /// Each connection is served in its own task, because the poll and the
    /// upload are meant to be in flight at the same time.
    async fn holding_relay(
        hold: std::time::Duration,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let (uploads_tx, uploads_rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let uploads_tx = uploads_tx.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buffer = [0_u8; 2048];
                    // The request line is all this stub reads; the body is
                    // left in the socket, which the client does not mind.
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => head.extend_from_slice(&buffer[..read]),
                        }
                    }
                    let request = String::from_utf8_lossy(&head).to_string();

                    let response: &[u8] = if request.starts_with("GET") {
                        tokio::time::sleep(hold).await;
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n[]"
                    } else {
                        let _ = uploads_tx.send(request);
                        b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    };
                    let _ = socket.write_all(response).await;
                    let _ = socket.flush().await;
                });
            }
        });

        (format!("http://{address}"), uploads_rx)
    }

    #[tokio::test]
    async fn an_event_uploads_while_a_poll_is_held_open() {
        // The poll holds far longer than the assertion window, so this cannot
        // pass while uploads share the poll's turn. Before the split an event
        // produced mid-poll waited for the server to let the poll go, which is
        // 25 seconds against our own relay.
        let hold = std::time::Duration::from_secs(5);
        let (server_url, mut uploads) = holding_relay(hold).await;

        let session = BridgeSession::new(BridgeConfig {
            enabled: true,
            server_url,
            session_token: Some("test-token".into()),
            polling_interval_ms: 50,
            ..Default::default()
        });

        let (msg_tx, _msg_rx) = mpsc::channel::<BridgeMessage>(8);
        let (event_tx, event_rx) = mpsc::channel::<BridgeEvent>(8);
        let cancel = CancellationToken::new();
        let loop_cancel = cancel.clone();
        let poll_loop = tokio::spawn(async move {
            session.run_poll_loop(msg_tx, event_rx, loop_cancel).await;
        });

        // Long enough for the poll to be in flight, short enough to be well
        // inside the hold.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        event_tx
            .send(BridgeEvent::Status {
                message: "mid-poll".into(),
            })
            .await
            .expect("event channel should accept");

        let seen = tokio::time::timeout(std::time::Duration::from_secs(2), uploads.recv())
            .await
            .expect("the upload should not wait for the poll to return")
            .expect("the stub should report the upload");
        assert!(seen.starts_with("POST"), "unexpected request: {seen}");

        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), poll_loop).await;
    }

    #[test]
    fn test_device_fingerprint_is_non_empty() {
        let fp = device_fingerprint();
        assert!(!fp.is_empty(), "fingerprint should not be empty");
        // SHA-256 hex is always 64 chars
        assert_eq!(fp.len(), 64, "SHA-256 hex digest should be 64 chars");
    }

    #[test]
    fn test_device_fingerprint_is_stable() {
        let a = device_fingerprint();
        let b = device_fingerprint();
        assert_eq!(a, b, "fingerprint must be deterministic");
    }

    #[test]
    fn test_jwt_decode_invalid() {
        assert!(JwtClaims::decode("notajwt").is_err());
        // Malformed-but-two-segment input: either Ok or Err is acceptable, the
        // contract is only that decoding must not panic.
        let _ = JwtClaims::decode("only.two");
    }

    #[test]
    fn test_jwt_expired_unparseable() {
        // Unparseable token defaults to expired=true
        assert!(jwt_is_expired("bad.token.here"));
    }

    #[test]
    fn test_bridge_config_default_not_active() {
        let cfg = BridgeConfig::default();
        assert!(!cfg.is_active(), "default config must not be active");
    }

    #[test]
    fn test_bridge_config_with_token_still_needs_enabled() {
        let mut cfg = BridgeConfig {
            session_token: Some("tok".into()),
            ..Default::default()
        };
        assert!(!cfg.is_active(), "needs enabled=true too");
        cfg.enabled = true;
        assert!(cfg.is_active());
    }

    #[test]
    fn test_validate_id_rejects_traversal() {
        assert!(BridgeConfig::validate_id("../../etc/passwd", "id").is_err());
        assert!(BridgeConfig::validate_id("abc123", "id").is_ok());
        assert!(BridgeConfig::validate_id("env_abc-123", "id").is_ok());
        assert!(BridgeConfig::validate_id("", "id").is_err());
    }

    #[test]
    fn test_permission_decision_serde() {
        let d = PermissionDecision::AllowPermanently;
        let s = serde_json::to_string(&d).unwrap();
        assert_eq!(s, r#""allow_permanently""#);
        let back: PermissionDecision = serde_json::from_str(&s).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn an_allow_permanently_survives_the_next_tool_call() {
        assert_eq!(
            response_kind_for(PermissionDecision::AllowPermanently),
            PermissionResponseKind::AllowSession
        );
        assert_eq!(
            response_kind_for(PermissionDecision::Allow),
            PermissionResponseKind::Allow
        );
        assert_eq!(
            response_kind_for(PermissionDecision::DenyPermanently),
            PermissionResponseKind::Deny
        );
    }

    #[test]
    fn test_bridge_session_state_serde() {
        // Both, because the client reads the wire word rather than a boolean
        // and either one spelled differently would silently stop the spinner
        // or leave it on forever.
        for (state, wire) in [
            (BridgeSessionState::Processing, r#""processing""#),
            (BridgeSessionState::Idle, r#""idle""#),
        ] {
            let encoded = serde_json::to_string(&state).expect("serialise");
            assert_eq!(encoded, wire);
        }
    }

    #[test]
    fn test_bridge_message_serde_user_message() {
        let msg = BridgeMessage::UserMessage {
            content: "hello".into(),
            session_id: "s1".into(),
            message_id: "m1".into(),
            attachments: vec![],
        };
        let j = serde_json::to_string(&msg).unwrap();
        assert!(j.contains(r#""type":"user_message""#));
    }

    #[test]
    fn test_bridge_event_text_delta_serde() {
        let ev = BridgeEvent::TextDelta {
            text: "hello world".into(),
            message_id: "m1".into(),
            index: Some(0),
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"text_delta""#));
        assert!(j.contains("hello world"));
    }

    #[test]
    fn test_bridge_event_pong_serde() {
        let ev = BridgeEvent::Pong {
            server_time: Some(1_700_000_000),
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"pong""#));
    }

    #[test]
    fn a_completed_turn_carries_every_usage_figure() {
        let ev = BridgeEvent::TurnComplete {
            message_id: "turn-3".into(),
            stop_reason: "end_turn".into(),
            usage: Some(BridgeUsage {
                input_tokens: 1_240,
                output_tokens: 380,
                cache_creation_tokens: 2_048,
                cache_read_tokens: 10_752,
                cost_usd: Some(0.0038),
                session_cost_usd: Some(0.0421),
            }),
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"turn_complete""#));
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_creation_tokens",
            "cache_read_tokens",
            "cost_usd",
            "session_cost_usd",
        ] {
            assert!(j.contains(field), "{field} missing from {j}");
        }

        // The client keys off the presence of `usage`, so it has to survive a
        // round trip rather than be reconstructed from defaults.
        let back: BridgeEvent = serde_json::from_str(&j).unwrap();
        match back {
            BridgeEvent::TurnComplete { usage: Some(u), .. } => {
                assert_eq!(u.cache_read_tokens, 10_752);
                assert_eq!(u.session_cost_usd, Some(0.0421));
            }
            other => panic!("expected a turn_complete carrying usage, got {other:?}"),
        }
    }

    #[test]
    fn a_registration_omits_facts_it_does_not_know() {
        // Absent, not null: the relay merges a registration over what it
        // already holds, so a null would erase the value instead of leaving
        // it alone.
        let config = BridgeConfig {
            label: Some("workstation".into()),
            ..Default::default()
        };

        let plain = registration_body(&config, "s1", None);
        assert_eq!(plain["label"], "workstation");
        assert!(plain.get("model").is_none());
        assert!(plain.get("cost_usd").is_none());

        let partial = registration_body(
            &config,
            "s1",
            Some(&SessionInfo {
                model: Some("claude-sonnet-4-5".into()),
                permission_mode: None,
                cost_usd: Some(0.0421),
                title: Some("refactor the parser".into()),
            }),
        );
        assert_eq!(partial["model"], "claude-sonnet-4-5");
        assert_eq!(partial["cost_usd"], 0.0421);
        assert_eq!(partial["title"], "refactor the parser");
        assert!(partial.get("permission_mode").is_none());
    }

    #[test]
    fn a_message_type_this_build_does_not_know_is_ignored() {
        // The poll body is parsed as a whole, so an unrecognised variant used
        // to fail every message in the batch and stall the channel. A relay
        // newer than the CLI is the ordinary way that happens.
        let batch = r#"[{"type":"client_attached"},{"type":"invented_later"},{"type":"ping"}]"#;
        let parsed: Vec<BridgeMessage> = serde_json::from_str(batch).expect("batch should parse");
        assert!(matches!(parsed[0], BridgeMessage::ClientAttached));
        assert!(matches!(parsed[1], BridgeMessage::Unknown));
        assert!(matches!(parsed[2], BridgeMessage::Ping));
    }

    #[test]
    fn a_rename_survives_the_wire() {
        let msg = BridgeMessage::RenameSession {
            title: "parser rewrite".into(),
        };
        let encoded = serde_json::to_string(&msg).expect("serialise");
        match serde_json::from_str::<BridgeMessage>(&encoded).expect("deserialise") {
            BridgeMessage::RenameSession { title } => assert_eq!(title, "parser rewrite"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn an_mcp_trust_prompt_carries_what_would_run() {
        // The command line is the whole basis for the decision. A client that
        // received only the server name would be asking the operator to trust
        // something they cannot see.
        let ev = BridgeEvent::McpApprovalRequest {
            request_id: "r1".into(),
            server_name: "github".into(),
            command: Some("npx -y @scope/server-github".into()),
            url: None,
        };
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"mcp_approval_request""#));

        let back: BridgeEvent = serde_json::from_str(&j).unwrap();
        match back {
            BridgeEvent::McpApprovalRequest {
                server_name,
                command,
                ..
            } => {
                assert_eq!(server_name, "github");
                assert_eq!(command.as_deref(), Some("npx -y @scope/server-github"));
            }
            other => panic!("expected an mcp approval request, got {other:?}"),
        }
    }

    #[test]
    fn an_mcp_trust_decision_survives_the_wire() {
        // Each decision does something different on the machine, so a value
        // mangled in transit would launch a refused server or persist trust
        // that was granted for one session only.
        for decision in [
            McpApprovalDecision::AllowSession,
            McpApprovalDecision::AllowAlways,
            McpApprovalDecision::Deny,
        ] {
            let msg = BridgeMessage::McpApprovalResponse {
                request_id: "r1".into(),
                decision,
            };
            let j = serde_json::to_string(&msg).unwrap();
            let back: BridgeMessage = serde_json::from_str(&j).unwrap();
            match back {
                BridgeMessage::McpApprovalResponse { decision: got, .. } => {
                    assert_eq!(got, decision)
                }
                other => panic!("expected an mcp approval response, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_command_outcome_keeps_its_severity() {
        // The client colours on `is_error` alone. If the flag were dropped in
        // transit a failed command would read as a normal confirmation.
        for is_error in [true, false] {
            let ev = BridgeEvent::Notice {
                message: "Unknown command: /foo".into(),
                is_error,
            };
            let j = serde_json::to_string(&ev).unwrap();
            assert!(j.contains(r#""type":"notice""#));

            let back: BridgeEvent = serde_json::from_str(&j).unwrap();
            match back {
                BridgeEvent::Notice {
                    message,
                    is_error: got,
                } => {
                    assert_eq!(got, is_error);
                    assert_eq!(message, "Unknown command: /foo");
                }
                other => panic!("expected a notice, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_turn_that_spent_nothing_reports_no_usage() {
        // A slash-command reply takes this path. Sending zeroed figures would
        // claim the turn cost nothing when it was never a model turn at all.
        let ev = BridgeEvent::TurnComplete {
            message_id: "cmd-1".into(),
            stop_reason: "command".into(),
            usage: None,
        };
        let j = serde_json::to_string(&ev).unwrap();
        let back: BridgeEvent = serde_json::from_str(&j).unwrap();
        assert!(matches!(
            back,
            BridgeEvent::TurnComplete { usage: None, .. }
        ));
    }
}

#[cfg(test)]
mod timeline_event_tests {
    use super::*;
    use mikmik_core::timeline::{TimelineKind, TimelineStatus};

    fn sample_row() -> TimelineRow {
        TimelineRow {
            id: "tool-1".to_string(),
            title: "Reading file: README.md".to_string(),
            kind: TimelineKind::ToolCall,
            status: TimelineStatus::Done,
            started_at_ms: 1_000,
            finished_at_ms: Some(1_450),
            token_delta_input: None,
            token_delta_output: None,
            cost_delta_usd: None,
            detail_preview: "12 lines".to_string(),
            expandable_details: "{\"file_path\":\"README.md\"}".to_string(),
        }
    }

    #[test]
    fn a_timeline_row_travels_under_its_wire_name() {
        let event = BridgeEvent::TimelineRow { row: sample_row() };
        let json = match serde_json::to_value(&event) {
            Ok(json) => json,
            Err(error) => panic!("the event should serialise: {error}"),
        };

        assert_eq!(json["type"], "timeline_row");
        assert_eq!(json["row"]["id"], "tool-1");
        assert_eq!(json["row"]["kind"], "tool_call");
        assert_eq!(json["row"]["status"], "done");
    }

    #[test]
    fn a_timeline_row_survives_the_round_trip() {
        let row = sample_row();
        let json = match serde_json::to_string(&BridgeEvent::TimelineRow { row: row.clone() }) {
            Ok(json) => json,
            Err(error) => panic!("the event should serialise: {error}"),
        };
        let decoded: BridgeEvent = match serde_json::from_str(&json) {
            Ok(decoded) => decoded,
            Err(error) => panic!("the event should decode: {error}"),
        };

        match decoded {
            BridgeEvent::TimelineRow { row: decoded } => assert_eq!(decoded, row),
            other => panic!("expected a timeline row, got {other:?}"),
        }
    }

    #[test]
    fn the_server_timings_are_the_ones_that_travel() {
        let json = match serde_json::to_value(BridgeEvent::TimelineRow { row: sample_row() }) {
            Ok(json) => json,
            Err(error) => panic!("the event should serialise: {error}"),
        };

        assert_eq!(json["row"]["started_at_ms"], 1_000);
        assert_eq!(
            json["row"]["finished_at_ms"], 1_450,
            "a client must not have to time the step itself"
        );
    }

    fn tool_end(duration_ms: Option<u64>) -> BridgeEvent {
        BridgeEvent::ToolEnd {
            tool_name: "Bash".to_string(),
            tool_id: "tool-1".to_string(),
            result: "ok".to_string(),
            is_error: false,
            duration_ms,
        }
    }

    #[test]
    fn a_finished_call_carries_how_long_it_took() {
        // The mapping used to drop it, so a remote client could only time the
        // transport, never the tool.
        let json = match serde_json::to_value(tool_end(Some(240))) {
            Ok(json) => json,
            Err(error) => panic!("the event should serialise: {error}"),
        };

        assert_eq!(json["type"], "tool_end");
        assert_eq!(json["duration_ms"], 240);
    }

    #[test]
    fn an_untimed_call_names_no_duration_at_all() {
        // Rather than zero: a call that was blocked or cancelled before it ran
        // took no time, and reporting `0ms` would read as an instant success.
        let json = match serde_json::to_value(tool_end(None)) {
            Ok(json) => json,
            Err(error) => panic!("the event should serialise: {error}"),
        };

        assert!(json.get("duration_ms").is_none(), "{json}");
    }

    #[test]
    fn a_client_written_before_the_field_still_decodes() {
        let json = r#"{"type":"tool_end","tool_name":"Bash","tool_id":"t","result":"ok","is_error":false}"#;
        let decoded: BridgeEvent = match serde_json::from_str(json) {
            Ok(decoded) => decoded,
            Err(error) => panic!("the event should decode: {error}"),
        };

        match decoded {
            BridgeEvent::ToolEnd { duration_ms, .. } => assert_eq!(duration_ms, None),
            other => panic!("expected a tool_end, got {other:?}"),
        }
    }
}
