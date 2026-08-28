//! providers/devin/proto.rs — hand-written protobuf codecs for the Cascade
//! chat wire.
//!
//! Field numbers and enum values are ported from oh-my-pi's `devin-proto.ts`
//! descriptors (`exa.api_server_pb` / `exa.chat_pb` / `exa.codeium_common_pb`).
//! Only the subset the chat turn needs is covered: the request builders
//! (`GetUserJwtRequest`, `GetChatMessageRequest` and their nested messages) and
//! the response decoders (`GetUserJwtResponse`, `GetChatMessageResponse`).
//!
//! Encoding follows protobuf defaults: a zero / empty / false scalar is omitted
//! unless it is semantically required, matching the reference client's wire.

use crate::protocol::protobuf::{ProtoError, ProtoReader, ProtoWriter, WIRE_LEN};

// ---- enum values ---------------------------------------------------------

/// `ChatMessageSource`.
pub const SOURCE_USER: u64 = 1;
pub const SOURCE_SYSTEM: u64 = 2;
pub const SOURCE_TOOL: u64 = 4;

/// `ChatMessageRequestType.CASCADE`.
pub const REQUEST_TYPE_CASCADE: u64 = 5;

/// `ConversationalPlannerMode.DEFAULT`.
pub const PLANNER_MODE_DEFAULT: u64 = 1;

/// `CacheControlType.EPHEMERAL`.
pub const CACHE_EPHEMERAL: u64 = 1;

/// `StopReason` values seen on responses.
pub const STOP_MAX_TOKENS: i32 = 3;
pub const STOP_FUNCTION_CALL: i32 = 10;

// ---- request-side inputs -------------------------------------------------

/// Identity metadata carried on every RPC (`exa.codeium_common_pb.Metadata`).
pub struct Metadata<'a> {
    pub api_key: &'a str,
    pub user_jwt: &'a str,
}

impl Metadata<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = ProtoWriter::new();
        w.field_string(1, "windsurf"); // ideName
        w.field_string(7, "3.2.23"); // ideVersion
        w.field_string(12, "windsurf"); // extensionName
        w.field_string(2, "1.48.2"); // extensionVersion
        if !self.api_key.is_empty() {
            w.field_string(3, self.api_key);
        }
        w.field_string(4, "en"); // locale
        if !self.user_jwt.is_empty() {
            w.field_string(21, self.user_jwt);
        }
        w.finish()
    }
}

/// A tool call echoed back into history, or emitted by the model.
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

impl ToolCall {
    fn encode(&self) -> Vec<u8> {
        let mut w = ProtoWriter::new();
        if !self.id.is_empty() {
            w.field_string(1, &self.id);
        }
        if !self.name.is_empty() {
            w.field_string(2, &self.name);
        }
        if !self.arguments_json.is_empty() {
            w.field_string(3, &self.arguments_json);
        }
        w.finish()
    }
}

/// An inline image (`exa.codeium_common_pb.ImageData`).
pub struct Image {
    pub base64_data: String,
    pub mime_type: String,
}

impl Image {
    fn encode(&self) -> Vec<u8> {
        let mut w = ProtoWriter::new();
        w.field_string(1, &self.base64_data);
        w.field_string(2, &self.mime_type);
        w.finish()
    }
}

/// One conversation-history entry (`exa.chat_pb.ChatMessagePrompt`).
pub struct Prompt {
    pub message_id: String,
    pub source: u64,
    pub prompt: String,
    pub thinking: String,
    pub signature: String,
    pub signature_type: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    pub tool_result_is_error: bool,
    pub images: Vec<Image>,
}

impl Prompt {
    fn encode(&self) -> Vec<u8> {
        let mut w = ProtoWriter::new();
        if !self.message_id.is_empty() {
            w.field_string(1, &self.message_id);
        }
        w.field_varint(2, self.source);
        if !self.prompt.is_empty() {
            w.field_string(3, &self.prompt);
        }
        for tc in &self.tool_calls {
            w.field_message(6, &tc.encode());
        }
        if !self.tool_call_id.is_empty() {
            w.field_string(7, &self.tool_call_id);
        }
        if self.tool_result_is_error {
            w.field_bool(9, true);
        }
        for img in &self.images {
            w.field_message(10, &img.encode());
        }
        if !self.thinking.is_empty() {
            w.field_string(11, &self.thinking);
        }
        if !self.signature.is_empty() {
            w.field_string(12, &self.signature);
        }
        if !self.signature_type.is_empty() {
            w.field_string(18, &self.signature_type);
        }
        w.finish()
    }
}

/// A tool definition (`exa.chat_pb.ChatToolDefinition`).
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub json_schema_string: String,
    pub strict: bool,
}

impl ToolDefinition {
    fn encode(&self) -> Vec<u8> {
        let mut w = ProtoWriter::new();
        w.field_string(1, &self.name);
        w.field_string(2, &self.description);
        w.field_string(3, &self.json_schema_string);
        if self.strict {
            w.field_bool(12, true);
        }
        w.finish()
    }
}

/// Sampling configuration (`exa.codeium_common_pb.CompletionConfiguration`).
pub struct Configuration {
    pub max_tokens: u64,
    pub temperature: f64,
    pub top_p: f64,
    pub stop_patterns: Vec<String>,
}

impl Configuration {
    fn encode(&self) -> Vec<u8> {
        let mut w = ProtoWriter::new();
        w.field_varint(1, 1); // numCompletions
        w.field_varint(2, self.max_tokens);
        w.field_varint(3, 200); // maxNewlines
        w.field_double(5, self.temperature);
        w.field_double(6, self.temperature); // firstTemperature
        w.field_varint(7, 50); // topK
        w.field_double(8, self.top_p);
        for pattern in &self.stop_patterns {
            w.field_string(9, pattern);
        }
        w.field_double(11, 1.0); // fimEotProbThreshold
        w.finish()
    }
}

/// A full `GetChatMessageRequest`.
pub struct ChatRequest<'a> {
    pub api_key: &'a str,
    pub user_jwt: &'a str,
    /// Flattened system prompt.
    pub prompt: String,
    pub prompts: Vec<Prompt>,
    pub chat_model_uid: String,
    pub cascade_id: String,
    pub execution_id: String,
    pub configuration: Configuration,
    pub tools: Vec<ToolDefinition>,
}

impl ChatRequest<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = ProtoWriter::new();
        let metadata = Metadata {
            api_key: self.api_key,
            user_jwt: self.user_jwt,
        };
        w.field_message(1, &metadata.encode());
        if !self.prompt.is_empty() {
            w.field_string(2, &self.prompt);
        }
        for p in &self.prompts {
            w.field_message(3, &p.encode());
        }
        w.field_varint(7, REQUEST_TYPE_CASCADE); // requestType
        w.field_message(8, &self.configuration.encode());
        for tool in &self.tools {
            w.field_message(10, &tool.encode());
        }
        w.field_bool(11, true); // disableParallelToolCalls
        w.field_message(12, &encode_tool_choice_auto()); // toolChoice
        w.field_message(13, &encode_cache_options_ephemeral()); // systemPromptCacheOptions
        if !self.cascade_id.is_empty() {
            w.field_string(16, &self.cascade_id);
        }
        w.field_varint(20, PLANNER_MODE_DEFAULT); // plannerMode
        if !self.chat_model_uid.is_empty() {
            w.field_string(21, &self.chat_model_uid);
        }
        if !self.execution_id.is_empty() {
            w.field_string(22, &self.execution_id);
        }
        w.finish()
    }
}

/// `ChatToolChoice { choice: optionName = "auto" }`.
fn encode_tool_choice_auto() -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_string(1, "auto"); // oneof optionName
    w.finish()
}

/// `PromptCacheOptions { type = EPHEMERAL }`.
fn encode_cache_options_ephemeral() -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_varint(1, CACHE_EPHEMERAL);
    w.finish()
}

/// Build a `GetUserJwtRequest` from the session token.
pub fn encode_user_jwt_request(api_key: &str) -> Vec<u8> {
    let metadata = Metadata {
        api_key,
        user_jwt: "",
    };
    let mut w = ProtoWriter::new();
    w.field_message(1, &metadata.encode());
    w.finish()
}

// ---- response-side decoders ----------------------------------------------

/// A decoded `GetUserJwtResponse`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UserJwtResponse {
    pub user_jwt: String,
    pub custom_api_server_url: String,
}

/// Decode a `GetUserJwtResponse`.
pub fn decode_user_jwt_response(bytes: &[u8]) -> Result<UserJwtResponse, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut out = UserJwtResponse::default();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match field {
            1 if wire == WIRE_LEN => out.user_jwt = r.string()?,
            2 if wire == WIRE_LEN => out.custom_api_server_url = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(out)
}

/// Token usage from a chat response (`ModelUsageStats` subset).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
}

fn decode_usage(bytes: &[u8]) -> Result<Usage, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut u = Usage::default();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match field {
            2 => u.input_tokens = r.varint()?,
            3 => u.output_tokens = r.varint()?,
            4 => u.cache_write_tokens = r.varint()?,
            5 => u.cache_read_tokens = r.varint()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(u)
}

/// A streamed tool-call delta.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolCallDelta {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

fn decode_tool_call(bytes: &[u8]) -> Result<ToolCallDelta, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut tc = ToolCallDelta::default();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match field {
            1 if wire == WIRE_LEN => tc.id = r.string()?,
            2 if wire == WIRE_LEN => tc.name = r.string()?,
            3 if wire == WIRE_LEN => tc.arguments_json = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(tc)
}

/// A decoded `GetChatMessageResponse` delta.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ChatDelta {
    pub message_id: String,
    pub delta_text: String,
    pub delta_thinking: String,
    pub delta_signature: String,
    pub stop_reason: i32,
    pub tool_calls: Vec<ToolCallDelta>,
    pub usage: Option<Usage>,
}

/// Decode one `GetChatMessageResponse` frame payload.
pub fn decode_chat_delta(bytes: &[u8]) -> Result<ChatDelta, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut d = ChatDelta::default();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match field {
            1 if wire == WIRE_LEN => d.message_id = r.string()?,
            3 if wire == WIRE_LEN => d.delta_text = r.string()?,
            5 => d.stop_reason = r.int32()?,
            6 if wire == WIRE_LEN => d.tool_calls.push(decode_tool_call(r.bytes()?)?),
            7 if wire == WIRE_LEN => d.usage = Some(decode_usage(r.bytes()?)?),
            9 if wire == WIRE_LEN => d.delta_thinking = r.string()?,
            10 if wire == WIRE_LEN => d.delta_signature = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_jwt_request_round_trips_the_api_key() {
        let bytes = encode_user_jwt_request("sess-1");
        // The request wraps a Metadata whose field 3 is the api key.
        let mut r = ProtoReader::new(&bytes);
        let (field, _) = r.tag().unwrap();
        assert_eq!(field, 1);
        let mut meta = r.message().unwrap();
        let mut found = None;
        while !meta.is_empty() {
            let (f, w) = meta.tag().unwrap();
            if f == 3 {
                found = Some(meta.string().unwrap());
            } else {
                meta.skip(w).unwrap();
            }
        }
        assert_eq!(found.as_deref(), Some("sess-1"));
    }

    #[test]
    fn decode_user_jwt_response_reads_both_fields() {
        let mut w = ProtoWriter::new();
        w.field_string(1, "jwt-abc");
        w.field_string(2, "https://custom.example");
        let decoded = decode_user_jwt_response(&w.finish()).unwrap();
        assert_eq!(decoded.user_jwt, "jwt-abc");
        assert_eq!(decoded.custom_api_server_url, "https://custom.example");
    }

    #[test]
    fn decode_chat_delta_reads_text_tool_and_usage() {
        // Build a response with a text delta, a tool call, and usage.
        let mut tc = ProtoWriter::new();
        tc.field_string(1, "call-1");
        tc.field_string(2, "read");
        tc.field_string(3, "{\"path\":\"a\"}");

        let mut usage = ProtoWriter::new();
        usage.field_varint(2, 100);
        usage.field_varint(3, 20);

        let mut w = ProtoWriter::new();
        w.field_string(1, "msg-1");
        w.field_string(3, "hello");
        w.field_int32(5, STOP_FUNCTION_CALL);
        w.field_message(6, tc.as_slice());
        w.field_message(7, usage.as_slice());

        let delta = decode_chat_delta(&w.finish()).unwrap();
        assert_eq!(delta.message_id, "msg-1");
        assert_eq!(delta.delta_text, "hello");
        assert_eq!(delta.stop_reason, STOP_FUNCTION_CALL);
        assert_eq!(delta.tool_calls.len(), 1);
        assert_eq!(delta.tool_calls[0].name, "read");
        assert_eq!(delta.tool_calls[0].arguments_json, "{\"path\":\"a\"}");
        assert_eq!(delta.usage.as_ref().unwrap().input_tokens, 100);
        assert_eq!(delta.usage.as_ref().unwrap().output_tokens, 20);
    }

    #[test]
    fn chat_request_encodes_metadata_and_prompts() {
        let request = ChatRequest {
            api_key: "sess",
            user_jwt: "jwt",
            prompt: "system".to_string(),
            prompts: vec![Prompt {
                message_id: "m1".to_string(),
                source: SOURCE_USER,
                prompt: "hi".to_string(),
                thinking: String::new(),
                signature: String::new(),
                signature_type: String::new(),
                tool_calls: vec![],
                tool_call_id: String::new(),
                tool_result_is_error: false,
                images: vec![],
            }],
            chat_model_uid: "model-x".to_string(),
            cascade_id: "casc-1".to_string(),
            execution_id: "exec-1".to_string(),
            configuration: Configuration {
                max_tokens: 64000,
                temperature: 0.4,
                top_p: 1.0,
                stop_patterns: vec!["<|user|>".to_string()],
            },
            tools: vec![ToolDefinition {
                name: "read".to_string(),
                description: "read a file".to_string(),
                json_schema_string: "{}".to_string(),
                strict: false,
            }],
        };
        let bytes = request.encode();
        // Re-read the top level: field 1 metadata, field 2 prompt, field 3 prompt,
        // field 7 request type = CASCADE, field 21 model uid.
        let mut r = ProtoReader::new(&bytes);
        let mut saw_cascade = false;
        let mut model_uid = None;
        while !r.is_empty() {
            let (field, wire) = r.tag().unwrap();
            match field {
                7 => {
                    assert_eq!(r.varint().unwrap(), REQUEST_TYPE_CASCADE);
                    saw_cascade = true;
                }
                21 => model_uid = Some(r.string().unwrap()),
                _ => r.skip(wire).unwrap(),
            }
        }
        assert!(saw_cascade);
        assert_eq!(model_uid.as_deref(), Some("model-x"));
    }
}
