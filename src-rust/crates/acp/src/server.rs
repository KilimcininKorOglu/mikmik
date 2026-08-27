//! Top-level ACP request / notification dispatcher.

use std::sync::Arc;

use agent_client_protocol_schema as acp;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::connection::{Connection, Inbound};
use crate::runtime::AgentRuntime;
use crate::sessions::{SessionRegistry, SessionState};

/// The ACP agent: owns the connection, the runtime, and the session registry.
pub struct AgentServer {
    pub connection: Arc<Connection>,
    pub runtime: Arc<AgentRuntime>,
    pub sessions: Arc<SessionRegistry>,
    pub client_capabilities: parking_lot::RwLock<acp::ClientCapabilities>,
}

impl AgentServer {
    pub fn new(connection: Arc<Connection>, runtime: Arc<AgentRuntime>) -> Arc<Self> {
        Arc::new(Self {
            connection,
            runtime,
            sessions: Arc::new(SessionRegistry::new()),
            client_capabilities: parking_lot::RwLock::new(acp::ClientCapabilities::default()),
        })
    }

    /// Dispatch a single inbound message. Spawns the actual handler on a
    /// background task so the reader loop stays responsive while a prompt
    /// is in flight. Returns the join handle so the caller can wait for
    /// in-flight work to finish before shutting down.
    pub fn dispatch(self: &Arc<Self>, msg: Inbound) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            match msg {
                Inbound::Request { id, method, params } => {
                    let response = this.handle_request(&method, params).await;
                    let (result, after) = match response {
                        Ok(Answer { value, after }) => {
                            (this.connection.send_response(id, value).await, after)
                        }
                        Err(err) => (
                            this.connection.send_error_response(id, err).await,
                            Vec::new(),
                        ),
                    };
                    if let Err(e) = result {
                        warn!(?e, method = %method, "ACP: failed to send response");
                    }
                    // Sent after the response on purpose: a notification about
                    // a session the client has not been told the id of yet has
                    // nowhere to land.
                    for notification in after {
                        if let Err(e) = this
                            .connection
                            .send_notification("session/update", notification)
                            .await
                        {
                            warn!(?e, method = %method, "ACP: failed to send a follow-up update");
                        }
                    }
                }
                Inbound::Notification { method, params } => {
                    this.handle_notification(&method, params).await;
                }
            }
        })
    }

    async fn handle_request(
        self: &Arc<Self>,
        method: &str,
        params: Option<Value>,
    ) -> Result<Answer, acp::Error> {
        debug!(method, "ACP: dispatch request");
        match method {
            "initialize" => {
                let req: acp::InitializeRequest = parse_params(params)?;
                let result = self.on_initialize(req).await?;
                answer(result)
            }
            "authenticate" => {
                let _req: acp::AuthenticateRequest = parse_params(params)?;
                // MikMik uses local credentials; clients don't need to authenticate.
                answer(acp::AuthenticateResponse::default())
            }
            "session/new" => {
                let req: acp::NewSessionRequest = parse_params(params)?;
                let result = self.on_new_session(req).await?;
                let id = result.session_id.clone();
                Ok(Answer {
                    after: vec![announce_commands(id)],
                    ..answer(result)?
                })
            }
            "session/load" => {
                let req: acp::LoadSessionRequest = parse_params(params)?;
                let id = req.session_id.clone();
                let result = self.on_load_session(req).await?;
                Ok(Answer {
                    after: vec![announce_commands(id)],
                    ..answer(result)?
                })
            }
            "session/resume" => {
                let req: acp::ResumeSessionRequest = parse_params(params)?;
                let id = req.session_id.clone();
                let result = self.on_resume_session(req).await?;
                Ok(Answer {
                    after: vec![announce_commands(id)],
                    ..answer(result)?
                })
            }
            "session/list" => {
                let req: acp::ListSessionsRequest = parse_params(params)?;
                let result = self.on_list_sessions(req).await?;
                answer(result)
            }
            "session/fork" => {
                let req: acp::ForkSessionRequest = parse_params(params)?;
                let result = self.on_fork_session(req).await?;
                let id = result.session_id.clone();
                Ok(Answer {
                    after: vec![announce_commands(id)],
                    ..answer(result)?
                })
            }
            "session/close" => {
                let req: acp::CloseSessionRequest = parse_params(params)?;
                let result = self.on_close_session(req).await?;
                answer(result)
            }
            "session/prompt" => {
                let req: acp::PromptRequest = parse_params(params)?;
                let result = self.on_prompt(req).await?;
                answer(result)
            }
            "session/set_mode" => {
                let req: acp::SetSessionModeRequest = parse_params(params)?;
                let result = self.on_set_mode(req).await?;
                answer(result)
            }
            "session/set_model" => {
                let req: acp::SetSessionModelRequest = parse_params(params)?;
                let result = self.on_set_model(req).await?;
                answer(result)
            }
            "session/set_config_option" => {
                let req: acp::SetSessionConfigOptionRequest = parse_params(params)?;
                let result = self.on_set_config_option(req).await?;
                answer(result)
            }
            other => {
                warn!(method = other, "ACP: method not found");
                Err(acp::Error::method_not_found())
            }
        }
    }

    async fn handle_notification(self: &Arc<Self>, method: &str, params: Option<Value>) {
        debug!(method, "ACP: dispatch notification");
        match method {
            "session/cancel" => {
                let parsed: Result<acp::CancelNotification, _> = params
                    .map(serde_json::from_value)
                    .unwrap_or(Err(serde::de::Error::custom("missing params")));
                match parsed {
                    Ok(notif) => {
                        if let Some(session) = self.sessions.get(&notif.session_id) {
                            info!(session_id = %notif.session_id, "ACP: cancelling session");
                            // Only the turn that is running is affected: the
                            // next prompt calls `begin_turn`, which installs a
                            // token this cancellation never touched.
                            session.cancel();
                        }
                    }
                    Err(e) => warn!(?e, "ACP: malformed session/cancel notification"),
                }
            }
            other => {
                warn!(method = other, "ACP: ignoring unknown notification");
            }
        }
    }

    async fn on_initialize(
        self: &Arc<Self>,
        req: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        info!(
            client_version = ?req.client_info.as_ref().map(|i| (&i.name, &i.version)),
            "ACP: initialize"
        );
        *self.client_capabilities.write() = req.client_capabilities.clone();

        let agent_info = acp::Implementation::new("mikmik", env!("CARGO_PKG_VERSION"))
            .title(Some("MikMik".to_string()));

        let mut response = acp::InitializeResponse::new(acp::ProtocolVersion::V1)
            .agent_capabilities(
                acp::AgentCapabilities::new()
                    // Every turn is filed, so a session outlives the process
                    // and can be handed back in full or reopened silently.
                    .load_session(true)
                    .session_capabilities(
                        acp::SessionCapabilities::new()
                            .list(Some(acp::SessionListCapabilities::new()))
                            .fork(Some(acp::SessionForkCapabilities::new()))
                            .resume(Some(acp::SessionResumeCapabilities::new()))
                            .close(Some(acp::SessionCloseCapabilities::new())),
                    )
                    .prompt_capabilities(
                        // Embedded resources and images reach the model:
                        // `render_prompt_blocks` reads a resource link, an
                        // inline resource, and an image. Audio stays
                        // unadvertised because the internal message type has
                        // no audio block to carry it in, and claiming
                        // otherwise would lose what the user attached.
                        acp::PromptCapabilities::new()
                            .embedded_context(true)
                            .image(true),
                    )
                    // Stdio is implied by the protocol; http and sse are said
                    // out loud because a session can now be opened against
                    // either, and the headers a client attaches to one are
                    // carried through to the transport.
                    .mcp_capabilities(acp::McpCapabilities::new().http(true).sse(true)),
            );
        response = response.agent_info(Some(agent_info));
        Ok(response)
    }

    async fn on_new_session(
        self: &Arc<Self>,
        req: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        if !req.cwd.is_absolute() {
            return Err(acp::Error::invalid_params().data(Some(
                serde_json::json!({ "reason": "cwd must be absolute" }),
            )));
        }
        let session_id = acp::SessionId::new(format!("acp-{}", uuid::Uuid::new_v4()));
        let state = SessionState::new(session_id.clone(), req.cwd.clone());
        info!(session_id = %session_id, cwd = %req.cwd.display(), "ACP: new session");

        *state.mcp.lock() = self.session_mcp(&req.mcp_servers).await?;

        self.sessions.insert(state.clone());
        Ok(acp::NewSessionResponse::new(session_id)
            .modes(Some(self.mode_state_for(&state)))
            .models(Some(self.model_state_for(&state)))
            .config_options(Some(self.config_options_for(&state))))
    }

    /// Reopen a stored session and hand the whole conversation back.
    ///
    /// The client draws a transcript it never saw being written, so the
    /// history is replayed as `session/update` notifications before this
    /// answers: the protocol requires the updates to arrive first.
    async fn on_load_session(
        self: &Arc<Self>,
        req: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let session = self
            .reopen_with_mcp(&req.session_id, &req.cwd, &req.mcp_servers)
            .await?;

        let messages = session.messages.lock().clone();
        for update in crate::replay::updates_for(&messages) {
            let notification = acp::SessionNotification::new(req.session_id.clone(), update);
            if let Err(e) = self
                .connection
                .send_notification("session/update", notification)
                .await
            {
                warn!(?e, "ACP: failed to replay a stored session");
                return Err(acp::Error::internal_error());
            }
        }
        info!(
            session_id = %req.session_id,
            messages = messages.len(),
            "ACP: session loaded and replayed"
        );

        Ok(acp::LoadSessionResponse::new()
            .modes(Some(self.mode_state_for(&session)))
            .models(Some(self.model_state_for(&session)))
            .config_options(Some(self.config_options_for(&session))))
    }

    /// Reopen a stored session and keep its context, without handing the
    /// conversation back. A client that draws its own transcript asks for this
    /// instead of `session/load`.
    async fn on_resume_session(
        self: &Arc<Self>,
        req: acp::ResumeSessionRequest,
    ) -> Result<acp::ResumeSessionResponse, acp::Error> {
        let session = self
            .reopen_with_mcp(&req.session_id, &req.cwd, &req.mcp_servers)
            .await?;
        info!(session_id = %req.session_id, "ACP: session resumed");

        Ok(acp::ResumeSessionResponse::new()
            .modes(Some(self.mode_state_for(&session)))
            .models(Some(self.model_state_for(&session)))
            .config_options(Some(self.config_options_for(&session))))
    }

    /// Split a session in two: a new one carrying the conversation so far,
    /// leaving the original untouched.
    ///
    /// The fork keeps whatever the source session had chosen for itself (its
    /// model, its account, its effort, its permission mode), because it
    /// continues the same conversation.
    async fn on_fork_session(
        self: &Arc<Self>,
        req: acp::ForkSessionRequest,
    ) -> Result<acp::ForkSessionResponse, acp::Error> {
        let source = reopen(&self.sessions, &req.session_id, &req.cwd).await?;

        let model = self.runtime.query_config.model.clone();
        let forked = fork_from(&self.sessions, &source, req.cwd.clone(), &model);
        // Servers the fork named are its own; otherwise it continues the same
        // conversation, so it continues against the same servers.
        *forked.mcp.lock() = match self.session_mcp(&req.mcp_servers).await? {
            Some(own) => Some(own),
            None => source.mcp.lock().clone(),
        };
        let forked_id = forked.session_id.clone();
        crate::persist::save(&forked, &model).await;
        info!(
            session_id = %forked_id,
            forked_from = %req.session_id,
            messages = forked.messages.lock().len(),
            "ACP: session forked"
        );

        Ok(acp::ForkSessionResponse::new(forked_id)
            .modes(Some(self.mode_state_for(&forked)))
            .models(Some(self.model_state_for(&forked)))
            .config_options(Some(self.config_options_for(&forked))))
    }

    /// Every session on file, so a client can offer to reopen one.
    async fn on_list_sessions(
        self: &Arc<Self>,
        req: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        if let Some(cwd) = &req.cwd {
            if !cwd.is_absolute() {
                return Err(acp::Error::invalid_params().data(Some(
                    serde_json::json!({ "reason": "cwd must be absolute" }),
                )));
            }
        }

        let listing = mikmik_core::history::list_sessions().await;
        for failure in &listing.unreadable {
            warn!(
                path = %failure.path.display(),
                error = %failure.error,
                "ACP: session file could not be read"
            );
        }
        let stored = listing.sessions;
        let page = crate::listing::page(&stored, req.cwd.as_deref(), req.cursor.as_deref())
            .map_err(|reason| {
                acp::Error::invalid_params().data(Some(serde_json::json!({
                    "reason": reason,
                    "cursor": req.cursor,
                })))
            })?;
        debug!(
            listed = page.sessions.len(),
            stored = stored.len(),
            "ACP: sessions listed"
        );

        Ok(acp::ListSessionsResponse::new(page.sessions).next_cursor(page.next_cursor))
    }

    /// Let go of a session: stop whatever it is doing, write it out, and drop
    /// it from the registry. What is on disk stays, so it can be loaded again.
    async fn on_close_session(
        self: &Arc<Self>,
        req: acp::CloseSessionRequest,
    ) -> Result<acp::CloseSessionResponse, acp::Error> {
        let session = self.session_or_error(&req.session_id)?;
        session.cancel();
        crate::persist::save(&session, &self.runtime.query_config.model).await;
        self.sessions.remove(&req.session_id);
        info!(session_id = %req.session_id, "ACP: session closed");
        Ok(acp::CloseSessionResponse::new())
    }

    /// The modes a session offers, with the one it is actually in marked.
    ///
    /// A session that changed its own mode reports that mode, not the one the
    /// runtime started in, so a reopened or forked session is not drawn as
    /// something it is not.
    fn mode_state_for(&self, session: &Arc<SessionState>) -> acp::SessionModeState {
        let current = session
            .settings
            .lock()
            .permission_mode
            .unwrap_or(self.runtime.config.permission_mode);
        crate::session_config::mode_state(&current)
    }

    /// The options a session currently offers, rebuilt from its overrides.
    fn config_options_for(&self, session: &Arc<SessionState>) -> Vec<acp::SessionConfigOption> {
        let overrides = session.settings.lock().clone();
        let mut config = self.runtime.config.clone();
        crate::session_config::apply_overrides(&mut config, &overrides);
        let effort = overrides.effort.or(self.runtime.query_config.effort_level);
        // The turn sends the runtime's resolved model unless the session says
        // otherwise; `config.model` is often unset and would resolve to a
        // fallback the session is not using.
        let model = overrides
            .model
            .clone()
            .unwrap_or_else(|| self.runtime.query_config.model.clone());
        crate::session_config::config_options(&config, &self.runtime.model_registry, &model, effort)
    }

    /// The models a session can be switched to, with the one it uses marked.
    fn model_state_for(&self, session: &Arc<SessionState>) -> acp::SessionModelState {
        let overrides = session.settings.lock().clone();
        let mut config = self.runtime.config.clone();
        crate::session_config::apply_overrides(&mut config, &overrides);
        let model = overrides
            .model
            .clone()
            .unwrap_or_else(|| self.runtime.query_config.model.clone());
        crate::session_config::model_state(&config, &self.runtime.model_registry, &model)
    }

    /// Pick the model this session sends to.
    ///
    /// The same override the `model` configuration option writes, so the two
    /// selectors a client may show cannot drift apart, and both are announced
    /// whichever one was used.
    async fn on_set_model(
        self: &Arc<Self>,
        req: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        let session = self.session_or_error(&req.session_id)?;
        let model_id = req.model_id.0.to_string();
        session.settings.lock().model = Some(model_id.clone());
        info!(session_id = %req.session_id, model = %model_id, "ACP: session model changed");

        self.announce_options(&req.session_id, self.config_options_for(&session))
            .await;
        Ok(acp::SetSessionModelResponse::new())
    }

    /// Restate a session's options to the client, so a view that did not make
    /// the change still shows what the session is set to now.
    async fn announce_options(
        self: &Arc<Self>,
        session_id: &acp::SessionId,
        options: Vec<acp::SessionConfigOption>,
    ) {
        let update = acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(options));
        let notification = acp::SessionNotification::new(session_id.clone(), update);
        if let Err(e) = self
            .connection
            .send_notification("session/update", notification)
            .await
        {
            warn!(?e, "ACP: failed to announce the configuration change");
        }
    }

    /// Change the model, the account, or the reasoning effort for this session
    /// alone. Session-scoped: nothing is written to `settings.json`.
    async fn on_set_config_option(
        self: &Arc<Self>,
        req: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        let session = self.session_or_error(&req.session_id)?;
        let option_id = req.config_id.0.to_string();
        let value = req.value.0.to_string();

        {
            let mut overrides = session.settings.lock();
            let mut config = self.runtime.config.clone();
            crate::session_config::apply_overrides(&mut config, &overrides);
            if let Err(reason) = crate::session_config::apply_config_option(
                &mut overrides,
                &config,
                &self.runtime.model_registry,
                &option_id,
                &value,
            ) {
                return Err(acp::Error::invalid_params().data(Some(serde_json::json!({
                    "reason": reason,
                    "configId": option_id,
                    "value": value,
                }))));
            }
        }
        info!(
            session_id = %req.session_id,
            option = %option_id,
            value = %value,
            "ACP: session configuration changed"
        );

        // Changing one option restates the other two: the model list belongs
        // to the account, and the effort ladder belongs to the model.
        let options = self.config_options_for(&session);
        self.announce_options(&req.session_id, options.clone())
            .await;

        Ok(acp::SetSessionConfigOptionResponse::new(options))
    }

    /// Switch how this session answers permission requests. Session-scoped:
    /// nothing is written to `settings.json`.
    async fn on_set_mode(
        self: &Arc<Self>,
        req: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        let session = self.session_or_error(&req.session_id)?;
        let mode_id = req.mode_id.0.as_ref();
        let Some(mode) = crate::session_config::permission_mode_for(mode_id) else {
            return Err(acp::Error::invalid_params().data(Some(serde_json::json!({
                "reason": "unknown mode",
                "modeId": mode_id,
            }))));
        };

        session.settings.lock().permission_mode = Some(mode);
        info!(session_id = %req.session_id, mode = mode_id, "ACP: session mode changed");

        // Say it out loud as well as answering: a client with more than one
        // view of the session updates all of them from the notification.
        let update =
            acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(req.mode_id.clone()));
        let notification = acp::SessionNotification::new(req.session_id.clone(), update);
        if let Err(e) = self
            .connection
            .send_notification("session/update", notification)
            .await
        {
            warn!(?e, "ACP: failed to announce the mode change");
        }

        Ok(acp::SetSessionModeResponse::new())
    }

    /// Look a session up, or report the id back as invalid.
    fn session_or_error(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<Arc<SessionState>, acp::Error> {
        self.sessions.get(session_id).ok_or_else(|| {
            acp::Error::invalid_params().data(Some(serde_json::json!({
                "reason": "unknown session",
                "sessionId": session_id,
            })))
        })
    }

    /// Reopen a session, giving it the MCP servers the request named.
    ///
    /// A session that is already live keeps the servers it was opened with
    /// unless this request named its own: reconnecting under a client that
    /// asked for nothing in particular would drop the tools a turn may be
    /// using right now.
    async fn reopen_with_mcp(
        self: &Arc<Self>,
        session_id: &acp::SessionId,
        cwd: &std::path::Path,
        servers: &[acp::McpServer],
    ) -> Result<Arc<SessionState>, acp::Error> {
        let session = reopen(&self.sessions, session_id, cwd).await?;
        if let Some(own) = self.session_mcp(servers).await? {
            *session.mcp.lock() = Some(own);
        }
        Ok(session)
    }

    /// Connect the MCP servers a request named, for that session alone.
    ///
    /// A request that named none answers `None`, and that session runs with
    /// the agent's own roster: a client relying on the agent's configuration
    /// must not be handed an empty one.
    async fn session_mcp(
        self: &Arc<Self>,
        servers: &[acp::McpServer],
    ) -> Result<Option<crate::mcp::SessionMcp>, acp::Error> {
        crate::mcp::connect(servers, &self.runtime.config, &self.runtime.working_dir).await
    }

    async fn on_prompt(
        self: &Arc<Self>,
        req: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        let session = self.session_or_error(&req.session_id)?;
        // What the client offered to host for this session. Read per prompt
        // rather than stored, so a client that reconnects with different
        // capabilities is believed.
        let editor = crate::editor::AcpEditorHost::for_session(
            self.connection.clone(),
            req.session_id.clone(),
            &self.client_capabilities.read().clone(),
        );
        // One turn at a time per session: the second would clone the same
        // transcript, run against it, and write its own copy back over the
        // first. A client with two prompts to make opens two sessions.
        let turn = session.begin_turn().ok_or_else(|| {
            acp::Error::invalid_request().data(Some(serde_json::json!({
                "reason": "a turn is already running on this session",
                "sessionId": req.session_id,
            })))
        })?;
        crate::prompt::handle(
            self.runtime.clone(),
            self.connection.clone(),
            session,
            turn,
            editor,
            req,
        )
        .await
    }
}

/// Register a copy of `source` under a new id, carrying its conversation and
/// its choices, and remembering where the two parted.
fn fork_from(
    sessions: &SessionRegistry,
    source: &Arc<SessionState>,
    cwd: std::path::PathBuf,
    model: &str,
) -> Arc<SessionState> {
    let snapshot = crate::persist::snapshot(source, model);
    let forked_id = acp::SessionId::new(format!("acp-{}", uuid::Uuid::new_v4()));
    let forked = SessionState::forked(forked_id, cwd, &snapshot);
    *forked.settings.lock() = source.settings.lock().clone();
    sessions.insert(forked.clone());
    forked
}

/// Put a session back in the registry: the live one if it is still there,
/// otherwise the one on disk. An id nobody ever wrote is reported back.
///
/// A free function over the registry rather than a method, so the rule can be
/// exercised without standing up a whole runtime.
async fn reopen(
    sessions: &SessionRegistry,
    session_id: &acp::SessionId,
    cwd: &std::path::Path,
) -> Result<Arc<SessionState>, acp::Error> {
    if !cwd.is_absolute() {
        return Err(acp::Error::invalid_params().data(Some(
            serde_json::json!({ "reason": "cwd must be absolute" }),
        )));
    }

    if let Some(live) = sessions.get(session_id) {
        return Ok(live);
    }

    let stored = crate::persist::load(session_id.0.as_ref())
        .await
        .ok_or_else(|| {
            acp::Error::invalid_params().data(Some(serde_json::json!({
                "reason": "unknown session",
                "sessionId": session_id,
            })))
        })?;
    let session = SessionState::restored(session_id.clone(), cwd.to_path_buf(), &stored);
    sessions.insert(session.clone());
    Ok(session)
}

/// What a request is answered with: the result, and anything the client is
/// told once it has that result in hand.
struct Answer {
    value: Value,
    after: Vec<acp::SessionNotification>,
}

/// Tell a session which slash commands it can run.
///
/// Sent after the response rather than before it, because the client only
/// learns the session id from that response. Every way of opening a session
/// says this, not just `session/new`: a client that loaded, resumed or forked
/// one is just as able to run `/help`.
fn announce_commands(session_id: acp::SessionId) -> acp::SessionNotification {
    acp::SessionNotification::new(
        session_id,
        acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
            crate::commands::available_commands(),
        )),
    )
}

/// The plain case: a result and nothing to follow it.
fn answer<T: serde::Serialize>(result: T) -> Result<Answer, acp::Error> {
    Ok(Answer {
        value: serde_json::to_value(result).map_err(|_| acp::Error::internal_error())?,
        after: Vec::new(),
    })
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, acp::Error> {
    let value = params.ok_or_else(acp::Error::invalid_params)?;
    serde_json::from_value(value).map_err(|e| {
        acp::Error::invalid_params().data(Some(
            serde_json::json!({ "deserialize_error": e.to_string() }),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::types::Message;
    use std::path::{Path, PathBuf};

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Params {
        name: String,
    }

    /// `MIKMIK_HOME` is process-wide, so the tests that move it run one at a
    /// time and put it back when they are done.
    static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct HomeGuard {
        previous: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl HomeGuard {
        fn set() -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let previous = std::env::var_os("MIKMIK_HOME");
            unsafe { std::env::set_var("MIKMIK_HOME", dir.path()) };
            Self {
                previous,
                _dir: dir,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var("MIKMIK_HOME", v) },
                None => unsafe { std::env::remove_var("MIKMIK_HOME") },
            }
        }
    }

    #[tokio::test]
    async fn a_relative_directory_is_refused_rather_than_resolved() {
        // Resolving it against the agent's own cwd would open the session
        // somewhere the client never named.
        let sessions = SessionRegistry::new();
        let Err(error) = reopen(
            &sessions,
            &acp::SessionId::new("acp-1"),
            Path::new("relative/path"),
        )
        .await
        else {
            panic!("a relative cwd must be refused");
        };

        assert_eq!(error.code, acp::ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn an_id_nobody_ever_wrote_is_reported_back() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::set();

        let sessions = SessionRegistry::new();
        let Err(error) = reopen(
            &sessions,
            &acp::SessionId::new("acp-missing"),
            Path::new("/tmp"),
        )
        .await
        else {
            panic!("an unknown session must be refused");
        };

        assert_eq!(error.code, acp::ErrorCode::InvalidParams);
        let data = error.data.expect("the error names the session");
        assert_eq!(data["reason"], "unknown session");
    }

    #[tokio::test]
    async fn a_session_still_running_is_reopened_as_itself() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::set();

        let sessions = SessionRegistry::new();
        let id = acp::SessionId::new("acp-live");
        let live = SessionState::new(id.clone(), PathBuf::from("/tmp/live"));
        sessions.insert(live.clone());

        let reopened = reopen(&sessions, &id, Path::new("/tmp/elsewhere"))
            .await
            .expect("a live session reopens");

        // The same state, not a second copy: a turn in flight keeps its
        // transcript and its cancel token.
        assert!(Arc::ptr_eq(&reopened, &live));
    }

    #[tokio::test]
    async fn a_session_that_named_no_servers_is_left_on_the_agents_roster() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::set();

        let sessions = SessionRegistry::new();
        let id = acp::SessionId::new("acp-live");
        sessions.insert(SessionState::new(id.clone(), PathBuf::from("/tmp/live")));

        let reopened = reopen(&sessions, &id, Path::new("/tmp/live"))
            .await
            .expect("a live session reopens");

        assert!(
            reopened.mcp.lock().is_none(),
            "a session with no servers of its own shares the agent's roster"
        );
    }

    #[test]
    fn a_fork_carries_the_conversation_and_the_choices_but_not_the_id() {
        let sessions = SessionRegistry::new();
        let source = SessionState::new(
            acp::SessionId::new("acp-source"),
            PathBuf::from("/tmp/source"),
        );
        *source.messages.lock() = vec![Message::user("one"), Message::assistant("two")];
        source.settings.lock().model = Some("claude-opus-4".to_string());
        source.settings.lock().permission_mode = Some(mikmik_core::PermissionMode::AcceptEdits);

        let forked = fork_from(&sessions, &source, PathBuf::from("/tmp/fork"), "m");

        assert_ne!(forked.session_id, source.session_id);
        assert_eq!(forked.messages.lock().len(), 2);
        assert_eq!(
            forked.settings.lock().model.as_deref(),
            Some("claude-opus-4")
        );
        assert_eq!(
            forked.settings.lock().permission_mode,
            Some(mikmik_core::PermissionMode::AcceptEdits)
        );
        assert_eq!(
            forked.forked_from,
            Some(("acp-source".to_string(), 2)),
            "the fork records where it split"
        );
        assert!(sessions.get(&forked.session_id).is_some());
    }

    #[test]
    fn a_fork_and_its_source_then_move_apart() {
        let sessions = SessionRegistry::new();
        let source = SessionState::new(
            acp::SessionId::new("acp-source"),
            PathBuf::from("/tmp/source"),
        );
        *source.messages.lock() = vec![Message::user("one")];

        let forked = fork_from(&sessions, &source, PathBuf::from("/tmp/source"), "m");
        forked.messages.lock().push(Message::user("only mine"));

        assert_eq!(source.messages.lock().len(), 1);
        assert_eq!(forked.messages.lock().len(), 2);
    }

    #[tokio::test]
    async fn a_stored_session_comes_back_with_its_transcript() {
        let _lock = HOME_LOCK.lock().await;
        let _home = HomeGuard::set();

        let stored = SessionState::new(
            acp::SessionId::new("acp-stored"),
            PathBuf::from("/tmp/stored"),
        );
        *stored.messages.lock() = vec![Message::user("hello"), Message::assistant("hi")];
        *stored.title.lock() = Some("greeting".to_string());
        crate::persist::save(&stored, "m").await;

        let sessions = SessionRegistry::new();
        let reopened = reopen(
            &sessions,
            &acp::SessionId::new("acp-stored"),
            Path::new("/tmp/stored"),
        )
        .await
        .expect("a stored session reopens");

        assert_eq!(reopened.messages.lock().len(), 2);
        assert_eq!(reopened.title.lock().as_deref(), Some("greeting"));
        // And it is registered, so the next prompt finds it without a reload.
        assert!(sessions.get(&acp::SessionId::new("acp-stored")).is_some());
    }

    #[test]
    fn a_request_without_params_is_rejected() {
        let error = parse_params::<Params>(None).expect_err("None must not deserialize");
        assert_eq!(error.code, acp::ErrorCode::InvalidParams);
    }

    #[test]
    fn a_matching_shape_deserializes() {
        let parsed: Params = parse_params(Some(serde_json::json!({ "name": "mikmik" })))
            .expect("a matching shape parses");
        assert_eq!(
            parsed,
            Params {
                name: "mikmik".to_string()
            }
        );
    }

    #[test]
    fn a_mismatched_shape_reports_why() {
        // The editor sees only what `data` carries, so a bare code would leave
        // the user with no way to tell which field was wrong.
        let error = parse_params::<Params>(Some(serde_json::json!({ "wrong_field": 1 })))
            .expect_err("a mismatched shape must not deserialize");

        assert_eq!(error.code, acp::ErrorCode::InvalidParams);
        let data = error.data.expect("the error carries the parse failure");
        assert!(
            data["deserialize_error"].is_string(),
            "expected a deserialize_error string, got {data}"
        );
    }
}
