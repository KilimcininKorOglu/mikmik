//! providers/cursor/exec.rs — run one exec frame and build its reply.
//!
//! The server drives tools over the exec channel: an `ExecServerMessage` carries
//! a tool's arguments, the client runs the matching mikmik tool through the
//! `CursorExecHandlers` bridge, and answers with an `ExecClientMessage` whose
//! result oneof case is set. A frame this build cannot serve is answered with a
//! `throw` rather than an unset-oneof result the server would read as a silent
//! empty success.
//!
//! mikmik's tools return plain text, so the native `ls`/`grep` result trees are
//! reconstructed by parsing that text; the pi-tool family, `mcp`, `read`,
//! `write`, `delete`, `shell` and `diagnostics` results carry the text directly.

use super::proto::{self, ExecKind, ExecServerMessage, GrepHit, LsEntry};
use super::CursorExecHandlers;

// ExecClientMessage result oneof field numbers.
const F_SHELL: u32 = 2;
const F_WRITE: u32 = 3;
const F_DELETE: u32 = 4;
const F_GREP: u32 = 5;
const F_READ: u32 = 7;
const F_LS: u32 = 8;
const F_DIAGNOSTICS: u32 = 9;
const F_REQUEST_CONTEXT: u32 = 10;
const F_MCP: u32 = 11;
const F_MCP_STATE: u32 = 36;
const F_EXECUTE_HOOK: u32 = 27;
const F_PI_READ: u32 = 46;
const F_PI_BASH: u32 = 47;
const F_PI_EDIT: u32 = 48;
const F_PI_WRITE: u32 = 49;
const F_PI_GREP: u32 = 50;
const F_PI_FIND: u32 = 51;
const F_PI_LS: u32 = 52;

/// Run one exec frame and return the `AgentClientMessage` bytes to send back.
pub async fn handle_exec(
    exec: &ExecServerMessage,
    handlers: &dyn CursorExecHandlers,
    rules: &[Vec<u8>],
    tools: &[Vec<u8>],
) -> Vec<u8> {
    let id = exec.id;
    let exec_id = exec.exec_id.as_str();
    match &exec.kind {
        ExecKind::RequestContext => {
            let ctx = proto::encode_request_context(rules, tools);
            let body = proto::encode_request_context_result(&ctx);
            proto::encode_exec_result_message(id, exec_id, F_REQUEST_CONTEXT, &body)
        }
        ExecKind::McpState => {
            let body = proto::encode_mcp_state_result(tools);
            proto::encode_exec_result_message(id, exec_id, F_MCP_STATE, &body)
        }
        ExecKind::ExecuteHook(Some(case)) => {
            let body = proto::encode_hook_result(*case);
            proto::encode_exec_result_message(id, exec_id, F_EXECUTE_HOOK, &body)
        }
        ExecKind::ExecuteHook(None) => {
            proto::encode_exec_throw_message(id, "Unmodelled hook request", "unknown_hook")
        }
        ExecKind::Unknown => {
            proto::encode_exec_throw_message(id, "Unknown exec message variant", "unknown_exec")
        }
        kind => handle_tool(id, exec_id, kind, handlers).await,
    }
}

async fn handle_tool(
    id: u32,
    exec_id: &str,
    kind: &ExecKind,
    handlers: &dyn CursorExecHandlers,
) -> Vec<u8> {
    let (field, body) = match kind {
        ExecKind::Read(a) => native_read(a, handlers).await,
        ExecKind::Ls(a) => native_ls(a, handlers).await,
        ExecKind::Grep(a) => native_grep(a, handlers).await,
        ExecKind::Write(a) => native_write(a, handlers).await,
        ExecKind::Delete(a) => native_delete(a, handlers).await,
        ExecKind::Shell(a) => native_shell(a, handlers).await,
        ExecKind::Diagnostics(a) => native_diagnostics(a, handlers).await,
        ExecKind::Mcp(a) => native_mcp(a, handlers).await,
        ExecKind::PiRead(a) => pi_read(a, handlers).await,
        ExecKind::PiBash(a) => pi_bash(a, handlers).await,
        ExecKind::PiEdit(a) => pi_edit(a, handlers).await,
        ExecKind::PiWrite(a) => pi_write(a, handlers).await,
        ExecKind::PiGrep(a) => pi_grep(a, handlers).await,
        ExecKind::PiFind(a) => pi_find(a, handlers).await,
        ExecKind::PiLs(a) => pi_ls(a, handlers).await,
        // RequestContext / McpState / ExecuteHook / Unknown are handled above.
        _ => {
            return proto::encode_exec_throw_message(id, "Unhandled exec variant", "unhandled_exec")
        }
    };
    proto::encode_exec_result_message(id, exec_id, field, &body)
}

// ---------------------------------------------------------------------------
// Native tool family
// ---------------------------------------------------------------------------

async fn native_read(a: &proto::ReadArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h
        .read(&a.path, a.offset.map(i64::from), a.limit.map(i64::from))
        .await;
    (
        F_READ,
        proto::encode_read_result(&a.path, &out.text, out.is_error),
    )
}

async fn native_write(a: &proto::WriteArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let content = if a.file_text.is_empty() {
        String::from_utf8_lossy(&a.file_bytes).into_owned()
    } else {
        a.file_text.clone()
    };
    let out = h.write(&a.path, &content).await;
    (
        F_WRITE,
        proto::encode_write_result(&a.path, &out.text, out.is_error),
    )
}

async fn native_delete(a: &proto::PathArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h.delete(&a.path).await;
    (
        F_DELETE,
        proto::encode_delete_result(&a.path, &out.text, out.is_error),
    )
}

async fn native_shell(a: &proto::ShellArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let timeout = (a.timeout > 0).then_some(i64::from(a.timeout));
    let out = h.shell(&a.command, &a.working_directory, timeout).await;
    (
        F_SHELL,
        proto::encode_shell_result(&a.command, &a.working_directory, &out.text, out.is_error),
    )
}

async fn native_diagnostics(a: &proto::PathArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h.diagnostics(&a.path).await;
    (
        F_DIAGNOSTICS,
        proto::encode_diagnostics_result(&a.path, &out.text, out.is_error),
    )
}

async fn native_ls(a: &proto::LsArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h.ls(&a.path).await;
    let entries = parse_ls_entries(&out.text);
    (
        F_LS,
        proto::encode_ls_result(&a.path, &entries, &out.text, out.is_error),
    )
}

async fn native_grep(a: &proto::GrepArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h
        .grep(&a.pattern, &a.path, &a.glob, a.case_insensitive)
        .await;
    let hits = parse_grep_hits(&out.text);
    (
        F_GREP,
        proto::encode_grep_result(&a.pattern, &a.path, &hits, out.is_error, &out.text),
    )
}

async fn native_mcp(a: &proto::McpArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let args_json = mcp_args_json(&a.args);
    let out = h.mcp(&a.name, &args_json).await;
    (
        F_MCP,
        proto::encode_mcp_exec_result(&out.text, out.is_error),
    )
}

// ---------------------------------------------------------------------------
// Pi tool family (output-string results)
// ---------------------------------------------------------------------------

async fn pi_read(a: &proto::PiReadArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h
        .read(&a.path, a.offset.map(i64::from), a.limit.map(i64::from))
        .await;
    (F_PI_READ, proto::encode_pi_result(&out.text, out.is_error))
}

async fn pi_bash(a: &proto::PiBashArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    // `pi_bash` carries its timeout in seconds; the shell bridge expects
    // milliseconds, so a supplied value is scaled here.
    let timeout = a.timeout.map(|t| (t * 1000.0) as i64);
    let out = h.shell(&a.command, "", timeout).await;
    (F_PI_BASH, proto::encode_pi_result(&out.text, out.is_error))
}

async fn pi_edit(a: &proto::PiEditArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let edits: Vec<(String, String)> = a
        .edits
        .iter()
        .map(|e| (e.old_text.clone(), e.new_text.clone()))
        .collect();
    let out = h.edit(&a.path, &edits).await;
    (F_PI_EDIT, proto::encode_pi_result(&out.text, out.is_error))
}

async fn pi_write(a: &proto::PiWriteArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h.write(&a.path, &a.content).await;
    (F_PI_WRITE, proto::encode_pi_result(&out.text, out.is_error))
}

async fn pi_grep(a: &proto::PiGrepArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h.grep(&a.pattern, &a.path, &a.glob, a.ignore_case).await;
    (F_PI_GREP, proto::encode_pi_result(&out.text, out.is_error))
}

async fn pi_find(a: &proto::PiFindArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h.find(&a.pattern, &a.path).await;
    (F_PI_FIND, proto::encode_pi_result(&out.text, out.is_error))
}

async fn pi_ls(a: &proto::PiLsArgs, h: &dyn CursorExecHandlers) -> (u32, Vec<u8>) {
    let out = h.ls(&a.path).await;
    (F_PI_LS, proto::encode_pi_result(&out.text, out.is_error))
}

// ---------------------------------------------------------------------------
// Text parsing helpers
// ---------------------------------------------------------------------------

/// Reassemble the tool arguments into a JSON object string for the bridge.
///
/// Each value is the JSON-encoded bytes Cursor sent for that argument; an
/// unparseable value falls back to its raw string.
fn mcp_args_json(args: &[(String, Vec<u8>)]) -> String {
    let mut map = serde_json::Map::new();
    for (name, value) in args {
        let parsed = serde_json::from_slice::<serde_json::Value>(value).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(value).into_owned())
        });
        map.insert(name.clone(), parsed);
    }
    serde_json::Value::Object(map).to_string()
}

/// Parse a directory listing's text into single-level entries.
fn parse_ls_entries(text: &str) -> Vec<LsEntry> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('['))
        .map(|line| {
            let name = line.split(" (").next().unwrap_or(line);
            match name.strip_suffix('/') {
                Some(dir) => LsEntry {
                    name: dir.to_string(),
                    is_dir: true,
                },
                None => LsEntry {
                    name: name.to_string(),
                    is_dir: false,
                },
            }
        })
        .collect()
}

/// Parse `file:line:content` (and `file-line-context`) grep lines into hits.
fn parse_grep_hits(text: &str) -> Vec<GrepHit> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('['))
        .filter_map(parse_grep_line)
        .collect()
}

fn parse_grep_line(line: &str) -> Option<GrepHit> {
    for (sep, is_context) in [(':', false), ('-', true)] {
        if let Some(hit) = split_grep_line(line, sep, is_context) {
            return Some(hit);
        }
    }
    None
}

/// Split on the first `<sep><digits><sep>` boundary: `file:12:content`.
fn split_grep_line(line: &str, sep: char, is_context: bool) -> Option<GrepHit> {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(sep) {
        let first = search_from + rel;
        let rest = &line[first + 1..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            let after = &rest[digits.len()..];
            if let Some(content) = after.strip_prefix(sep) {
                let line_number = digits.parse::<i32>().ok()?;
                return Some(GrepHit {
                    file: line[..first].to_string(),
                    line: line_number,
                    content: content.strip_prefix(' ').unwrap_or(content).to_string(),
                    is_context,
                });
            }
        }
        search_from = first + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_entries_split_dirs_and_files() {
        let entries = parse_ls_entries("[header]\nsrc/\nmain.rs (1.2k)\n\n");
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "src");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].name, "main.rs");
    }

    #[test]
    fn grep_line_parses_file_line_content() {
        let hit = parse_grep_line("src/main.rs:42: fn main() {").unwrap();
        assert_eq!(hit.file, "src/main.rs");
        assert_eq!(hit.line, 42);
        assert_eq!(hit.content, "fn main() {");
        assert!(!hit.is_context);
    }

    #[test]
    fn grep_context_line_uses_dash_separator() {
        let hit = parse_grep_line("a.rs-7- context").unwrap();
        assert_eq!(hit.file, "a.rs");
        assert_eq!(hit.line, 7);
        assert!(hit.is_context);
    }

    #[test]
    fn mcp_args_json_reassembles_values() {
        let args = vec![
            ("path".to_string(), b"\"a.rs\"".to_vec()),
            ("count".to_string(), b"3".to_vec()),
        ];
        let json = mcp_args_json(&args);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["path"], "a.rs");
        assert_eq!(parsed["count"], 3);
    }
}
