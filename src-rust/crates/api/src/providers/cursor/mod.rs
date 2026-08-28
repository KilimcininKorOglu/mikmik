// providers/cursor — Cursor (Cursor Pro) agent-executor provider.
//
// Unlike every other provider here, Cursor is not an ordinary `LlmProvider`
// that emits tool calls for mikmik's query loop to dispatch. Cursor's server
// runs the whole agent loop over one long-lived bidirectional HTTP/2 stream and
// expects the client to execute local tools on that same stream: the server
// sends `ExecServerMessage` tool-argument frames, the client runs the matching
// mikmik tool through a `CursorExecHandlers` bridge, and writes the result back
// as an `ExecClientMessage`. Assistant text, thinking and tool activity arrive
// as `InteractionUpdate` deltas, and hosted-action gates as `InteractionQuery`.
//
// `proto` is the hand-written wire codec, `transport` the full-duplex Connect
// stream, `request` the run-request assembly, and `exec` the per-tool dispatch.
// The `CursorExecHandlers` trait is defined here in `api`; its implementation,
// which binds real mikmik tools, lives in the `query` crate because `api`
// cannot depend on `tools`.

mod exec;
pub mod proto;
mod request;
mod transport;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mikmik_core::cursor_oauth::{
    self, get_cursor_tokens, is_expired, load_cursor_tokens_for_account, save_cursor_tokens,
    save_cursor_tokens_for_account, CursorTokens,
};
use mikmik_core::provider_id::ProviderId;
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use self::proto::{InteractionUpdate, KvServerMessage, QueryCase, ServerMessage};
use self::request::BlobStore;
use self::transport::CursorConnection;
use crate::provider_error::ProviderError;
use crate::provider_types::{StopReason, StreamEvent};
use mikmik_core::types::{ContentBlock, UsageInfo};

/// The plain-text outcome of one tool the Cursor agent asked the client to run.
///
/// mikmik's tools produce text; the exec layer shapes that into whichever wire
/// result the frame expects. `is_error` selects the error variant so a refusal
/// or failure is never reported to Cursor as an empty success.
pub struct ToolExecOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolExecOutcome {
    /// A successful run carrying `text`.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// A failed run carrying an error message.
    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// The bridge Cursor's exec channel runs local tools through.
///
/// Each method maps one Cursor tool onto a mikmik tool and returns its text
/// outcome. The implementation lives in the `query` crate, where the real tools
/// and their permission context are reachable.
#[async_trait]
pub trait CursorExecHandlers: Send + Sync {
    async fn read(&self, path: &str, offset: Option<i64>, limit: Option<i64>) -> ToolExecOutcome;
    async fn write(&self, path: &str, content: &str) -> ToolExecOutcome;
    async fn edit(&self, path: &str, edits: &[(String, String)]) -> ToolExecOutcome;
    async fn delete(&self, path: &str) -> ToolExecOutcome;
    async fn ls(&self, path: &str) -> ToolExecOutcome;
    async fn grep(
        &self,
        pattern: &str,
        path: &str,
        glob: &str,
        ignore_case: bool,
    ) -> ToolExecOutcome;
    async fn find(&self, pattern: &str, path: &str) -> ToolExecOutcome;
    async fn shell(&self, command: &str, cwd: &str, timeout: Option<i64>) -> ToolExecOutcome;
    async fn diagnostics(&self, path: &str) -> ToolExecOutcome;
    async fn mcp(&self, name: &str, args_json: &str) -> ToolExecOutcome;
}

/// What one Cursor turn produced, for the query loop to record as the assistant
/// message.
pub struct CursorTurnOutcome {
    pub text: String,
    pub thinking: String,
    pub stop_reason: StopReason,
    pub usage: UsageInfo,
}

/// The Cursor agent client, holding the account's tokens.
pub struct CursorAgent {
    id: ProviderId,
    tokens: Arc<Mutex<CursorTokens>>,
    account: Option<String>,
    http_client: reqwest::Client,
}

impl CursorAgent {
    fn new(tokens: CursorTokens) -> Self {
        Self {
            id: ProviderId::new(ProviderId::CURSOR),
            tokens: Arc::new(Mutex::new(tokens)),
            account: None,
            http_client: reqwest::Client::new(),
        }
    }

    /// Construct from the active (or only) stored Cursor account.
    pub fn from_stored() -> Option<Self> {
        let tokens = get_cursor_tokens()?;
        if tokens.access_token.is_empty() {
            return None;
        }
        Some(Self::new(tokens))
    }

    /// Construct from one named account, bypassing the active pointer.
    pub fn from_account(account_id: &str) -> Option<Self> {
        let tokens = load_cursor_tokens_for_account(account_id)?;
        if tokens.access_token.is_empty() {
            return None;
        }
        let mut agent = Self::new(tokens);
        agent.account = Some(account_id.to_string());
        Some(agent)
    }

    fn snapshot(&self) -> CursorTokens {
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn persist(&self, tokens: &CursorTokens) {
        let result = match self.account.as_deref() {
            Some(account_id) => save_cursor_tokens_for_account(tokens, account_id),
            None => save_cursor_tokens(tokens),
        };
        if let Err(e) = result {
            warn!("could not persist Cursor tokens: {e}");
        }
    }

    /// The access token, refreshed first when it has expired.
    async fn access_token(&self) -> Result<String, ProviderError> {
        let current = self.snapshot();
        if !is_expired(&current) {
            return Ok(current.access_token);
        }
        let Some(refresh_token) = current.refresh_token.clone() else {
            return Ok(current.access_token);
        };
        match cursor_oauth::refresh(&refresh_token).await {
            Ok(mut refreshed) => {
                if refreshed.refresh_token.is_none() {
                    refreshed.refresh_token = current.refresh_token;
                }
                if let Ok(mut guard) = self.tokens.lock() {
                    *guard = refreshed.clone();
                }
                self.persist(&refreshed);
                Ok(refreshed.access_token)
            }
            Err(e) => {
                warn!("Cursor token refresh failed: {e}");
                Ok(current.access_token)
            }
        }
    }

    fn other(&self, message: String) -> ProviderError {
        ProviderError::Other {
            provider: self.id.clone(),
            message,
            status: None,
            body: None,
        }
    }
}

/// Drive one full Cursor agent turn, streaming events and running tools.
///
/// Cursor runs its own multi-tool loop inside this single call, so the mikmik
/// query loop invokes it once per user turn and does not re-dispatch. Events are
/// pushed as they arrive; the returned outcome is the assistant message to
/// record.
pub async fn run_turn(
    agent: &CursorAgent,
    request: &crate::provider_types::ProviderRequest,
    handlers: &dyn CursorExecHandlers,
    events: &UnboundedSender<StreamEvent>,
) -> Result<CursorTurnOutcome, ProviderError> {
    let token = agent.access_token().await?;
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let built = request::build_request(request, &conversation_id);
    let mut conn = CursorConnection::open(
        &agent.http_client,
        agent.id.clone(),
        &token,
        &built.run_request,
    )
    .await?;

    let model = request.model.to_string();
    let message_id = format!("cursor-{}", uuid::Uuid::new_v4());
    let _ = events.send(StreamEvent::MessageStart {
        id: message_id,
        model,
        usage: UsageInfo::default(),
    });

    let mut turn = TurnState::new(built.blobs, built.rules, built.tools);
    while let Some(frame) = conn.next_frame().await? {
        if frame.is_end_stream() {
            if let Some(err) = trailer_error(&frame.payload) {
                return Err(agent.other(err));
            }
            break;
        }
        let message = proto::decode_server_message(&frame.payload)
            .map_err(|e| agent.other(format!("Cursor decode failed: {e}")))?;
        dispatch(&mut turn, message, &mut conn, handlers, events).await;
    }

    finish_turn(events);
    Ok(turn.into_outcome())
}

/// Accumulated state for one turn.
struct TurnState {
    blobs: BlobStore,
    rules: Vec<Vec<u8>>,
    tools: Vec<Vec<u8>>,
    text: String,
    thinking: String,
    thinking_open: bool,
    output_tokens: u32,
    context_tokens: u32,
    stop_reason: StopReason,
}

const TEXT_INDEX: usize = 0;
const THINKING_INDEX: usize = 2000;

impl TurnState {
    fn new(blobs: BlobStore, rules: Vec<Vec<u8>>, tools: Vec<Vec<u8>>) -> Self {
        Self {
            blobs,
            rules,
            tools,
            text: String::new(),
            thinking: String::new(),
            thinking_open: false,
            output_tokens: 0,
            context_tokens: 0,
            stop_reason: StopReason::EndTurn,
        }
    }

    fn into_outcome(self) -> CursorTurnOutcome {
        CursorTurnOutcome {
            text: self.text,
            thinking: self.thinking,
            stop_reason: self.stop_reason,
            usage: UsageInfo {
                input_tokens: u64::from(self.context_tokens),
                output_tokens: u64::from(self.output_tokens),
                ..Default::default()
            },
        }
    }
}

async fn dispatch(
    turn: &mut TurnState,
    message: ServerMessage,
    conn: &mut CursorConnection,
    handlers: &dyn CursorExecHandlers,
    events: &UnboundedSender<StreamEvent>,
) {
    match message {
        ServerMessage::Interaction(update) => apply_update(turn, update, events),
        ServerMessage::Exec(exec) => {
            let reply = exec::handle_exec(&exec, handlers, &turn.rules, &turn.tools).await;
            conn.send_frame(&reply);
        }
        ServerMessage::Query(query) => {
            if let Some(reply) = interaction_reply(query.id, &query.case) {
                conn.send_frame(&reply);
            }
        }
        ServerMessage::Kv(kv) => handle_kv(turn, kv, conn),
        ServerMessage::Checkpoint { used_tokens } => {
            if let Some(tokens) = used_tokens {
                turn.context_tokens = tokens;
            }
        }
        ServerMessage::ExecAbort(_) | ServerMessage::Unknown => {}
    }
}

fn apply_update(
    turn: &mut TurnState,
    update: InteractionUpdate,
    events: &UnboundedSender<StreamEvent>,
) {
    match update {
        InteractionUpdate::TextDelta(text) => {
            turn.text.push_str(&text);
            let _ = events.send(StreamEvent::TextDelta {
                index: TEXT_INDEX,
                text,
            });
        }
        InteractionUpdate::ThinkingDelta(thinking) => {
            turn.thinking.push_str(&thinking);
            turn.thinking_open = true;
            let _ = events.send(StreamEvent::ThinkingDelta {
                index: THINKING_INDEX,
                thinking,
            });
        }
        InteractionUpdate::ThinkingCompleted => close_thinking(turn, events),
        InteractionUpdate::TokenDelta(n) => {
            turn.output_tokens = turn.output_tokens.saturating_add(n.max(0) as u32);
        }
        InteractionUpdate::TurnEnded => turn.stop_reason = StopReason::EndTurn,
        InteractionUpdate::ToolCallStarted { .. }
        | InteractionUpdate::PartialToolCall { .. }
        | InteractionUpdate::ToolCallCompleted { .. }
        | InteractionUpdate::Other => {}
    }
}

fn close_thinking(turn: &mut TurnState, events: &UnboundedSender<StreamEvent>) {
    if turn.thinking_open {
        turn.thinking_open = false;
        let _ = events.send(StreamEvent::ContentBlockStop {
            index: THINKING_INDEX,
        });
    }
}

fn handle_kv(turn: &mut TurnState, kv: KvServerMessage, conn: &mut CursorConnection) {
    match kv {
        KvServerMessage::GetBlob { id, blob_id } => {
            let data = turn.blobs.get(&blob_id).map(<[u8]>::to_vec);
            let reply = proto::encode_kv_get_blob_result_message(id, data.as_deref());
            conn.send_frame(&reply);
        }
        KvServerMessage::SetBlob {
            id,
            blob_id: _,
            blob_data,
        } => {
            turn.blobs.store(blob_data);
            let reply = proto::encode_kv_set_blob_result_message(id);
            conn.send_frame(&reply);
        }
    }
}

const NOT_IMPLEMENTED: &str = "not implemented by this client";

/// The client's answer to one hosted-action gate: approve the searches,
/// reject the interactive prompts, and leave VM setup unanswered.
fn interaction_reply(id: u32, case: &QueryCase) -> Option<Vec<u8>> {
    match case {
        QueryCase::WebSearch => Some(proto::encode_interaction_approved_message(id, 2)),
        QueryCase::ExaSearch => Some(proto::encode_interaction_approved_message(id, 5)),
        QueryCase::ExaFetch => Some(proto::encode_interaction_approved_message(id, 6)),
        QueryCase::WebFetch => Some(proto::encode_interaction_approved_message(id, 9)),
        QueryCase::AskQuestion => Some(proto::encode_ask_question_rejected_message(
            id,
            &format!("Interactive questions are {NOT_IMPLEMENTED}"),
        )),
        QueryCase::SwitchMode => Some(proto::encode_switch_mode_rejected_message(
            id,
            &format!("Mode switches are {NOT_IMPLEMENTED}"),
        )),
        QueryCase::CreatePlan => Some(proto::encode_create_plan_error_message(
            id,
            &format!("Plan files are {NOT_IMPLEMENTED}"),
        )),
        QueryCase::SetupVm => None,
        QueryCase::Unknown(field) if *field >= 2 => {
            Some(proto::encode_unknown_approved_message(id, *field))
        }
        QueryCase::Unknown(_) => None,
    }
}

fn finish_turn(events: &UnboundedSender<StreamEvent>) {
    let _ = events.send(StreamEvent::ContentBlockStop { index: TEXT_INDEX });
    let _ = events.send(StreamEvent::MessageDelta {
        stop_reason: Some(StopReason::EndTurn),
        usage: None,
    });
    let _ = events.send(StreamEvent::MessageStop);
}

/// Build the assistant content blocks from a finished turn.
pub fn outcome_blocks(outcome: &CursorTurnOutcome) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    if !outcome.thinking.is_empty() {
        blocks.push(ContentBlock::Thinking {
            thinking: outcome.thinking.clone(),
            signature: String::new(),
        });
    }
    if !outcome.text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: outcome.text.clone(),
        });
    }
    blocks
}

/// Parse a Connect end-stream JSON trailer and return its error text, if any.
fn trailer_error(payload: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(payload);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let err = parsed.get("error")?;
    let code = err.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let message = err.get("message").and_then(|v| v.as_str()).unwrap_or("");
    if code.is_empty() && message.is_empty() {
        return None;
    }
    Some(format!(
        "Cursor stream error{}: {message}",
        if code.is_empty() {
            String::new()
        } else {
            format!(" {code}")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_reply_approves_searches() {
        assert!(interaction_reply(1, &QueryCase::WebSearch).is_some());
        assert!(interaction_reply(1, &QueryCase::ExaSearch).is_some());
        assert!(interaction_reply(1, &QueryCase::WebFetch).is_some());
    }

    #[test]
    fn interaction_reply_rejects_prompts() {
        assert!(interaction_reply(1, &QueryCase::AskQuestion).is_some());
        assert!(interaction_reply(1, &QueryCase::SwitchMode).is_some());
        assert!(interaction_reply(1, &QueryCase::CreatePlan).is_some());
    }

    #[test]
    fn interaction_reply_leaves_vm_setup_unanswered() {
        assert!(interaction_reply(1, &QueryCase::SetupVm).is_none());
        assert!(interaction_reply(1, &QueryCase::Unknown(0)).is_none());
        assert!(interaction_reply(1, &QueryCase::Unknown(11)).is_some());
    }

    #[test]
    fn trailer_error_reads_code_and_message() {
        let payload = br#"{"error":{"code":"unavailable","message":"try later"}}"#;
        assert_eq!(
            trailer_error(payload),
            Some("Cursor stream error unavailable: try later".to_string())
        );
        assert_eq!(trailer_error(b"{}"), None);
    }
}
