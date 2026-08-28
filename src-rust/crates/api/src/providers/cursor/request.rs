//! providers/cursor/request.rs — assemble a Cursor `AgentRunRequest`.
//!
//! Cursor addresses conversation history by content hash: the run request
//! carries only blob ids, and the server pulls the bytes back over the KV
//! channel. This module walks mikmik's message history into the blob payloads,
//! stores them, and produces the run-request bytes plus the encoded rules and
//! tool definitions the exec `requestContext` handshake answers with.
//!
//! `rootPromptMessagesJson` is the field Cursor's server builds the model prompt
//! from, so history goes there in full, JSON-shaped the way Cursor expects. The
//! display-only `turns[]` metadata is left empty — the server does not read it
//! to construct the prompt.

use std::collections::HashMap;

use mikmik_core::types::{ContentBlock, Message, MessageContent, Role, ToolResultContent};
use serde_json::json;

use super::proto::{
    self, ActionData, ConversationStateData, McpToolDefData, ModelData, RuleData, RunRequestData,
};
use crate::provider_types::{ProviderRequest, SystemPrompt};

/// A content-addressed store of history blobs, keyed by their SHA-256 id.
#[derive(Debug, Default)]
pub struct BlobStore {
    map: HashMap<[u8; 32], Vec<u8>>,
}

impl BlobStore {
    /// Store `data` and return its blob id.
    pub fn store(&mut self, data: Vec<u8>) -> [u8; 32] {
        let id = proto::blob_id(&data);
        self.map.insert(id, data);
        id
    }

    /// The bytes for a blob id, if held.
    pub fn get(&self, id: &[u8]) -> Option<&[u8]> {
        if id.len() != 32 {
            return None;
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(id);
        self.map.get(&key).map(Vec::as_slice)
    }
}

/// Everything one run request needs, once assembled.
pub struct BuiltRequest {
    pub run_request: Vec<u8>,
    pub blobs: BlobStore,
    /// Encoded `CursorRule` messages for the `requestContext` handshake.
    pub rules: Vec<Vec<u8>>,
    /// Encoded `McpToolDefinition` messages for the `requestContext` handshake.
    pub tools: Vec<Vec<u8>>,
}

/// Tools Cursor already drives natively, so they are not re-advertised as MCP.
const NATIVE_TOOL_NAMES: &[&str] = &["bash", "read", "write", "delete", "ls", "grep", "todo"];

/// Assemble the run request, its blob store, and the handshake rules and tools.
pub fn build_request(request: &ProviderRequest, conversation_id: &str) -> BuiltRequest {
    let system_prompts = normalize_system_prompts(request.system_prompt.as_ref());
    let mut blobs = BlobStore::default();

    let system_ids = store_system_prompt_blobs(&system_prompts, &mut blobs);
    let last_user = last_user_index(&request.messages);
    let root_ids = build_root_prompt_ids(&request.messages, &system_ids, last_user, &mut blobs);

    let state = proto::encode_conversation_state(&ConversationStateData {
        root_prompt_message_ids: root_ids,
        turn_ids: Vec::new(),
    });

    let action = build_action(&request.messages, last_user);
    let request_context = proto::encode_request_context(&[], &[]);
    let action_bytes = proto::encode_action(&action, &request_context);

    let model_id = request.model.to_string();
    let run_request = proto::encode_run_request_message(&RunRequestData {
        conversation_state: state,
        action: action_bytes,
        model: ModelData {
            model_id: model_id.clone(),
            display_model_id: model_id.clone(),
            display_name: model_id,
            max_mode: false,
            parameters: Vec::new(),
        },
        conversation_id: conversation_id.to_string(),
        custom_system_prompt: None,
    });

    BuiltRequest {
        run_request,
        blobs,
        rules: build_rules(&system_prompts),
        tools: build_tool_defs(&request.tools),
    }
}

/// Flatten the system prompt into ordered entries, dropping empty ones.
fn normalize_system_prompts(system: Option<&SystemPrompt>) -> Vec<String> {
    let raw = match system {
        Some(SystemPrompt::Text(t)) => vec![t.clone()],
        Some(SystemPrompt::Blocks(blocks)) => blocks.iter().map(|b| b.text.clone()).collect(),
        None => Vec::new(),
    };
    raw.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// One global `CursorRule` per system prompt so always-apply rules survive the
/// server's prompt reconstruction from `requestContext.rules`.
fn build_rules(system_prompts: &[String]) -> Vec<Vec<u8>> {
    system_prompts
        .iter()
        .enumerate()
        .map(|(index, content)| {
            proto::encode_rule(&RuleData {
                full_path: format!("/mikmik/system-prompt/{index}.mdc"),
                content: content.clone(),
            })
        })
        .collect()
}

/// Advertise every non-native tool as an MCP tool under the `pi-agent` provider.
fn build_tool_defs(tools: &[mikmik_core::ToolDefinition]) -> Vec<Vec<u8>> {
    tools
        .iter()
        .filter(|tool| !NATIVE_TOOL_NAMES.contains(&tool.name.as_str()))
        .map(|tool| {
            proto::encode_mcp_tool_def(&McpToolDefData {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.to_string().into_bytes(),
            })
        })
        .collect()
}

/// One system-message JSON blob per prompt, or a default greeting when none.
fn store_system_prompt_blobs(system_prompts: &[String], blobs: &mut BlobStore) -> Vec<[u8; 32]> {
    if system_prompts.is_empty() {
        let json = json!({ "role": "system", "content": "You are a helpful assistant." });
        return vec![blobs.store(json.to_string().into_bytes())];
    }
    system_prompts
        .iter()
        .map(|content| {
            let json = json!({ "role": "system", "content": content });
            blobs.store(json.to_string().into_bytes())
        })
        .collect()
}

/// The index of the last user message, which becomes the action rather than
/// history. `None` when the conversation ends on an assistant message.
fn last_user_index(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(|m| matches!(m.role, Role::User))
}

/// Build `rootPromptMessagesJson`: the system blobs followed by one JSON blob
/// per prior message, in Cursor's Vercel-AI-SDK message shape.
fn build_root_prompt_ids(
    messages: &[Message],
    system_ids: &[[u8; 32]],
    last_user: Option<usize>,
    blobs: &mut BlobStore,
) -> Vec<[u8; 32]> {
    let mut ids: Vec<[u8; 32]> = system_ids.to_vec();
    let end = last_user.unwrap_or(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if index >= end {
            break;
        }
        push_message_json(message, blobs, &mut ids);
    }
    ids
}

fn push_message_json(message: &Message, blobs: &mut BlobStore, ids: &mut Vec<[u8; 32]>) {
    let blocks = message_blocks(message);
    match message.role {
        Role::User => push_user_json(&blocks, blobs, ids),
        Role::Assistant => push_assistant_json(&blocks, blobs, ids),
    }
}

fn message_blocks(message: &Message) -> Vec<ContentBlock> {
    match &message.content {
        MessageContent::Blocks(b) => b.clone(),
        MessageContent::Text(t) => vec![ContentBlock::Text { text: t.clone() }],
    }
}

fn push_user_json(blocks: &[ContentBlock], blobs: &mut BlobStore, ids: &mut Vec<[u8; 32]>) {
    let mut text = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => push_tool_result_json(tool_use_id, content, *is_error, blobs, ids),
            _ => {}
        }
    }
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        let json = json!({
            "role": "user",
            "content": [{ "type": "text", "text": trimmed }],
        });
        ids.push(blobs.store(json.to_string().into_bytes()));
    }
}

fn push_tool_result_json(
    tool_use_id: &str,
    content: &ToolResultContent,
    is_error: Option<bool>,
    blobs: &mut BlobStore,
    ids: &mut Vec<[u8; 32]>,
) {
    let mut result = json!({
        "type": "tool-result",
        "toolCallId": tool_use_id,
        "result": tool_result_text(content),
    });
    if is_error == Some(true) {
        result["isError"] = json!(true);
    }
    let json = json!({
        "role": "tool",
        "id": tool_use_id,
        "content": [result],
    });
    ids.push(blobs.store(json.to_string().into_bytes()));
}

fn push_assistant_json(blocks: &[ContentBlock], blobs: &mut BlobStore, ids: &mut Vec<[u8; 32]>) {
    let mut content: Vec<serde_json::Value> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                content.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                content.push(json!({
                    "type": "tool-call",
                    "toolCallId": id,
                    "toolName": name,
                    "args": input,
                }));
            }
            _ => {}
        }
    }
    if !content.is_empty() {
        let json = json!({ "role": "assistant", "content": content });
        ids.push(blobs.store(json.to_string().into_bytes()));
    }
}

/// The turn action: the last user message's text, or a resume when none.
fn build_action(messages: &[Message], last_user: Option<usize>) -> ActionData {
    let Some(index) = last_user else {
        return ActionData::Resume;
    };
    let text = messages
        .get(index)
        .map(|m| user_text(&message_blocks(m)))
        .unwrap_or_default();
    if text.trim().is_empty() {
        return ActionData::Resume;
    }
    ActionData::UserMessage {
        text,
        message_id: uuid::Uuid::new_v4().to_string(),
    }
}

fn user_text(blocks: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in blocks {
        if let ContentBlock::Text { text: t } = block {
            text.push_str(t);
        }
    }
    text.trim().to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_types::ProviderRequest;
    use mikmik_core::config::WireModel;

    fn request_with(messages: Vec<Message>, system: Option<&str>) -> ProviderRequest {
        ProviderRequest {
            model: WireModel::literal("gpt-5"),
            messages,
            system_prompt: system.map(|s| SystemPrompt::Text(s.to_string())),
            tools: Vec::new(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            provider_options: serde_json::Value::Null,
        }
    }

    #[test]
    fn blob_store_round_trips_by_id() {
        let mut store = BlobStore::default();
        let id = store.store(b"hello".to_vec());
        assert_eq!(store.get(&id), Some(b"hello".as_slice()));
        assert_eq!(store.get(&[0u8; 32]), None);
    }

    #[test]
    fn build_request_stores_system_and_user_blobs() {
        let messages = vec![Message::user_blocks(vec![ContentBlock::Text {
            text: "hi there".to_string(),
        }])];
        let built = build_request(&request_with(messages, Some("be terse")), "conv-1");
        // The system prompt blob and its rule are both present.
        assert_eq!(built.rules.len(), 1);
        // The run request is non-empty and wraps a run_request (field 1).
        assert!(!built.run_request.is_empty());
    }

    #[test]
    fn prior_messages_become_root_prompt_history() {
        let messages = vec![
            Message::user_blocks(vec![ContentBlock::Text {
                text: "first".to_string(),
            }]),
            Message::assistant_blocks(vec![ContentBlock::Text {
                text: "answer".to_string(),
            }]),
            Message::user_blocks(vec![ContentBlock::Text {
                text: "second".to_string(),
            }]),
        ];
        let last = last_user_index(&messages);
        assert_eq!(last, Some(2));
        let mut blobs = BlobStore::default();
        let ids = build_root_prompt_ids(&messages, &[], last, &mut blobs);
        // The first user message and the assistant reply are history; the last
        // user message is the action and is excluded.
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn action_is_the_last_user_message() {
        let messages = vec![Message::user_blocks(vec![ContentBlock::Text {
            text: "do it".to_string(),
        }])];
        match build_action(&messages, Some(0)) {
            ActionData::UserMessage { text, .. } => assert_eq!(text, "do it"),
            ActionData::Resume => panic!("expected a user message action"),
        }
    }
}
