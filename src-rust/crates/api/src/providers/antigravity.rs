// providers/antigravity.rs — Google Antigravity provider.
//
// Antigravity reaches Gemini (and the Claude / GPT-OSS families Google fronts)
// through the internal Cloud Code Assist plane at
// `daily-cloudcode-pa.googleapis.com`. The inference wire is the ordinary
// Gemini `generateContent` body wrapped in a Cloud Code envelope
// (`{project, model, request: {…}}`), streamed over SSE where each event nests
// the Gemini chunk under a `response` field. So the request body is built by
// `GoogleProvider::build_gemini_body` and the SSE chunks are decoded by the
// same candidates/parts logic once the envelope is unwrapped.
//
// Auth is an OAuth Bearer resolved from the stored account, refreshed on expiry
// (the refresh preserves the resolved Cloud Code project). The OAuth flow and
// project provisioning live in `mikmik_core::antigravity_oauth`.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use mikmik_core::antigravity_oauth::{
    self, is_expired, load_antigravity_tokens_for_account, save_antigravity_tokens,
    save_antigravity_tokens_for_account, AntigravityTokens, CLIENT_METADATA,
    CLOUD_CODE_ASSIST_ENDPOINT,
};
use mikmik_core::provider_id::ProviderId;
use mikmik_core::types::{ContentBlock, UsageInfo};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamEvent, SystemPromptStyle,
};
use crate::providers::google::GoogleProvider;

pub struct AntigravityProvider {
    id: ProviderId,
    tokens: Arc<Mutex<AntigravityTokens>>,
    account: Option<String>,
    http_client: reqwest::Client,
}

impl AntigravityProvider {
    fn new(tokens: AntigravityTokens) -> Self {
        Self {
            id: ProviderId::new(ProviderId::GOOGLE_ANTIGRAVITY),
            tokens: Arc::new(Mutex::new(tokens)),
            account: None,
            http_client: reqwest::Client::new(),
        }
    }

    /// Construct from the active (or only) stored Antigravity account.
    pub fn from_stored() -> Option<Self> {
        let tokens = antigravity_oauth::get_antigravity_tokens()?;
        if tokens.access_token.is_empty() {
            return None;
        }
        Some(Self::new(tokens))
    }

    /// Construct from one named account, bypassing the active pointer.
    pub fn from_account(account_id: &str) -> Option<Self> {
        let tokens = load_antigravity_tokens_for_account(account_id)?;
        if tokens.access_token.is_empty() {
            return None;
        }
        let mut provider = Self::new(tokens);
        provider.account = Some(account_id.to_string());
        Some(provider)
    }

    fn persist_tokens(&self, updated: &AntigravityTokens) {
        let result = match self.account.as_deref() {
            Some(account_id) => save_antigravity_tokens_for_account(updated, account_id),
            None => save_antigravity_tokens(updated),
        };
        if let Err(e) = result {
            warn!("could not persist refreshed Antigravity tokens: {e}");
        }
    }

    /// The current access token and resolved project, refreshing first if the
    /// access token is expired.
    async fn access(&self) -> Result<(String, Option<String>), ProviderError> {
        let (token, expired, refresh_token, project) = {
            let guard = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
            (
                guard.access_token.clone(),
                is_expired(&guard),
                guard.refresh_token.clone(),
                guard.project_id.clone(),
            )
        };

        if !expired {
            return Ok((token, project));
        }

        let Some(refresh) = refresh_token else {
            warn!("Antigravity access token is expired and no refresh token is available");
            return Ok((token, project));
        };

        debug!("Antigravity access token expired — refreshing");
        match antigravity_oauth::refresh(&refresh, project.as_deref()).await {
            Ok(fresh) => {
                let access = fresh.access_token.clone();
                let fresh_project = fresh.project_id.clone();
                self.persist_tokens(&fresh);
                *self.tokens.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
                Ok((access, fresh_project))
            }
            Err(e) => Err(ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Antigravity token refresh failed: {e}"),
                status: None,
                body: None,
            }),
        }
    }

    /// Wrap a Gemini request body in the Cloud Code Assist envelope.
    fn envelope(&self, request: &ProviderRequest, project: Option<&str>) -> Value {
        let inner = GoogleProvider::build_gemini_body(request);
        json!({
            "project": project.unwrap_or_default(),
            "model": request.model.to_string(),
            "request": inner,
            "userAgent": antigravity_oauth::user_agent(),
        })
    }

    fn stream_url() -> String {
        format!("{CLOUD_CODE_ASSIST_ENDPOINT}/v1internal:streamGenerateContent?alt=sse")
    }
}

#[async_trait]
impl LlmProvider for AntigravityProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "Google Antigravity"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        // Cloud Code Assist only exposes the streaming RPC; collect the stream
        // into a single response.
        let model = request.model.to_string();
        let mut stream = self.create_message_stream(request).await?;
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let mut tool_json: std::collections::HashMap<usize, (String, String, String)> =
            std::collections::HashMap::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = UsageInfo::default();

        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta { text, .. } => {
                    if let Some(ContentBlock::Text { text: existing }) = blocks
                        .iter_mut()
                        .rev()
                        .find(|b| matches!(b, ContentBlock::Text { .. }))
                    {
                        existing.push_str(&text);
                    } else {
                        blocks.push(ContentBlock::Text { text });
                    }
                }
                StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::ToolUse { id, name, .. },
                } => {
                    tool_json.insert(index, (id, name, String::new()));
                }
                StreamEvent::InputJsonDelta {
                    index,
                    partial_json,
                } => {
                    tool_json
                        .entry(index)
                        .or_insert_with(|| (String::new(), String::new(), String::new()))
                        .2
                        .push_str(&partial_json);
                }
                StreamEvent::MessageDelta {
                    stop_reason: Some(sr),
                    usage: Some(u),
                } => {
                    stop_reason = sr;
                    usage = u;
                }
                StreamEvent::MessageDelta {
                    stop_reason: Some(sr),
                    ..
                } => stop_reason = sr,
                _ => {}
            }
        }

        let mut indices: Vec<usize> = tool_json.keys().copied().collect();
        indices.sort_unstable();
        for index in indices {
            if let Some((id, name, args)) = tool_json.remove(&index) {
                let input = serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
                blocks.push(ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature: None,
                });
            }
        }

        Ok(ProviderResponse {
            id: format!("antigravity-{}", uuid_hex()),
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
        let (token, project) = self.access().await?;
        let model = request.model.to_string();
        let body = self.envelope(&request, project.as_deref());
        let url = Self::stream_url();

        debug!("Antigravity create_message_stream: POST {url}");

        let resp = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", antigravity_oauth::user_agent())
            .header("Client-Metadata", CLIENT_METADATA)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::ServerError {
                provider: self.id.clone(),
                status: None,
                message: e.to_string(),
                is_retryable: true,
            })?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let resp_body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Antigravity request failed: {status}"),
                status: Some(status),
                body: Some(resp_body),
            });
        }

        let provider_id = self.id.clone();
        let byte_stream = resp.bytes_stream();

        let stream = async_stream::stream! {
            let mut byte_stream = byte_stream;
            let text_block_index: usize = 0;
            let mut tool_block_index: usize = 1000;
            let mut open_tool_calls: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();
            let mut emitted_message_start = false;
            let message_id = format!("antigravity-{}", uuid_hex());
            let mut decoder = crate::SseByteDecoder::new();
            let mut tool_name_counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();

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

                for line in decoder.push(&chunk) {
                    let line = line.trim_end_matches('\r');
                    let Some(data) = line.strip_prefix("data: ") else { continue };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }

                    let envelope: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Antigravity SSE: JSON parse error: {e}: {data}");
                            continue;
                        }
                    };

                    // In-band stream failure delivered as a final JSON event.
                    if let Some(err) = envelope.get("error") {
                        let msg = err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown error")
                            .to_string();
                        yield Err(ProviderError::StreamError {
                            provider: provider_id.clone(),
                            message: msg,
                            partial_response: None,
                        });
                        return;
                    }

                    // Every Gemini chunk is nested under `response`.
                    let Some(parsed) = envelope.get("response") else { continue };

                    if !emitted_message_start {
                        emitted_message_start = true;
                        yield Ok(StreamEvent::MessageStart {
                            id: message_id.clone(),
                            model: model.clone(),
                            usage: gemini_usage(parsed),
                        });
                    }

                    let Some(candidates) = parsed.get("candidates").and_then(|c| c.as_array()) else {
                        continue;
                    };

                    for candidate in candidates {
                        if let Some(parts) =
                            candidate.get("content").and_then(|c| c.get("parts")).and_then(|p| p.as_array())
                        {
                            for (part_idx, part) in parts.iter().enumerate() {
                                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                    yield Ok(StreamEvent::TextDelta {
                                        index: text_block_index,
                                        text: text.to_string(),
                                    });
                                } else if let Some(fc) = part.get("functionCall") {
                                    let name = fc
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let args_str = fc
                                        .get("args")
                                        .map(|a| a.to_string())
                                        .unwrap_or_else(|| "{}".to_string());
                                    let sig = part
                                        .get("thoughtSignature")
                                        .and_then(|s| s.as_str())
                                        .map(str::to_string);

                                    let idx = if let Some(existing) = open_tool_calls.get(&part_idx) {
                                        *existing
                                    } else {
                                        let occurrence = tool_name_counts
                                            .entry(name.clone())
                                            .and_modify(|c| *c += 1)
                                            .or_insert(0);
                                        let id = GoogleProvider::tool_use_id_for_name(&name, *occurrence);
                                        let idx = tool_block_index;
                                        tool_block_index += 1;
                                        open_tool_calls.insert(part_idx, idx);
                                        yield Ok(StreamEvent::ContentBlockStart {
                                            index: idx,
                                            content_block: ContentBlock::ToolUse {
                                                id,
                                                name: name.clone(),
                                                input: json!({}),
                                                thought_signature: sig,
                                            },
                                        });
                                        idx
                                    };

                                    yield Ok(StreamEvent::InputJsonDelta {
                                        index: idx,
                                        partial_json: args_str,
                                    });
                                }
                            }
                        }

                        let finish_reason = candidate
                            .get("finishReason")
                            .and_then(|r| r.as_str())
                            .unwrap_or("");
                        if finish_reason.is_empty() || finish_reason == "FINISH_REASON_UNSPECIFIED" {
                            continue;
                        }

                        yield Ok(StreamEvent::ContentBlockStop { index: text_block_index });
                        let mut tool_indices: Vec<usize> = open_tool_calls.values().copied().collect();
                        tool_indices.sort_unstable();
                        for idx in tool_indices {
                            yield Ok(StreamEvent::ContentBlockStop { index: idx });
                        }
                        open_tool_calls.clear();

                        let stop_reason = match finish_reason {
                            "STOP" => Some(StopReason::EndTurn),
                            "MAX_TOKENS" => Some(StopReason::MaxTokens),
                            "SAFETY" | "RECITATION" => Some(StopReason::ContentFiltered),
                            "TOOL_CODE" | "FUNCTION_CALL" => Some(StopReason::ToolUse),
                            other => Some(StopReason::Other(other.to_string())),
                        };
                        yield Ok(StreamEvent::MessageDelta {
                            stop_reason,
                            usage: Some(gemini_usage(parsed)),
                        });
                        yield Ok(StreamEvent::MessageStop);
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        let (token, _) = self.access().await?;
        if token.is_empty() {
            return Ok(ProviderStatus::Unavailable {
                reason: "no Antigravity access token".to_string(),
            });
        }
        Ok(ProviderStatus::Healthy)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: true,
            image_input: true,
            pdf_input: true,
            audio_input: false,
            video_input: true,
            caching: false,
            structured_output: true,
            system_prompt_style: SystemPromptStyle::SystemInstruction,
        }
    }
}

/// Read Gemini usage counters from a `response`-level chunk.
fn gemini_usage(response: &Value) -> UsageInfo {
    let meta = response.get("usageMetadata");
    UsageInfo {
        input_tokens: meta
            .and_then(|m| m.get("promptTokenCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: meta
            .and_then(|m| m.get("candidatesTokenCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: meta
            .and_then(|m| m.get("cachedContentTokenCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    }
}

fn uuid_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let a = t ^ (t >> 17) ^ (t << 13);
    let b = a.wrapping_mul(0x517cc1b727220a95);
    format!("{:032x}", b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::config::WireModel;
    use mikmik_core::types::Message;

    fn request(model: &'static str) -> ProviderRequest {
        ProviderRequest {
            model: WireModel::literal(model),
            messages: vec![Message::user_blocks(vec![ContentBlock::Text {
                text: "hi".to_string(),
            }])],
            system_prompt: None,
            tools: vec![],
            max_tokens: 256,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            provider_options: json!({}),
        }
    }

    fn provider() -> AntigravityProvider {
        AntigravityProvider::new(AntigravityTokens {
            access_token: "t".to_string(),
            project_id: Some("proj-1".to_string()),
            ..Default::default()
        })
    }

    #[test]
    fn envelope_wraps_gemini_body_with_project_and_model() {
        let env = provider().envelope(&request("gemini-3-flash"), Some("proj-1"));
        assert_eq!(env["project"], json!("proj-1"));
        assert_eq!(env["model"], json!("gemini-3-flash"));
        // The inner request is the Gemini body: contents + generationConfig.
        assert!(env["request"]["contents"].is_array());
        assert!(env["request"]["generationConfig"]["maxOutputTokens"].is_number());
        assert!(env["userAgent"]
            .as_str()
            .unwrap()
            .starts_with("antigravity/hub/"));
    }

    #[test]
    fn envelope_defaults_project_to_empty_when_unresolved() {
        let env = provider().envelope(&request("gemini-3-flash"), None);
        assert_eq!(env["project"], json!(""));
    }

    #[test]
    fn gemini_usage_reads_nested_counters() {
        let usage = gemini_usage(&json!({
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "cachedContentTokenCount": 3
            }
        }));
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cache_read_input_tokens, 3);
    }
}
