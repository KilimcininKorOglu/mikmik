//! providers/cursor/proto.rs — hand-written codec for the Cursor agent wire.
//!
//! Cursor's `agent.v1.AgentService/Run` is a single bidirectional stream of
//! Connect-framed protobuf messages. This module ports, by hand, the subset of
//! `agent.proto` that a chat/exec turn needs: the client-side request graph
//! (`AgentRunRequest` and the blob-addressed `ConversationStateStructure`), the
//! client-side answers (exec results, interaction responses, KV blob replies,
//! heartbeats), and the server-side messages the dispatcher reads back
//! (`InteractionUpdate` deltas, `ExecServerMessage` tool args,
//! `InteractionQuery`, KV blob requests, conversation checkpoints).
//!
//! There is no code generation: each message is encoded and decoded against the
//! primitives in `crate::protocol::protobuf`. Field numbers are transcribed from
//! `agent.proto` and must stay exact — a wrong number is a rejected request.
//! proto3 wire rules apply: default-valued plain scalars are omitted, `optional`
//! fields are emitted when present, repeated fields emit one entry each, and
//! `map<string,bytes>` entries are `{1:key, 2:value}` sub-messages.
//!
//! This module is a pure codec. Blob addressing and the conversation-history
//! walk live in the parent module, which calls these encoders to build blob
//! payloads and drives the decoded server messages through the state machine.

use crate::protocol::protobuf::{ProtoError, ProtoReader, ProtoWriter, WIRE_LEN};

// ---------------------------------------------------------------------------
// Blob addressing
// ---------------------------------------------------------------------------

/// The content-addressed id of a blob: the SHA-256 of its bytes.
///
/// Cursor's `ConversationStateStructure` carries history as blob ids; the
/// server pulls the bytes back over the KV channel keyed by this hash.
pub fn blob_id(data: &[u8]) -> [u8; 32] {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

// ---------------------------------------------------------------------------
// Encode helpers (proto3 default-skipping)
// ---------------------------------------------------------------------------

fn put_str(w: &mut ProtoWriter, field: u32, value: &str) {
    if !value.is_empty() {
        w.field_string(field, value);
    }
}

fn put_bytes(w: &mut ProtoWriter, field: u32, value: &[u8]) {
    if !value.is_empty() {
        w.field_bytes(field, value);
    }
}

fn put_bool(w: &mut ProtoWriter, field: u32, value: bool) {
    if value {
        w.field_bool(field, value);
    }
}

fn put_i32(w: &mut ProtoWriter, field: u32, value: i32) {
    if value != 0 {
        w.field_int32(field, value);
    }
}

/// Encode one `map<string,bytes>` entry as a `{1:key, 2:value}` sub-message.
fn map_string_bytes_entry(key: &str, value: &[u8]) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, key);
    put_bytes(&mut w, 2, value);
    w.finish()
}

// ---------------------------------------------------------------------------
// Encode: request context (rules + forwarded tools)
// ---------------------------------------------------------------------------

/// A global, always-apply Cursor rule carrying one system-prompt entry.
pub struct RuleData {
    pub full_path: String,
    pub content: String,
}

const CURSOR_RULE_SOURCE_USER: i32 = 2;

/// `CursorRuleType{ global: CursorRuleTypeGlobal{} }` — an empty inner message.
fn encode_cursor_rule_type_global() -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_message(1, &[]);
    w.finish()
}

/// Encode a `CursorRule`.
pub fn encode_rule(rule: &RuleData) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, &rule.full_path);
    put_str(&mut w, 2, &rule.content);
    w.field_message(3, &encode_cursor_rule_type_global());
    put_i32(&mut w, 4, CURSOR_RULE_SOURCE_USER);
    w.finish()
}

/// A tool forwarded to Cursor as an MCP tool under the synthetic `pi-agent`
/// provider.
pub struct McpToolDefData {
    pub name: String,
    pub description: String,
    pub input_schema: Vec<u8>,
}

/// The synthetic provider identifier every forwarded tool is published under.
pub const PI_AGENT_PROVIDER: &str = "pi-agent";

/// Encode an `McpToolDefinition`.
pub fn encode_mcp_tool_def(tool: &McpToolDefData) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, &tool.name);
    put_str(&mut w, 4, PI_AGENT_PROVIDER);
    put_str(&mut w, 5, &tool.name);
    put_str(&mut w, 2, &tool.description);
    put_bytes(&mut w, 3, &tool.input_schema);
    w.finish()
}

/// Encode a `RequestContext` from pre-encoded rule and tool sub-messages.
pub fn encode_request_context(rules: &[Vec<u8>], tools: &[Vec<u8>]) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    for rule in rules {
        w.field_message(2, rule);
    }
    for tool in tools {
        w.field_message(7, tool);
    }
    w.finish()
}

// ---------------------------------------------------------------------------
// Encode: conversation history graph (blob contents)
// ---------------------------------------------------------------------------

/// Encode a `UserMessage{ text, message_id }`.
pub fn encode_user_message(text: &str, message_id: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, text);
    put_str(&mut w, 2, message_id);
    w.finish()
}

/// `ConversationStep{ assistant_message: AssistantMessage{ text } }`.
pub fn encode_step_assistant(text: &str) -> Vec<u8> {
    let mut inner = ProtoWriter::new();
    put_str(&mut inner, 1, text);
    let mut w = ProtoWriter::new();
    w.field_message(1, &inner.finish());
    w.finish()
}

/// `ConversationStep{ thinking_message: ThinkingMessage{ text } }`.
pub fn encode_step_thinking(text: &str) -> Vec<u8> {
    let mut inner = ProtoWriter::new();
    put_str(&mut inner, 1, text);
    let mut w = ProtoWriter::new();
    w.field_message(3, &inner.finish());
    w.finish()
}

/// An assistant tool call with its optional paired result, for history replay.
pub struct HistoryToolCall {
    pub tool_name: String,
    pub tool_call_id: String,
    /// Argument name → raw JSON-encoded value bytes.
    pub args: Vec<(String, Vec<u8>)>,
    pub result: Option<HistoryToolResult>,
}

/// The text outcome of a prior tool call.
pub struct HistoryToolResult {
    pub text: String,
    pub is_error: bool,
}

fn encode_mcp_args(call: &HistoryToolCall) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, &call.tool_name);
    for (key, value) in &call.args {
        w.field_message(2, &map_string_bytes_entry(key, value));
    }
    put_str(&mut w, 3, &call.tool_call_id);
    put_str(&mut w, 4, PI_AGENT_PROVIDER);
    put_str(&mut w, 5, &call.tool_name);
    w.finish()
}

fn encode_mcp_text_content(text: &str) -> Vec<u8> {
    let mut inner = ProtoWriter::new();
    put_str(&mut inner, 1, text);
    let mut w = ProtoWriter::new();
    w.field_message(1, &inner.finish());
    w.finish()
}

fn encode_mcp_tool_result(result: &HistoryToolResult) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if result.is_error {
        let mut err = ProtoWriter::new();
        put_str(&mut err, 1, &result.text);
        w.field_message(2, &err.finish());
    } else {
        let mut success = ProtoWriter::new();
        success.field_message(1, &encode_mcp_text_content(&result.text));
        w.field_message(1, &success.finish());
    }
    w.finish()
}

/// `ConversationStep{ tool_call: ToolCall{ mcp_tool_call, tool_call_id } }`.
pub fn encode_step_tool_call(call: &HistoryToolCall) -> Vec<u8> {
    let mut mcp_call = ProtoWriter::new();
    mcp_call.field_message(1, &encode_mcp_args(call));
    if let Some(result) = &call.result {
        mcp_call.field_message(2, &encode_mcp_tool_result(result));
    }
    let mut tool_call = ProtoWriter::new();
    tool_call.field_message(15, &mcp_call.finish());
    put_str(&mut tool_call, 57, &call.tool_call_id);
    let mut w = ProtoWriter::new();
    w.field_message(2, &tool_call.finish());
    w.finish()
}

/// `ConversationTurnStructure{ agent_conversation_turn{ user_message, steps } }`.
pub fn encode_agent_turn(user_message_id: &[u8], step_ids: &[[u8; 32]]) -> Vec<u8> {
    let mut turn = ProtoWriter::new();
    put_bytes(&mut turn, 1, user_message_id);
    for step in step_ids {
        turn.field_bytes(2, step);
    }
    let mut w = ProtoWriter::new();
    w.field_message(1, &turn.finish());
    w.finish()
}

// ---------------------------------------------------------------------------
// Encode: run request
// ---------------------------------------------------------------------------

/// The blob-addressed conversation state for one request.
pub struct ConversationStateData {
    pub root_prompt_message_ids: Vec<[u8; 32]>,
    pub turn_ids: Vec<[u8; 32]>,
}

/// Encode a `ConversationStateStructure`. Only the history-bearing fields are
/// populated; every other field defaults empty, which the server accepts.
pub fn encode_conversation_state(state: &ConversationStateData) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    for id in &state.root_prompt_message_ids {
        w.field_bytes(1, id);
    }
    for id in &state.turn_ids {
        w.field_bytes(8, id);
    }
    w.finish()
}

/// The turn's action: a new user message, or a resume that carries no message.
pub enum ActionData {
    UserMessage { text: String, message_id: String },
    Resume,
}

/// Encode a `ConversationAction` with its `RequestContext` handshake attached.
pub fn encode_action(action: &ActionData, request_context: &[u8]) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    match action {
        ActionData::UserMessage { text, message_id } => {
            let mut inner = ProtoWriter::new();
            inner.field_message(1, &encode_user_message(text, message_id));
            inner.field_message(2, request_context);
            w.field_message(1, &inner.finish());
        }
        ActionData::Resume => {
            let mut inner = ProtoWriter::new();
            inner.field_message(2, request_context);
            w.field_message(2, &inner.finish());
        }
    }
    w.finish()
}

/// Model identity for a run request.
pub struct ModelData {
    pub model_id: String,
    pub display_model_id: String,
    pub display_name: String,
    pub max_mode: bool,
    /// Extra `(id, value)` request parameters, e.g. `("reasoning", "high")`.
    pub parameters: Vec<(String, String)>,
}

fn encode_model_details(model: &ModelData) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, &model.model_id);
    put_str(&mut w, 3, &model.display_model_id);
    put_str(&mut w, 4, &model.display_name);
    put_bool(&mut w, 7, model.max_mode);
    w.finish()
}

fn encode_model_parameter(id: &str, value: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, id);
    put_str(&mut w, 2, value);
    w.finish()
}

fn encode_requested_model(model: &ModelData) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, &model.model_id);
    put_bool(&mut w, 2, model.max_mode);
    for (id, value) in &model.parameters {
        w.field_message(3, &encode_model_parameter(id, value));
    }
    w.finish()
}

/// Everything a run request carries once the sub-messages are assembled.
pub struct RunRequestData {
    pub conversation_state: Vec<u8>,
    pub action: Vec<u8>,
    pub model: ModelData,
    pub conversation_id: String,
    pub custom_system_prompt: Option<String>,
}

/// Encode an `AgentClientMessage{ run_request: AgentRunRequest{...} }`.
pub fn encode_run_request_message(req: &RunRequestData) -> Vec<u8> {
    let mut run = ProtoWriter::new();
    run.field_message(1, &req.conversation_state);
    run.field_message(2, &req.action);
    run.field_message(3, &encode_model_details(&req.model));
    run.field_message(9, &encode_requested_model(&req.model));
    put_str(&mut run, 5, &req.conversation_id);
    if let Some(prompt) = &req.custom_system_prompt {
        put_str(&mut run, 8, prompt);
    }
    let mut w = ProtoWriter::new();
    w.field_message(1, &run.finish());
    w.finish()
}

// ---------------------------------------------------------------------------
// Encode: client control, KV and heartbeat
// ---------------------------------------------------------------------------

/// `AgentClientMessage{ client_heartbeat: ClientHeartbeat{} }`.
pub fn encode_client_heartbeat_message() -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_message(7, &[]);
    w.finish()
}

fn wrap_kv_client_message(kv: Vec<u8>) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_message(3, &kv);
    w.finish()
}

/// `AgentClientMessage{ kv_client_message{ id, get_blob_result{ blob_data? } } }`.
pub fn encode_kv_get_blob_result_message(id: u32, blob_data: Option<&[u8]>) -> Vec<u8> {
    let mut result = ProtoWriter::new();
    if let Some(data) = blob_data {
        put_bytes(&mut result, 1, data);
    }
    let mut kv = ProtoWriter::new();
    kv.field_varint(1, u64::from(id));
    kv.field_message(2, &result.finish());
    wrap_kv_client_message(kv.finish())
}

/// `AgentClientMessage{ kv_client_message{ id, set_blob_result{} } }`.
pub fn encode_kv_set_blob_result_message(id: u32) -> Vec<u8> {
    let mut kv = ProtoWriter::new();
    kv.field_varint(1, u64::from(id));
    kv.field_message(3, &[]);
    wrap_kv_client_message(kv.finish())
}

/// `AgentClientMessage{ exec_client_message{ id, exec_id, <field>: result } }`.
pub fn encode_exec_result_message(
    id: u32,
    exec_id: &str,
    result_field: u32,
    result: &[u8],
) -> Vec<u8> {
    let mut exec = ProtoWriter::new();
    exec.field_varint(1, u64::from(id));
    put_str(&mut exec, 15, exec_id);
    exec.field_message(result_field, result);
    let mut w = ProtoWriter::new();
    w.field_message(2, &exec.finish());
    w.finish()
}

/// `AgentClientMessage{ exec_client_control_message{ throw{ id, error, error_code } } }`.
pub fn encode_exec_throw_message(id: u32, error: &str, error_code: &str) -> Vec<u8> {
    let mut throw = ProtoWriter::new();
    throw.field_varint(1, u64::from(id));
    put_str(&mut throw, 2, error);
    put_str(&mut throw, 4, error_code);
    let mut control = ProtoWriter::new();
    control.field_message(2, &throw.finish());
    let mut w = ProtoWriter::new();
    w.field_message(5, &control.finish());
    w.finish()
}

/// `AgentClientMessage{ exec_client_control_message{ stream_close{ id } } }`.
pub fn encode_exec_stream_close_message(id: u32) -> Vec<u8> {
    let mut close = ProtoWriter::new();
    close.field_varint(1, u64::from(id));
    let mut control = ProtoWriter::new();
    control.field_message(1, &close.finish());
    let mut w = ProtoWriter::new();
    w.field_message(5, &control.finish());
    w.finish()
}

// ---------------------------------------------------------------------------
// Encode: interaction responses
// ---------------------------------------------------------------------------

fn wrap_interaction_response(inner: Vec<u8>) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    w.field_message(6, &inner);
    w.finish()
}

/// A `*RequestResponse{ approved{} }` on the InteractionResponse `result` oneof.
///
/// The four hosted-search gates (web search, Exa search/fetch, web fetch) each
/// answer with the empty `approved` case (field 1) of their response message.
pub fn encode_interaction_approved_message(id: u32, response_field: u32) -> Vec<u8> {
    let mut response = ProtoWriter::new();
    response.field_message(1, &[]); // approved{}
    let mut inner = ProtoWriter::new();
    inner.field_varint(1, u64::from(id));
    inner.field_message(response_field, &response.finish());
    wrap_interaction_response(inner.finish())
}

/// `AskQuestionInteractionResponse{ result: AskQuestionResult{ rejected{ reason } } }`.
pub fn encode_ask_question_rejected_message(id: u32, reason: &str) -> Vec<u8> {
    let mut rejected = ProtoWriter::new();
    put_str(&mut rejected, 1, reason);
    let mut result = ProtoWriter::new();
    result.field_message(3, &rejected.finish());
    let mut response = ProtoWriter::new();
    response.field_message(1, &result.finish());
    let mut inner = ProtoWriter::new();
    inner.field_varint(1, u64::from(id));
    inner.field_message(3, &response.finish());
    wrap_interaction_response(inner.finish())
}

/// `SwitchModeRequestResponse{ rejected{ reason } }`.
pub fn encode_switch_mode_rejected_message(id: u32, reason: &str) -> Vec<u8> {
    let mut rejected = ProtoWriter::new();
    put_str(&mut rejected, 1, reason);
    let mut response = ProtoWriter::new();
    response.field_message(2, &rejected.finish());
    let mut inner = ProtoWriter::new();
    inner.field_varint(1, u64::from(id));
    inner.field_message(4, &response.finish());
    wrap_interaction_response(inner.finish())
}

/// `CreatePlanRequestResponse{ result: CreatePlanResult{ error{ error } } }`.
pub fn encode_create_plan_error_message(id: u32, error: &str) -> Vec<u8> {
    let mut plan_error = ProtoWriter::new();
    put_str(&mut plan_error, 1, error);
    let mut result = ProtoWriter::new();
    result.field_message(2, &plan_error.finish());
    let mut response = ProtoWriter::new();
    response.field_message(1, &result.finish());
    let mut inner = ProtoWriter::new();
    inner.field_varint(1, u64::from(id));
    inner.field_message(7, &response.finish());
    wrap_interaction_response(inner.finish())
}

/// An `approved{}` reply on an interaction query whose oneof this build does not
/// model: the matching response field with an empty `approved` (field 1) inside.
pub fn encode_unknown_approved_message(id: u32, response_field: u32) -> Vec<u8> {
    encode_interaction_approved_message(id, response_field)
}

// ---------------------------------------------------------------------------
// Encode: exec result bodies — request context, mcp state, hooks
// ---------------------------------------------------------------------------

/// `RequestContextResult{ success: RequestContextSuccess{ request_context } }`.
pub fn encode_request_context_result(request_context: &[u8]) -> Vec<u8> {
    let mut success = ProtoWriter::new();
    success.field_message(1, request_context);
    let mut w = ProtoWriter::new();
    w.field_message(1, &success.finish());
    w.finish()
}

/// `McpStateExecResult{ success: McpStateSuccess{ servers:[McpStateServer] } }`,
/// regrouping the already-advertised tools under the `pi-agent` provider.
pub fn encode_mcp_state_result(tools: &[Vec<u8>]) -> Vec<u8> {
    let mut server = ProtoWriter::new();
    put_str(&mut server, 1, PI_AGENT_PROVIDER);
    put_str(&mut server, 2, PI_AGENT_PROVIDER);
    for tool in tools {
        server.field_message(5, tool);
    }
    put_str(&mut server, 7, "connected");
    let mut success = ProtoWriter::new();
    if !tools.is_empty() {
        success.field_message(1, &server.finish());
    }
    let mut w = ProtoWriter::new();
    w.field_message(1, &success.finish());
    w.finish()
}

/// `ExecuteHookResult{ response: ExecuteHookResponse{ <case>{} } }` — the empty
/// response of the matching hook case, meaning "no hook had anything to say".
pub fn encode_hook_result(response_field: u32) -> Vec<u8> {
    let mut response = ProtoWriter::new();
    response.field_message(response_field, &[]);
    let mut w = ProtoWriter::new();
    w.field_message(1, &response.finish());
    w.finish()
}

// ---------------------------------------------------------------------------
// Encode: exec result bodies — pi tool family (output-string shaped)
// ---------------------------------------------------------------------------

fn pi_success_body(output: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, output);
    w.finish()
}

fn pi_error_body(error: &str) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_str(&mut w, 1, error);
    w.finish()
}

/// `<PiXExecResult>{ success{ output } }` or `{ error{ error } }`.
pub fn encode_pi_result(output: &str, is_error: bool) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if is_error {
        w.field_message(2, &pi_error_body(output));
    } else {
        w.field_message(1, &pi_success_body(output));
    }
    w.finish()
}

// ---------------------------------------------------------------------------
// Encode: exec result bodies — native tool family
// ---------------------------------------------------------------------------

/// `ReadResult{ success: ReadSuccess{ path, content } }` / `{ error{...} }`.
pub fn encode_read_result(path: &str, text: &str, is_error: bool) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if is_error {
        let mut err = ProtoWriter::new();
        put_str(&mut err, 1, path);
        put_str(&mut err, 2, text);
        w.field_message(2, &err.finish());
    } else {
        let mut success = ProtoWriter::new();
        put_str(&mut success, 1, path);
        put_str(&mut success, 2, text);
        w.field_message(1, &success.finish());
    }
    w.finish()
}

/// `WriteResult{ success: WriteSuccess{ path } }` / `{ error{...} }`.
pub fn encode_write_result(path: &str, text: &str, is_error: bool) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if is_error {
        let mut err = ProtoWriter::new();
        put_str(&mut err, 1, path);
        put_str(&mut err, 2, text);
        w.field_message(5, &err.finish());
    } else {
        let mut success = ProtoWriter::new();
        put_str(&mut success, 1, path);
        w.field_message(1, &success.finish());
    }
    w.finish()
}

/// `DeleteResult{ success: DeleteSuccess{ path } }` / `{ error{...} }`.
pub fn encode_delete_result(path: &str, text: &str, is_error: bool) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if is_error {
        let mut err = ProtoWriter::new();
        put_str(&mut err, 1, path);
        put_str(&mut err, 2, text);
        w.field_message(7, &err.finish());
    } else {
        let mut success = ProtoWriter::new();
        put_str(&mut success, 1, path);
        w.field_message(1, &success.finish());
    }
    w.finish()
}

/// `DiagnosticsResult{ success: DiagnosticsSuccess{ path } }` / `{ error{...} }`.
///
/// mikmik's LSP tool returns the diagnostics as text; the structured
/// `Diagnostic` list is left empty and the text is surfaced through the paired
/// interaction-update block, so the server sees a successful, empty run.
pub fn encode_diagnostics_result(path: &str, text: &str, is_error: bool) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if is_error {
        let mut err = ProtoWriter::new();
        put_str(&mut err, 1, path);
        put_str(&mut err, 2, text);
        w.field_message(2, &err.finish());
    } else {
        let mut success = ProtoWriter::new();
        put_str(&mut success, 1, path);
        w.field_message(1, &success.finish());
    }
    w.finish()
}

/// `ShellResult{ success: ShellSuccess{ command, working_directory, stdout } }`
/// / `{ failure{...} }`.
pub fn encode_shell_result(
    command: &str,
    working_directory: &str,
    text: &str,
    is_error: bool,
) -> Vec<u8> {
    let mut inner = ProtoWriter::new();
    put_str(&mut inner, 1, command);
    put_str(&mut inner, 2, working_directory);
    put_str(&mut inner, 5, text);
    let mut w = ProtoWriter::new();
    w.field_message(if is_error { 2 } else { 1 }, &inner.finish());
    w.finish()
}

/// One directory entry from an `ls` listing: a name and whether it is a folder.
pub struct LsEntry {
    pub name: String,
    pub is_dir: bool,
}

/// `LsResult{ success: LsSuccess{ directory_tree_root } }` / `{ error{...} }`,
/// building a single-level tree from the listed entries.
pub fn encode_ls_result(path: &str, entries: &[LsEntry], text: &str, is_error: bool) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if is_error {
        let mut err = ProtoWriter::new();
        put_str(&mut err, 1, path);
        put_str(&mut err, 2, text);
        w.field_message(2, &err.finish());
        return w.finish();
    }
    let root_path = if path.is_empty() { "." } else { path };
    let mut root = ProtoWriter::new();
    put_str(&mut root, 1, root_path);
    let mut file_count = 0i32;
    for entry in entries {
        if entry.is_dir {
            let mut dir = ProtoWriter::new();
            put_str(
                &mut dir,
                1,
                &format!("{}/{}", root_path.trim_end_matches('/'), entry.name),
            );
            root.field_message(2, &dir.finish());
        } else {
            let mut file = ProtoWriter::new();
            put_str(&mut file, 1, &entry.name);
            root.field_message(3, &file.finish());
            file_count += 1;
        }
    }
    put_bool(&mut root, 4, true);
    put_i32(&mut root, 6, file_count);
    let mut success = ProtoWriter::new();
    success.field_message(1, &root.finish());
    w.field_message(1, &success.finish());
    w.finish()
}

/// One parsed `file:line:content` grep hit.
pub struct GrepHit {
    pub file: String,
    pub line: i32,
    pub content: String,
    pub is_context: bool,
}

fn encode_grep_content_match(hit: &GrepHit) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    put_i32(&mut w, 1, hit.line);
    put_str(&mut w, 2, &hit.content);
    put_bool(&mut w, 4, hit.is_context);
    w.finish()
}

/// `GrepResult{ success: GrepSuccess{ pattern, path, workspace_results } }` /
/// `{ error{...} }`, in `content` output mode.
pub fn encode_grep_result(
    pattern: &str,
    path: &str,
    hits: &[GrepHit],
    is_error: bool,
    error_text: &str,
) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if is_error {
        let mut err = ProtoWriter::new();
        put_str(&mut err, 1, error_text);
        w.field_message(2, &err.finish());
        return w.finish();
    }
    let content = encode_grep_content_result(hits);
    let mut union = ProtoWriter::new();
    union.field_message(3, &content);
    let mut success = ProtoWriter::new();
    put_str(&mut success, 1, pattern);
    put_str(&mut success, 2, path);
    put_str(&mut success, 3, "content");
    let workspace_key = if path.is_empty() { "." } else { path };
    success.field_message(4, &map_string_bytes_entry(workspace_key, &union.finish()));
    w.field_message(1, &success.finish());
    w.finish()
}

fn encode_grep_content_result(hits: &[GrepHit]) -> Vec<u8> {
    let mut by_file: Vec<(String, Vec<&GrepHit>)> = Vec::new();
    for hit in hits {
        match by_file.iter_mut().find(|(f, _)| *f == hit.file) {
            Some((_, list)) => list.push(hit),
            None => by_file.push((hit.file.clone(), vec![hit])),
        }
    }
    let mut total_lines = 0i32;
    let mut total_matched = 0i32;
    let mut content = ProtoWriter::new();
    for (file, list) in &by_file {
        let mut file_match = ProtoWriter::new();
        put_str(&mut file_match, 1, file);
        for hit in list {
            file_match.field_message(2, &encode_grep_content_match(hit));
            total_lines += 1;
            if !hit.is_context {
                total_matched += 1;
            }
        }
        content.field_message(1, &file_match.finish());
    }
    put_i32(&mut content, 2, total_lines);
    put_i32(&mut content, 3, total_matched);
    content.finish()
}

/// `McpResult{ success: McpSuccess{ content:[text] } }` / `{ error{...} }` on
/// the exec channel (distinct from the history `McpToolResult`).
pub fn encode_mcp_exec_result(text: &str, is_error: bool) -> Vec<u8> {
    let mut w = ProtoWriter::new();
    if is_error {
        let mut err = ProtoWriter::new();
        put_str(&mut err, 1, text);
        w.field_message(2, &err.finish());
    } else {
        let mut success = ProtoWriter::new();
        success.field_message(1, &encode_mcp_text_content(text));
        w.field_message(1, &success.finish());
    }
    w.finish()
}

// ===========================================================================
// Decode: server messages
// ===========================================================================

/// A decoded `AgentServerMessage`.
#[derive(Debug, Clone)]
pub enum ServerMessage {
    /// A streamed assistant delta or turn-lifecycle event.
    Interaction(InteractionUpdate),
    /// The server asks the client to run a local tool.
    Exec(ExecServerMessage),
    /// The server aborts an in-flight exec by id.
    ExecAbort(u32),
    /// A conversation checkpoint carrying updated token usage.
    Checkpoint { used_tokens: Option<u32> },
    /// The server pulls or pushes a history blob.
    Kv(KvServerMessage),
    /// The server asks the client to approve a hosted action.
    Query(InteractionQuery),
    /// A frame whose top-level oneof this build does not model.
    Unknown,
}

/// Decode one `AgentServerMessage` from its protobuf bytes.
pub fn decode_server_message(bytes: &[u8]) -> Result<ServerMessage, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if wire != WIRE_LEN {
            r.skip(wire)?;
            continue;
        }
        let payload = r.bytes()?;
        return match field {
            1 => Ok(ServerMessage::Interaction(decode_interaction_update(
                payload,
            )?)),
            2 => Ok(ServerMessage::Exec(decode_exec_server_message(payload)?)),
            5 => Ok(ServerMessage::ExecAbort(decode_exec_abort(payload)?)),
            3 => Ok(ServerMessage::Checkpoint {
                used_tokens: decode_checkpoint_used_tokens(payload)?,
            }),
            4 => Ok(ServerMessage::Kv(decode_kv_server_message(payload)?)),
            7 => Ok(ServerMessage::Query(decode_interaction_query(payload)?)),
            _ => Ok(ServerMessage::Unknown),
        };
    }
    Ok(ServerMessage::Unknown)
}

// ---------------------------------------------------------------------------
// Decode: interaction updates (assistant deltas)
// ---------------------------------------------------------------------------

/// A streamed assistant delta or turn-lifecycle event.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionUpdate {
    TextDelta(String),
    ThinkingDelta(String),
    ThinkingCompleted,
    ToolCallStarted {
        call_id: String,
        tool_name: String,
    },
    PartialToolCall {
        call_id: String,
        args_text_delta: String,
    },
    ToolCallCompleted {
        call_id: String,
    },
    TokenDelta(i32),
    TurnEnded,
    Other,
}

fn decode_interaction_update(bytes: &[u8]) -> Result<InteractionUpdate, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 8 && wire == crate::protocol::protobuf::WIRE_VARINT {
            return Ok(InteractionUpdate::TokenDelta(read_token_delta(r.int32()?)));
        }
        if wire != WIRE_LEN {
            r.skip(wire)?;
            continue;
        }
        let payload = r.bytes()?;
        if let Some(update) = interaction_update_for_field(field, payload)? {
            return Ok(update);
        }
    }
    Ok(InteractionUpdate::Other)
}

/// The `tokens` inside a `TokenDeltaUpdate{ tokens }` sub-message, or the direct
/// value when the server inlines it. `read_token_delta` keeps the varint path
/// distinct from the length-delimited sub-message path below.
fn read_token_delta(tokens: i32) -> i32 {
    tokens
}

fn interaction_update_for_field(
    field: u32,
    payload: &[u8],
) -> Result<Option<InteractionUpdate>, ProtoError> {
    match field {
        1 => Ok(Some(InteractionUpdate::TextDelta(decode_single_string(
            payload,
        )?))),
        4 => Ok(Some(InteractionUpdate::ThinkingDelta(
            decode_single_string(payload)?,
        ))),
        5 => Ok(Some(InteractionUpdate::ThinkingCompleted)),
        2 => Ok(Some(decode_tool_call_started(payload)?)),
        7 => Ok(Some(decode_partial_tool_call(payload)?)),
        3 => Ok(Some(InteractionUpdate::ToolCallCompleted {
            call_id: decode_call_id(payload)?,
        })),
        8 => Ok(Some(InteractionUpdate::TokenDelta(
            decode_token_delta_message(payload)?,
        ))),
        14 => Ok(Some(InteractionUpdate::TurnEnded)),
        _ => Ok(None),
    }
}

fn decode_token_delta_message(bytes: &[u8]) -> Result<i32, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut tokens = 0;
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == crate::protocol::protobuf::WIRE_VARINT {
            tokens = r.int32()?;
        } else {
            r.skip(wire)?;
        }
    }
    Ok(tokens)
}

fn decode_tool_call_started(bytes: &[u8]) -> Result<InteractionUpdate, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut call_id = String::new();
    let mut tool_name = String::new();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => call_id = r.string()?,
            (2, WIRE_LEN) => tool_name = decode_tool_call_name(r.bytes()?)?,
            _ => r.skip(wire)?,
        }
    }
    Ok(InteractionUpdate::ToolCallStarted { call_id, tool_name })
}

fn decode_partial_tool_call(bytes: &[u8]) -> Result<InteractionUpdate, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut call_id = String::new();
    let mut args_text_delta = String::new();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => call_id = r.string()?,
            (3, WIRE_LEN) => args_text_delta = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(InteractionUpdate::PartialToolCall {
        call_id,
        args_text_delta,
    })
}

fn decode_call_id(bytes: &[u8]) -> Result<String, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut call_id = String::new();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == WIRE_LEN {
            call_id = r.string()?;
        } else {
            r.skip(wire)?;
        }
    }
    Ok(call_id)
}

/// Name a `ToolCall` by which tool-variant field is set (or the MCP tool name).
fn decode_tool_call_name(bytes: &[u8]) -> Result<String, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if wire != WIRE_LEN {
            r.skip(wire)?;
            continue;
        }
        let payload = r.bytes()?;
        if field == 57 {
            continue; // tool_call_id, not a tool variant
        }
        if field == 15 {
            return decode_mcp_call_name(payload);
        }
        if let Some(name) = tool_variant_name(field) {
            return Ok(name.to_string());
        }
    }
    Ok(String::new())
}

fn decode_mcp_call_name(bytes: &[u8]) -> Result<String, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == WIRE_LEN {
            return decode_mcp_args_name(r.bytes()?);
        }
        r.skip(wire)?;
    }
    Ok(String::new())
}

fn decode_mcp_args_name(bytes: &[u8]) -> Result<String, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut name = String::new();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => name = r.string()?,
            (5, WIRE_LEN) if name.is_empty() => name = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(name)
}

/// The tool name for a `ToolCall` oneof field number that carries a name.
fn tool_variant_name(field: u32) -> Option<&'static str> {
    Some(match field {
        1 => "shell",
        3 => "delete",
        4 => "glob",
        5 => "grep",
        8 => "read",
        12 => "edit",
        13 => "ls",
        24 | 37 => "web_fetch",
        18 => "web_search",
        61 => "read",
        62 => "bash",
        63 => "edit",
        64 => "write",
        65 => "grep",
        66 => "find",
        67 => "ls",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Decode: exec server message (tool args)
// ---------------------------------------------------------------------------

/// A tool the server asks the client to run, with its decoded arguments.
#[derive(Debug, Clone)]
pub struct ExecServerMessage {
    pub id: u32,
    pub exec_id: String,
    pub kind: ExecKind,
}

/// The tool and arguments carried by an `ExecServerMessage`.
#[derive(Debug, Clone)]
pub enum ExecKind {
    RequestContext,
    McpState,
    ExecuteHook(Option<u32>),
    Read(ReadArgs),
    Ls(LsArgs),
    Grep(GrepArgs),
    Write(WriteArgs),
    Delete(PathArgs),
    Shell(ShellArgs),
    Diagnostics(PathArgs),
    Mcp(McpArgs),
    PiRead(PiReadArgs),
    PiBash(PiBashArgs),
    PiEdit(PiEditArgs),
    PiWrite(PiWriteArgs),
    PiGrep(PiGrepArgs),
    PiFind(PiFindArgs),
    PiLs(PiLsArgs),
    /// A frame whose oneof this build does not model.
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub struct ReadArgs {
    pub path: String,
    pub tool_call_id: String,
    pub offset: Option<i32>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct PathArgs {
    pub path: String,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct LsArgs {
    pub path: String,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct WriteArgs {
    pub path: String,
    pub file_text: String,
    pub file_bytes: Vec<u8>,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct ShellArgs {
    pub command: String,
    pub working_directory: String,
    pub timeout: i32,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: String,
    pub glob: String,
    pub case_insensitive: bool,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct McpArgs {
    pub name: String,
    pub tool_call_id: String,
    /// Argument name → raw value bytes (the value is a JSON-encoded blob).
    pub args: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, Default)]
pub struct PiReadArgs {
    pub path: String,
    pub offset: Option<i32>,
    pub limit: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct PiBashArgs {
    pub command: String,
    pub timeout: Option<f64>,
}

/// One search/replace pair for a `pi_edit` frame.
#[derive(Debug, Clone, Default)]
pub struct PiEditReplacement {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, Default)]
pub struct PiEditArgs {
    pub path: String,
    pub edits: Vec<PiEditReplacement>,
}

#[derive(Debug, Clone, Default)]
pub struct PiWriteArgs {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct PiGrepArgs {
    pub pattern: String,
    pub path: String,
    pub glob: String,
    pub ignore_case: bool,
    pub literal: bool,
    pub limit: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct PiFindArgs {
    pub pattern: String,
    pub path: String,
    pub limit: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct PiLsArgs {
    pub path: String,
    pub limit: Option<i32>,
}

fn decode_exec_server_message(bytes: &[u8]) -> Result<ExecServerMessage, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut id = 0u32;
    let mut exec_id = String::new();
    let mut kind = ExecKind::Unknown;
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, crate::protocol::protobuf::WIRE_VARINT) => id = r.varint()? as u32,
            (15, WIRE_LEN) => exec_id = r.string()?,
            (_, WIRE_LEN) => {
                let payload = r.bytes()?;
                if let Some(decoded) = exec_kind_for_field(field, payload)? {
                    kind = decoded;
                }
            }
            _ => r.skip(wire)?,
        }
    }
    Ok(ExecServerMessage { id, exec_id, kind })
}

fn exec_kind_for_field(field: u32, payload: &[u8]) -> Result<Option<ExecKind>, ProtoError> {
    if let Some(kind) = exec_kind_native(field, payload)? {
        return Ok(Some(kind));
    }
    exec_kind_pi(field, payload)
}

fn exec_kind_native(field: u32, payload: &[u8]) -> Result<Option<ExecKind>, ProtoError> {
    Ok(Some(match field {
        10 => ExecKind::RequestContext,
        36 => ExecKind::McpState,
        27 => ExecKind::ExecuteHook(decode_hook_request_case(payload)?),
        7 | 29 => ExecKind::Read(decode_read_args(payload)?),
        8 => ExecKind::Ls(decode_ls_args(payload)?),
        5 => ExecKind::Grep(decode_grep_args(payload)?),
        3 => ExecKind::Write(decode_write_args(payload)?),
        4 => ExecKind::Delete(decode_path_args(payload)?),
        2 | 14 | 52 | 55 => ExecKind::Shell(decode_shell_args(payload)?),
        9 => ExecKind::Diagnostics(decode_path_args(payload)?),
        11 => ExecKind::Mcp(decode_mcp_args(payload)?),
        _ => return Ok(None),
    }))
}

fn exec_kind_pi(field: u32, payload: &[u8]) -> Result<Option<ExecKind>, ProtoError> {
    Ok(Some(match field {
        45 => ExecKind::PiRead(decode_pi_read_args(payload)?),
        46 => ExecKind::PiBash(decode_pi_bash_args(payload)?),
        47 => ExecKind::PiEdit(decode_pi_edit_args(payload)?),
        48 => ExecKind::PiWrite(decode_pi_write_args(payload)?),
        49 => ExecKind::PiGrep(decode_pi_grep_args(payload)?),
        50 => ExecKind::PiFind(decode_pi_find_args(payload)?),
        51 => ExecKind::PiLs(decode_pi_ls_args(payload)?),
        _ => return Ok(None),
    }))
}

/// The `ExecuteHookRequest` oneof case, mapped to the matching response field.
fn decode_hook_request_case(bytes: &[u8]) -> Result<Option<u32>, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == WIRE_LEN {
            return decode_hook_inner_case(r.bytes()?);
        }
        r.skip(wire)?;
    }
    Ok(None)
}

fn decode_hook_inner_case(bytes: &[u8]) -> Result<Option<u32>, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    if r.is_empty() {
        return Ok(None);
    }
    let (field, _wire) = r.tag()?;
    // The response field number matches the request oneof field number for
    // every hook case, so the case number is the answer.
    Ok(Some(field))
}

fn decode_read_args(bytes: &[u8]) -> Result<ReadArgs, ProtoError> {
    let mut args = ReadArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.path = r.string()?,
            (2, WIRE_LEN) => args.tool_call_id = r.string()?,
            (4, crate::protocol::protobuf::WIRE_VARINT) => args.offset = Some(r.int32()?),
            (5, crate::protocol::protobuf::WIRE_VARINT) => args.limit = Some(r.varint()? as u32),
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_path_args(bytes: &[u8]) -> Result<PathArgs, ProtoError> {
    let mut args = PathArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.path = r.string()?,
            (2, WIRE_LEN) => args.tool_call_id = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_ls_args(bytes: &[u8]) -> Result<LsArgs, ProtoError> {
    let mut args = LsArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.path = r.string()?,
            (3, WIRE_LEN) => args.tool_call_id = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_write_args(bytes: &[u8]) -> Result<WriteArgs, ProtoError> {
    let mut args = WriteArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.path = r.string()?,
            (2, WIRE_LEN) => args.file_text = r.string()?,
            (3, WIRE_LEN) => args.tool_call_id = r.string()?,
            (5, WIRE_LEN) => args.file_bytes = r.bytes()?.to_vec(),
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_shell_args(bytes: &[u8]) -> Result<ShellArgs, ProtoError> {
    let mut args = ShellArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.command = r.string()?,
            (2, WIRE_LEN) => args.working_directory = r.string()?,
            (3, crate::protocol::protobuf::WIRE_VARINT) => args.timeout = r.int32()?,
            (4, WIRE_LEN) => args.tool_call_id = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_grep_args(bytes: &[u8]) -> Result<GrepArgs, ProtoError> {
    let mut args = GrepArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.pattern = r.string()?,
            (2, WIRE_LEN) => args.path = r.string()?,
            (3, WIRE_LEN) => args.glob = r.string()?,
            (8, crate::protocol::protobuf::WIRE_VARINT) => args.case_insensitive = r.bool()?,
            (14, WIRE_LEN) => args.tool_call_id = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_mcp_args(bytes: &[u8]) -> Result<McpArgs, ProtoError> {
    let mut args = McpArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.name = r.string()?,
            (2, WIRE_LEN) => args.args.push(decode_map_entry(r.bytes()?)?),
            (3, WIRE_LEN) => args.tool_call_id = r.string()?,
            (5, WIRE_LEN) if args.name.is_empty() => args.name = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_map_entry(bytes: &[u8]) -> Result<(String, Vec<u8>), ProtoError> {
    let mut key = String::new();
    let mut value = Vec::new();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => key = r.string()?,
            (2, WIRE_LEN) => value = r.bytes()?.to_vec(),
            _ => r.skip(wire)?,
        }
    }
    Ok((key, value))
}

fn decode_pi_read_args(bytes: &[u8]) -> Result<PiReadArgs, ProtoError> {
    let mut args = PiReadArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.path = r.string()?,
            (2, crate::protocol::protobuf::WIRE_VARINT) => args.offset = Some(r.int32()?),
            (3, crate::protocol::protobuf::WIRE_VARINT) => args.limit = Some(r.int32()?),
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_pi_bash_args(bytes: &[u8]) -> Result<PiBashArgs, ProtoError> {
    let mut args = PiBashArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.command = r.string()?,
            (2, crate::protocol::protobuf::WIRE_FIXED64) => args.timeout = Some(r.double()?),
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_pi_edit_args(bytes: &[u8]) -> Result<PiEditArgs, ProtoError> {
    let mut args = PiEditArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.path = r.string()?,
            (2, WIRE_LEN) => args.edits.push(decode_pi_edit_replacement(r.bytes()?)?),
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_pi_edit_replacement(bytes: &[u8]) -> Result<PiEditReplacement, ProtoError> {
    let mut edit = PiEditReplacement::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => edit.old_text = r.string()?,
            (2, WIRE_LEN) => edit.new_text = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(edit)
}

fn decode_pi_write_args(bytes: &[u8]) -> Result<PiWriteArgs, ProtoError> {
    let mut args = PiWriteArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.path = r.string()?,
            (2, WIRE_LEN) => args.content = r.string()?,
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_pi_grep_args(bytes: &[u8]) -> Result<PiGrepArgs, ProtoError> {
    let mut args = PiGrepArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.pattern = r.string()?,
            (2, WIRE_LEN) => args.path = r.string()?,
            (3, WIRE_LEN) => args.glob = r.string()?,
            (4, crate::protocol::protobuf::WIRE_VARINT) => args.ignore_case = r.bool()?,
            (5, crate::protocol::protobuf::WIRE_VARINT) => args.literal = r.bool()?,
            (7, crate::protocol::protobuf::WIRE_VARINT) => args.limit = Some(r.int32()?),
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_pi_find_args(bytes: &[u8]) -> Result<PiFindArgs, ProtoError> {
    let mut args = PiFindArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.pattern = r.string()?,
            (2, WIRE_LEN) => args.path = r.string()?,
            (3, crate::protocol::protobuf::WIRE_VARINT) => args.limit = Some(r.int32()?),
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_pi_ls_args(bytes: &[u8]) -> Result<PiLsArgs, ProtoError> {
    let mut args = PiLsArgs::default();
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => args.path = r.string()?,
            (2, crate::protocol::protobuf::WIRE_VARINT) => args.limit = Some(r.int32()?),
            _ => r.skip(wire)?,
        }
    }
    Ok(args)
}

fn decode_exec_abort(bytes: &[u8]) -> Result<u32, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == WIRE_LEN {
            return decode_id_message(r.bytes()?);
        }
        r.skip(wire)?;
    }
    Ok(0)
}

fn decode_id_message(bytes: &[u8]) -> Result<u32, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut id = 0u32;
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == crate::protocol::protobuf::WIRE_VARINT {
            id = r.varint()? as u32;
        } else {
            r.skip(wire)?;
        }
    }
    Ok(id)
}

// ---------------------------------------------------------------------------
// Decode: KV server message (blob requests)
// ---------------------------------------------------------------------------

/// A blob pull or push from the server.
#[derive(Debug, Clone)]
pub enum KvServerMessage {
    GetBlob {
        id: u32,
        blob_id: Vec<u8>,
    },
    SetBlob {
        id: u32,
        blob_id: Vec<u8>,
        blob_data: Vec<u8>,
    },
}

fn decode_kv_server_message(bytes: &[u8]) -> Result<KvServerMessage, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut id = 0u32;
    let mut get_blob: Option<Vec<u8>> = None;
    let mut set_blob: Option<(Vec<u8>, Vec<u8>)> = None;
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, crate::protocol::protobuf::WIRE_VARINT) => id = r.varint()? as u32,
            (2, WIRE_LEN) => get_blob = Some(decode_get_blob_args(r.bytes()?)?),
            (3, WIRE_LEN) => set_blob = Some(decode_set_blob_args(r.bytes()?)?),
            _ => r.skip(wire)?,
        }
    }
    if let Some((blob_id, blob_data)) = set_blob {
        return Ok(KvServerMessage::SetBlob {
            id,
            blob_id,
            blob_data,
        });
    }
    Ok(KvServerMessage::GetBlob {
        id,
        blob_id: get_blob.unwrap_or_default(),
    })
}

fn decode_get_blob_args(bytes: &[u8]) -> Result<Vec<u8>, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut blob_id = Vec::new();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == WIRE_LEN {
            blob_id = r.bytes()?.to_vec();
        } else {
            r.skip(wire)?;
        }
    }
    Ok(blob_id)
}

fn decode_set_blob_args(bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut blob_id = Vec::new();
    let mut blob_data = Vec::new();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => blob_id = r.bytes()?.to_vec(),
            (2, WIRE_LEN) => blob_data = r.bytes()?.to_vec(),
            _ => r.skip(wire)?,
        }
    }
    Ok((blob_id, blob_data))
}

// ---------------------------------------------------------------------------
// Decode: interaction query (approval gates)
// ---------------------------------------------------------------------------

/// A hosted-action approval gate the server blocks the turn on.
#[derive(Debug, Clone, PartialEq)]
pub struct InteractionQuery {
    pub id: u32,
    pub case: QueryCase,
}

/// Which hosted action an `InteractionQuery` asks the client to approve.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryCase {
    WebSearch,
    AskQuestion,
    SwitchMode,
    ExaSearch,
    ExaFetch,
    CreatePlan,
    SetupVm,
    WebFetch,
    /// An unmodelled length-delimited query on field number `n`.
    Unknown(u32),
}

fn decode_interaction_query(bytes: &[u8]) -> Result<InteractionQuery, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut id = 0u32;
    let mut case = QueryCase::Unknown(0);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, crate::protocol::protobuf::WIRE_VARINT) => id = r.varint()? as u32,
            (_, WIRE_LEN) => {
                r.bytes()?;
                if matches!(case, QueryCase::Unknown(0)) {
                    case = query_case_for_field(field);
                }
            }
            _ => r.skip(wire)?,
        }
    }
    Ok(InteractionQuery { id, case })
}

fn query_case_for_field(field: u32) -> QueryCase {
    match field {
        2 => QueryCase::WebSearch,
        3 => QueryCase::AskQuestion,
        4 => QueryCase::SwitchMode,
        5 => QueryCase::ExaSearch,
        6 => QueryCase::ExaFetch,
        7 => QueryCase::CreatePlan,
        8 => QueryCase::SetupVm,
        9 => QueryCase::WebFetch,
        other => QueryCase::Unknown(other),
    }
}

// ---------------------------------------------------------------------------
// Decode: conversation checkpoint (usage)
// ---------------------------------------------------------------------------

fn decode_checkpoint_used_tokens(bytes: &[u8]) -> Result<Option<u32>, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 5 && wire == WIRE_LEN {
            return decode_token_details_used(r.bytes()?);
        }
        r.skip(wire)?;
    }
    Ok(None)
}

fn decode_token_details_used(bytes: &[u8]) -> Result<Option<u32>, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut used = None;
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == crate::protocol::protobuf::WIRE_VARINT {
            used = Some(r.varint()? as u32);
        } else {
            r.skip(wire)?;
        }
    }
    Ok(used)
}

// ---------------------------------------------------------------------------
// Decode helpers
// ---------------------------------------------------------------------------

fn decode_single_string(bytes: &[u8]) -> Result<String, ProtoError> {
    let mut r = ProtoReader::new(bytes);
    let mut text = String::new();
    while !r.is_empty() {
        let (field, wire) = r.tag()?;
        if field == 1 && wire == WIRE_LEN {
            text = r.string()?;
        } else {
            r.skip(wire)?;
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_field_message(bytes: &[u8], want: u32) -> Vec<u8> {
        let mut r = ProtoReader::new(bytes);
        while !r.is_empty() {
            let (field, wire) = r.tag().unwrap();
            if field == want && wire == WIRE_LEN {
                return r.bytes().unwrap().to_vec();
            }
            r.skip(wire).unwrap();
        }
        panic!("field {want} not found");
    }

    #[test]
    fn blob_id_is_sha256() {
        let id = blob_id(b"hello");
        assert_eq!(id.len(), 32);
        assert_eq!(blob_id(b"hello"), id);
        assert_ne!(blob_id(b"world"), id);
    }

    #[test]
    fn run_request_wraps_the_agent_run_request() {
        let state = encode_conversation_state(&ConversationStateData {
            root_prompt_message_ids: vec![blob_id(b"sys")],
            turn_ids: vec![],
        });
        let action = encode_action(
            &ActionData::UserMessage {
                text: "hi".to_string(),
                message_id: "m1".to_string(),
            },
            &encode_request_context(&[], &[]),
        );
        let bytes = encode_run_request_message(&RunRequestData {
            conversation_state: state,
            action,
            model: ModelData {
                model_id: "gpt-5".to_string(),
                display_model_id: "gpt-5".to_string(),
                display_name: "GPT-5".to_string(),
                max_mode: false,
                parameters: vec![],
            },
            conversation_id: "c1".to_string(),
            custom_system_prompt: None,
        });
        // AgentClientMessage.run_request = field 1.
        let run = read_field_message(&bytes, 1);
        // AgentRunRequest.conversation_state = 1, action = 2, model_details = 3.
        let _ = read_field_message(&run, 1);
        let _ = read_field_message(&run, 2);
        let _ = read_field_message(&run, 3);
    }

    #[test]
    fn conversation_state_carries_blob_ids() {
        let root = blob_id(b"sys");
        let turn = blob_id(b"turn");
        let bytes = encode_conversation_state(&ConversationStateData {
            root_prompt_message_ids: vec![root],
            turn_ids: vec![turn],
        });
        let mut r = ProtoReader::new(&bytes);
        let (f1, _) = r.tag().unwrap();
        assert_eq!(f1, 1);
        assert_eq!(r.bytes().unwrap(), root);
        let (f8, _) = r.tag().unwrap();
        assert_eq!(f8, 8);
        assert_eq!(r.bytes().unwrap(), turn);
    }

    #[test]
    fn exec_result_message_sets_id_exec_id_and_result_field() {
        let body = encode_pi_result("done", false);
        let bytes = encode_exec_result_message(7, "e1", 46, &body);
        let exec = read_field_message(&bytes, 2);
        let mut r = ProtoReader::new(&exec);
        assert_eq!(
            r.tag().unwrap(),
            (1, crate::protocol::protobuf::WIRE_VARINT)
        );
        assert_eq!(r.varint().unwrap(), 7);
        assert_eq!(r.tag().unwrap(), (15, WIRE_LEN));
        assert_eq!(r.string().unwrap(), "e1");
        assert_eq!(r.tag().unwrap(), (46, WIRE_LEN));
    }

    #[test]
    fn interaction_approved_uses_the_response_field() {
        let bytes = encode_interaction_approved_message(3, 2);
        let inner = read_field_message(&bytes, 6);
        let mut r = ProtoReader::new(&inner);
        assert_eq!(
            r.tag().unwrap(),
            (1, crate::protocol::protobuf::WIRE_VARINT)
        );
        assert_eq!(r.varint().unwrap(), 3);
        assert_eq!(r.tag().unwrap(), (2, WIRE_LEN));
    }

    #[test]
    fn server_message_routes_text_delta() {
        // AgentServerMessage{ interaction_update{ text_delta{ text } } }.
        let mut text_delta = ProtoWriter::new();
        text_delta.field_string(1, "hi");
        let mut update = ProtoWriter::new();
        update.field_message(1, &text_delta.finish());
        let mut msg = ProtoWriter::new();
        msg.field_message(1, &update.finish());
        let decoded = decode_server_message(&msg.finish()).unwrap();
        match decoded {
            ServerMessage::Interaction(InteractionUpdate::TextDelta(t)) => assert_eq!(t, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn server_message_routes_exec_pi_bash() {
        // ExecServerMessage{ id, exec_id, pi_bash_args{ command } }.
        let mut pi = ProtoWriter::new();
        pi.field_string(1, "ls -la");
        let mut exec = ProtoWriter::new();
        exec.field_varint(1, 4);
        exec.field_string(15, "exec-1");
        exec.field_message(46, &pi.finish());
        let mut msg = ProtoWriter::new();
        msg.field_message(2, &exec.finish());
        match decode_server_message(&msg.finish()).unwrap() {
            ServerMessage::Exec(exec) => {
                assert_eq!(exec.id, 4);
                assert_eq!(exec.exec_id, "exec-1");
                match exec.kind {
                    ExecKind::PiBash(args) => assert_eq!(args.command, "ls -la"),
                    other => panic!("unexpected kind: {other:?}"),
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn server_message_routes_kv_get_blob() {
        let want = blob_id(b"sys").to_vec();
        let mut get = ProtoWriter::new();
        get.field_bytes(1, &want);
        let mut kv = ProtoWriter::new();
        kv.field_varint(1, 9);
        kv.field_message(2, &get.finish());
        let mut msg = ProtoWriter::new();
        msg.field_message(4, &kv.finish());
        match decode_server_message(&msg.finish()).unwrap() {
            ServerMessage::Kv(KvServerMessage::GetBlob { id, blob_id }) => {
                assert_eq!(id, 9);
                assert_eq!(blob_id, want);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn server_message_routes_interaction_query_webfetch() {
        // InteractionQuery{ id, web_fetch_request_query{...} } on field 9.
        let mut query = ProtoWriter::new();
        query.field_varint(1, 12);
        query.field_message(9, &[0x0a, 0x00]);
        let mut msg = ProtoWriter::new();
        msg.field_message(7, &query.finish());
        match decode_server_message(&msg.finish()).unwrap() {
            ServerMessage::Query(q) => {
                assert_eq!(q.id, 12);
                assert_eq!(q.case, QueryCase::WebFetch);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_call_started_names_the_pi_variant() {
        // ToolCallStartedUpdate{ call_id, tool_call{ pi_bash_tool_call{} } }.
        let mut tool_call = ProtoWriter::new();
        tool_call.field_message(62, &[]);
        let mut started = ProtoWriter::new();
        started.field_string(1, "call-1");
        started.field_message(2, &tool_call.finish());
        let mut update = ProtoWriter::new();
        update.field_message(2, &started.finish());
        match decode_interaction_update(&update.finish()).unwrap() {
            InteractionUpdate::ToolCallStarted { call_id, tool_name } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(tool_name, "bash");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn checkpoint_reads_used_tokens() {
        // ConversationStateStructure{ token_details{ used_tokens } } on field 5.
        let mut details = ProtoWriter::new();
        details.field_varint(1, 4096);
        let mut state = ProtoWriter::new();
        state.field_message(5, &details.finish());
        let mut msg = ProtoWriter::new();
        msg.field_message(3, &state.finish());
        match decode_server_message(&msg.finish()).unwrap() {
            ServerMessage::Checkpoint { used_tokens } => assert_eq!(used_tokens, Some(4096)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn grep_result_groups_hits_by_file() {
        let hits = vec![
            GrepHit {
                file: "a.rs".to_string(),
                line: 3,
                content: "fn main".to_string(),
                is_context: false,
            },
            GrepHit {
                file: "a.rs".to_string(),
                line: 4,
                content: "    body".to_string(),
                is_context: true,
            },
        ];
        let bytes = encode_grep_result("fn", "src", &hits, false, "");
        // GrepResult.success = field 1.
        let _ = read_field_message(&bytes, 1);
    }

    #[test]
    fn ls_result_builds_a_single_level_tree() {
        let entries = vec![
            LsEntry {
                name: "src".to_string(),
                is_dir: true,
            },
            LsEntry {
                name: "main.rs".to_string(),
                is_dir: false,
            },
        ];
        let bytes = encode_ls_result("proj", &entries, "", false);
        let success = read_field_message(&bytes, 1);
        let _root = read_field_message(&success, 1);
    }

    #[test]
    fn history_tool_call_round_trips_the_name() {
        let call = HistoryToolCall {
            tool_name: "read_file".to_string(),
            tool_call_id: "tc1".to_string(),
            args: vec![("path".to_string(), b"\"a.rs\"".to_vec())],
            result: Some(HistoryToolResult {
                text: "contents".to_string(),
                is_error: false,
            }),
        };
        let step = encode_step_tool_call(&call);
        // ConversationStep.tool_call = field 2 → ToolCall.mcp_tool_call = field 15.
        let tool_call = read_field_message(&step, 2);
        let mcp = read_field_message(&tool_call, 15);
        let args = read_field_message(&mcp, 1);
        assert_eq!(decode_mcp_args_name(&args).unwrap(), "read_file");
    }
}
