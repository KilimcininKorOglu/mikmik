// providers/devin — Devin / Windsurf Cascade provider.
//
// Cascade chat runs over the Connect protocol on `server.codeium.com`: a
// gzip-compressed protobuf `GetChatMessageRequest` in a single Connect frame,
// answered by a stream of `GetChatMessageResponse` frames. Auth is two-step —
// the stored session token is exchanged for a short-lived user JWT via the
// unary `GetUserJwt` RPC before each turn.
//
// The provider is an ordinary `LlmProvider`: it emits tool calls and mikmik's
// query loop dispatches them, feeding results back as TOOL prompts on the next
// turn. The wire codecs live in `proto`; framing and gzip in
// `crate::protocol::connect`.

mod proto;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use futures::{Stream, StreamExt};
use mikmik_core::devin_oauth::{
    self, is_expired, load_devin_tokens_for_account, save_devin_tokens,
    save_devin_tokens_for_account, DevinTokens,
};
use mikmik_core::provider_id::ProviderId;
use mikmik_core::types::{
    ContentBlock, Message, MessageContent, Role, ToolResultContent, UsageInfo,
};
use std::io::{Read, Write};
use tracing::{debug, warn};

use crate::protocol::connect::{encode_frame, ConnectDecoder, FLAG_COMPRESSED};
use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamEvent, SystemPrompt, SystemPromptStyle,
};

use proto::{ChatRequest, Configuration, Image, Prompt, ToolCall, ToolDefinition};

const DEVIN_API_URL: &str = "https://server.codeium.com";
const CHAT_MESSAGE_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const AUTH_PATH: &str = "/exa.auth_pb.AuthService/GetUserJwt";
const SESSION_TOKEN_PREFIX: &str = "devin-session-token$";
const DEFAULT_STOP_PATTERNS: &[&str] = &[
    "<|user|>",
    "<|bot|>",
    "<|context_request|>",
    "<|endoftext|>",
    "<|end_of_turn|>",
];

pub struct DevinProvider {
    id: ProviderId,
    tokens: Arc<Mutex<DevinTokens>>,
    account: Option<String>,
    http_client: reqwest::Client,
}

impl DevinProvider {
    fn new(tokens: DevinTokens) -> Self {
        Self {
            id: ProviderId::new(ProviderId::DEVIN),
            tokens: Arc::new(Mutex::new(tokens)),
            account: None,
            http_client: reqwest::Client::new(),
        }
    }

    /// Construct from the active (or only) stored Devin account.
    pub fn from_stored() -> Option<Self> {
        let tokens = devin_oauth::get_devin_tokens()?;
        if tokens.session_token.is_empty() {
            return None;
        }
        Some(Self::new(tokens))
    }

    /// Construct from one named account, bypassing the active pointer.
    pub fn from_account(account_id: &str) -> Option<Self> {
        let tokens = load_devin_tokens_for_account(account_id)?;
        if tokens.session_token.is_empty() {
            return None;
        }
        let mut provider = Self::new(tokens);
        provider.account = Some(account_id.to_string());
        Some(provider)
    }

    fn session_token(&self) -> String {
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_token
            .clone()
    }

    /// The session token with the wire prefix the metadata field expects.
    fn prefixed_session_token(&self) -> String {
        let token = self.session_token();
        if token.starts_with(SESSION_TOKEN_PREFIX) {
            token
        } else {
            format!("{SESSION_TOKEN_PREFIX}{token}")
        }
    }

    fn warn_if_expired(&self) {
        let guard = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        if is_expired(&guard) {
            warn!("Devin session token is expired; the user may need to re-login");
        }
    }

    /// Exchange the session token for a short-lived user JWT.
    async fn fetch_user_jwt(&self) -> Result<(String, Option<String>), ProviderError> {
        let api_key = self.prefixed_session_token();
        let body = proto::encode_user_jwt_request(&api_key);
        let resp = self
            .http_client
            .post(format!("{DEVIN_API_URL}{AUTH_PATH}"))
            .header("content-type", "application/proto")
            .header("connect-protocol-version", "1")
            .header("accept", "*/*")
            .body(body)
            .send()
            .await
            .map_err(|e| self.server_error(None, e.to_string()))?;

        let status = resp.status().as_u16();
        let raw = resp
            .bytes()
            .await
            .map_err(|e| self.server_error(Some(status), e.to_string()))?;
        if status >= 400 {
            return Err(self.other(
                Some(status),
                format!("Devin auth error {status}"),
                Some(String::from_utf8_lossy(&raw).into_owned()),
            ));
        }

        let decoded = proto::decode_user_jwt_response(&raw)
            .or_else(|_| {
                gunzip(&raw)
                    .and_then(|b| proto::decode_user_jwt_response(&b).map_err(|e| e.to_string()))
                    .map_err(map_proto_err)
            })
            .map_err(|e| self.other(None, format!("Devin auth decode failed: {e}"), None))?;

        if decoded.user_jwt.is_empty() {
            return Err(self.other(
                None,
                "Devin auth returned an empty user JWT".to_string(),
                None,
            ));
        }
        let base = if decoded.custom_api_server_url.trim().is_empty() {
            None
        } else {
            Some(
                decoded
                    .custom_api_server_url
                    .trim()
                    .trim_end_matches('/')
                    .to_string(),
            )
        };
        Ok((decoded.user_jwt, base))
    }

    fn server_error(&self, status: Option<u16>, message: String) -> ProviderError {
        ProviderError::ServerError {
            provider: self.id.clone(),
            status,
            message,
            is_retryable: true,
        }
    }

    fn other(&self, status: Option<u16>, message: String, body: Option<String>) -> ProviderError {
        ProviderError::Other {
            provider: self.id.clone(),
            message,
            status,
            body,
        }
    }

    fn persist_tokens(&self, updated: &DevinTokens) {
        let result = match self.account.as_deref() {
            Some(account_id) => save_devin_tokens_for_account(updated, account_id),
            None => save_devin_tokens(updated),
        };
        if let Err(e) = result {
            warn!("could not persist Devin tokens: {e}");
        }
    }

    fn build_request(&self, request: &ProviderRequest, user_jwt: &str, api_key: &str) -> Vec<u8> {
        let cascade_id = uuid_v4();
        let prompt = system_prompt_text(request.system_prompt.as_ref());
        let prompts = messages_to_prompts(&request.messages, &cascade_id);

        let mut stop_patterns: Vec<String> = DEFAULT_STOP_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect();
        stop_patterns.extend(request.stop_sequences.iter().cloned());

        let tools = request
            .tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name.clone(),
                description: t.description.clone(),
                json_schema_string: t.input_schema.to_string(),
                strict: false,
            })
            .collect();

        let chat = ChatRequest {
            api_key,
            user_jwt,
            prompt,
            prompts,
            chat_model_uid: request.model.to_string(),
            cascade_id: cascade_id.clone(),
            execution_id: uuid_v4(),
            configuration: Configuration {
                max_tokens: u64::from(request.max_tokens.max(1)),
                temperature: request.temperature.unwrap_or(0.4),
                top_p: request.top_p.unwrap_or(1.0),
                stop_patterns,
            },
            tools,
        };
        chat.encode()
    }
}

#[async_trait]
impl LlmProvider for DevinProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "Devin"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let model = request.model.to_string();
        let mut stream = self.create_message_stream(request).await?;
        let mut text = String::new();
        let mut tool_args: HashMap<usize, (String, String, String)> = HashMap::new();
        let mut order: Vec<usize> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = UsageInfo::default();

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta { text: t, .. } => text.push_str(&t),
                StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::ToolUse { id, name, .. },
                } => {
                    tool_args.insert(index, (id, name, String::new()));
                    order.push(index);
                }
                StreamEvent::InputJsonDelta {
                    index,
                    partial_json,
                } => {
                    tool_args
                        .entry(index)
                        .or_insert_with(|| (String::new(), String::new(), String::new()))
                        .2
                        .push_str(&partial_json);
                }
                StreamEvent::MessageDelta {
                    stop_reason: Some(sr),
                    usage: u,
                } => {
                    stop_reason = sr;
                    if let Some(u) = u {
                        usage = u;
                    }
                }
                _ => {}
            }
        }

        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::Text { text });
        }
        for index in order {
            if let Some((id, name, args)) = tool_args.remove(&index) {
                let input = serde_json::from_str(&args).unwrap_or(serde_json::json!({}));
                blocks.push(ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature: None,
                });
            }
        }

        Ok(ProviderResponse {
            id: format!("devin-{}", uuid_v4()),
            content: blocks,
            stop_reason,
            usage,
            model,
        })
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.warn_if_expired();
        let (user_jwt, custom_base) = self.fetch_user_jwt().await?;
        // The session token also flows fresh through here; persist if the JWT
        // exchange reported nothing new (identity/expiry unchanged).
        let tokens_snapshot = self
            .tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        self.persist_tokens(&tokens_snapshot);

        let api_key = self.prefixed_session_token();
        let body = self.build_request(&request, &user_jwt, &api_key);
        let compressed = gzip(&body).map_err(|e| self.other(None, e, None))?;
        let frame = encode_frame(FLAG_COMPRESSED, &compressed);

        let base = custom_base.unwrap_or_else(|| DEVIN_API_URL.to_string());
        let url = format!("{base}{CHAT_MESSAGE_PATH}");
        debug!("Devin create_message_stream: POST {url}");

        let resp = self
            .http_client
            .post(&url)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("connect-content-encoding", "gzip")
            .header("accept-encoding", "identity")
            .header("user-agent", "connect-go/1.18.1 (go1.26.3)")
            .header("connect-accept-encoding", "gzip")
            .body(frame)
            .send()
            .await
            .map_err(|e| self.server_error(None, e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(self.other(
                Some(status),
                format!("Devin API error {status}"),
                Some(text),
            ));
        }

        let provider_id = self.id.clone();
        let model = request.model.to_string();
        let byte_stream = resp.bytes_stream();

        let stream = async_stream::stream! {
            let mut byte_stream = byte_stream;
            let mut decoder = ConnectDecoder::new();
            let mut emitted_start = false;
            let message_id = format!("devin-{}", uuid_v4());
            let text_index: usize = 0;
            let mut thinking_index: Option<usize> = None;
            let mut tool_block_index: usize = 1000;
            // toolCallId → (block_index, accumulated_args_json)
            let mut open_tools: HashMap<String, (usize, String)> = HashMap::new();
            let mut active_tool: Option<String> = None;
            let mut last_stop = StopReason::EndTurn;
            let mut final_usage = UsageInfo::default();
            let mut saw_tool = false;

            while let Some(chunk_result) = byte_stream.next().await {
                let chunk: Bytes = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(ProviderError::StreamError {
                            provider: provider_id.clone(),
                            message: e.to_string(),
                            partial_response: None,
                        });
                        return;
                    }
                };
                decoder.push(&chunk);

                loop {
                    let frame = match decoder.next_frame() {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(e) => {
                            yield Err(ProviderError::StreamError {
                                provider: provider_id.clone(),
                                message: e.to_string(),
                                partial_response: None,
                            });
                            return;
                        }
                    };

                    if frame.is_end_stream() {
                        if let Some(err) = trailer_error(&frame.payload) {
                            yield Err(ProviderError::StreamError {
                                provider: provider_id.clone(),
                                message: err,
                                partial_response: None,
                            });
                            return;
                        }
                        continue;
                    }

                    let delta = match proto::decode_chat_delta(&frame.payload) {
                        Ok(d) => d,
                        Err(e) => {
                            warn!("Devin: response decode error: {e}");
                            continue;
                        }
                    };

                    if !emitted_start {
                        emitted_start = true;
                        yield Ok(StreamEvent::MessageStart {
                            id: message_id.clone(),
                            model: model.clone(),
                            usage: UsageInfo::default(),
                        });
                    }

                    if !delta.delta_thinking.is_empty() {
                        let idx = *thinking_index.get_or_insert(2000);
                        yield Ok(StreamEvent::ThinkingDelta {
                            index: idx,
                            thinking: delta.delta_thinking.clone(),
                        });
                    }

                    if !delta.delta_text.is_empty() {
                        yield Ok(StreamEvent::TextDelta {
                            index: text_index,
                            text: delta.delta_text.clone(),
                        });
                    }

                    for tc in &delta.tool_calls {
                        saw_tool = true;
                        let tool_id = if tc.id.is_empty() {
                            active_tool.clone().unwrap_or_default()
                        } else {
                            tc.id.clone()
                        };
                        if tool_id.is_empty() {
                            continue;
                        }
                        let is_new = !open_tools.contains_key(&tool_id);
                        if is_new {
                            let idx = tool_block_index;
                            tool_block_index += 1;
                            open_tools.insert(tool_id.clone(), (idx, String::new()));
                            yield Ok(StreamEvent::ContentBlockStart {
                                index: idx,
                                content_block: ContentBlock::ToolUse {
                                    id: tool_id.clone(),
                                    name: tc.name.clone(),
                                    input: serde_json::json!({}),
                                    thought_signature: None,
                                },
                            });
                        }
                        active_tool = Some(tool_id.clone());
                        if tc.arguments_json.is_empty() {
                            continue;
                        }
                        let Some(entry) = open_tools.get_mut(&tool_id) else {
                            continue;
                        };
                        let accumulated = if tc.arguments_json.starts_with(&entry.1) {
                            tc.arguments_json.clone()
                        } else {
                            format!("{}{}", entry.1, tc.arguments_json)
                        };
                        let delta_str = accumulated[entry.1.len()..].to_string();
                        entry.1 = accumulated;
                        if !delta_str.is_empty() {
                            yield Ok(StreamEvent::InputJsonDelta {
                                index: entry.0,
                                partial_json: delta_str,
                            });
                        }
                    }

                    if delta.stop_reason == proto::STOP_MAX_TOKENS {
                        last_stop = StopReason::MaxTokens;
                    } else if delta.stop_reason == proto::STOP_FUNCTION_CALL {
                        last_stop = StopReason::ToolUse;
                    }

                    if let Some(u) = &delta.usage {
                        final_usage = UsageInfo {
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                            cache_creation_input_tokens: u.cache_write_tokens,
                            cache_read_input_tokens: u.cache_read_tokens,
                        };
                    }
                }
            }

            if !emitted_start {
                yield Ok(StreamEvent::MessageStart {
                    id: message_id.clone(),
                    model: model.clone(),
                    usage: UsageInfo::default(),
                });
            }

            if thinking_index.is_some() {
                yield Ok(StreamEvent::ContentBlockStop { index: 2000 });
            }
            yield Ok(StreamEvent::ContentBlockStop { index: text_index });
            let mut tool_indices: Vec<usize> = open_tools.values().map(|(idx, _)| *idx).collect();
            tool_indices.sort_unstable();
            for idx in tool_indices {
                yield Ok(StreamEvent::ContentBlockStop { index: idx });
            }

            let stop = if saw_tool { StopReason::ToolUse } else { last_stop };
            yield Ok(StreamEvent::MessageDelta {
                stop_reason: Some(stop),
                usage: Some(final_usage),
            });
            yield Ok(StreamEvent::MessageStop);
        };

        Ok(Box::pin(stream))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        match self.fetch_user_jwt().await {
            Ok(_) => Ok(ProviderStatus::Healthy),
            Err(ProviderError::AuthFailed { message, .. }) => Err(ProviderError::AuthFailed {
                provider: self.id.clone(),
                message,
            }),
            Err(e) => Ok(ProviderStatus::Unavailable {
                reason: e.to_string(),
            }),
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: true,
            image_input: true,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: true,
            structured_output: false,
            system_prompt_style: SystemPromptStyle::SystemMessage,
        }
    }
}

// ---------------------------------------------------------------------------
// Message transform
// ---------------------------------------------------------------------------

fn system_prompt_text(system: Option<&SystemPrompt>) -> String {
    match system {
        Some(SystemPrompt::Text(t)) => t.clone(),
        Some(SystemPrompt::Blocks(blocks)) => blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        None => String::new(),
    }
}

fn tool_result_text(content: &ToolResultContent) -> String {
    match content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Map mikmik conversation history onto Cascade prompts (USER / SYSTEM / TOOL).
fn messages_to_prompts(messages: &[Message], cascade_id: &str) -> Vec<Prompt> {
    let mut prompts = Vec::new();
    for (index, msg) in messages.iter().enumerate() {
        let blocks = match &msg.content {
            MessageContent::Blocks(b) => b.clone(),
            MessageContent::Text(t) => vec![ContentBlock::Text { text: t.clone() }],
        };
        match msg.role {
            Role::User => push_user_prompts(&mut prompts, &blocks, cascade_id, index),
            Role::Assistant => push_assistant_prompt(&mut prompts, &blocks, cascade_id, index),
        }
    }
    prompts
}

fn push_user_prompts(
    prompts: &mut Vec<Prompt>,
    blocks: &[ContentBlock],
    cascade_id: &str,
    index: usize,
) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::Image { source } => {
                if let (Some(data), Some(mime)) = (&source.data, &source.media_type) {
                    images.push(Image {
                        base64_data: data.clone(),
                        mime_type: mime.clone(),
                    });
                }
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                prompts.push(Prompt {
                    message_id: seed_id(cascade_id, index, "tool"),
                    source: proto::SOURCE_TOOL,
                    prompt: tool_result_text(content),
                    thinking: String::new(),
                    signature: String::new(),
                    signature_type: String::new(),
                    tool_calls: vec![],
                    tool_call_id: tool_use_id.clone(),
                    tool_result_is_error: is_error.unwrap_or(false),
                    images: vec![],
                });
            }
            _ => {}
        }
    }
    if !text.is_empty() || !images.is_empty() {
        prompts.push(Prompt {
            message_id: seed_id(cascade_id, index, "user"),
            source: proto::SOURCE_USER,
            prompt: text,
            thinking: String::new(),
            signature: String::new(),
            signature_type: String::new(),
            tool_calls: vec![],
            tool_call_id: String::new(),
            tool_result_is_error: false,
            images,
        });
    }
}

fn push_assistant_prompt(
    prompts: &mut Vec<Prompt>,
    blocks: &[ContentBlock],
    cascade_id: &str,
    index: usize,
) {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut signature = String::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::Thinking {
                thinking: th,
                signature: sig,
            } => {
                thinking.push_str(th);
                if signature.is_empty() {
                    signature = sig.clone();
                }
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments_json: input.to_string(),
                });
            }
            _ => {}
        }
    }
    if text.is_empty() && thinking.is_empty() && signature.is_empty() && tool_calls.is_empty() {
        return;
    }
    prompts.push(Prompt {
        message_id: format!("bot-{}", seed_id(cascade_id, index, "assistant")),
        source: proto::SOURCE_SYSTEM,
        prompt: text,
        thinking,
        signature,
        signature_type: String::new(),
        tool_calls,
        tool_call_id: String::new(),
        tool_result_is_error: false,
        images: vec![],
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn seed_id(cascade_id: &str, index: usize, role: &str) -> String {
    // A deterministic UUID-shaped id from a stable seed (no prompt text), so it
    // survives history edits, matching the reference client's threading.
    let seed = format!("{cascade_id}\0{index}\0{role}");
    let digest = <sha2::Sha256 as sha2::Digest>::digest(seed.as_bytes());
    let b = &digest[..16];
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn gzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .map_err(|e| format!("gzip write failed: {e}"))?;
    encoder
        .finish()
        .map_err(|e| format!("gzip finish failed: {e}"))
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| format!("gunzip failed: {e}"))?;
    Ok(out)
}

fn map_proto_err(e: String) -> String {
    e
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
        "Devin stream error{}: {message}",
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
    use mikmik_core::types::Message;

    #[test]
    fn messages_map_tool_result_to_a_tool_prompt() {
        let messages = vec![
            Message::user_blocks(vec![ContentBlock::Text {
                text: "do it".to_string(),
            }]),
            Message::assistant_blocks(vec![ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"path": "a"}),
                thought_signature: None,
            }]),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: ToolResultContent::Text("done".to_string()),
                is_error: Some(false),
            }]),
        ];
        let prompts = messages_to_prompts(&messages, "casc");
        assert_eq!(prompts.len(), 3);
        assert_eq!(prompts[0].source, proto::SOURCE_USER);
        assert_eq!(prompts[1].source, proto::SOURCE_SYSTEM);
        assert_eq!(prompts[1].tool_calls.len(), 1);
        assert_eq!(prompts[2].source, proto::SOURCE_TOOL);
        assert_eq!(prompts[2].tool_call_id, "call-1");
    }

    #[test]
    fn seed_id_is_deterministic_and_uuid_shaped() {
        let a = seed_id("casc", 1, "user");
        let b = seed_id("casc", 1, "user");
        assert_eq!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[8], b'-');
    }

    #[test]
    fn gzip_round_trips() {
        let data = b"the quick brown fox";
        let compressed = gzip(data).unwrap();
        assert_eq!(gunzip(&compressed).unwrap(), data);
    }

    #[test]
    fn trailer_error_reads_code_and_message() {
        let payload = br#"{"error":{"code":"invalid_argument","message":"bad"}}"#;
        assert_eq!(
            trailer_error(payload),
            Some("Devin stream error invalid_argument: bad".to_string())
        );
        assert_eq!(trailer_error(b"{}"), None);
    }
}
