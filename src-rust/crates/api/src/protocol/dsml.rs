// protocol::dsml — DeepSeek Markup Language tool-call envelopes.
//
// DeepSeek V4-family models emit tool calls as tagged envelopes inside plain
// `delta.content` SSE text instead of OpenAI-style `tool_calls` JSON. Without a
// parser the raw markup reaches the user as assistant text and the tool never
// runs, which breaks the agent loop (upstream issue #395).
//
// Grammar, taken from the reference implementations rather than guessed
// (vllm-project/vllm#53227 carries the server-side regex, zeroclaw-labs/zeroclaw#9723
// the canonical fullwidth spec):
//
//   <｜DSML｜tool_calls>
//   <｜DSML｜invoke name="TOOL">
//   <｜DSML｜parameter name="NAME" string="true|false">VALUE</｜DSML｜parameter>
//   </｜DSML｜invoke>
//   </｜DSML｜tool_calls>
//
// `｜` is U+FF5C FULLWIDTH VERTICAL LINE, not ASCII `|`. `string="true"` means
// VALUE is a raw string; `string="false"` means VALUE is JSON. An envelope may
// carry several `invoke` elements. An ASCII `<|tool_call|>` variant also exists.
//
// This module is sans-IO: it turns text into parsed calls and never touches the
// network, so it is unit-testable without a live endpoint.

use mikmik_core::types::ContentBlock;
use serde_json::{Map, Value};
use tracing::warn;

/// Fullwidth vertical line (U+FF5C) that delimits every DSML tag name.
const BAR: &str = "\u{FF5C}";

/// One resolved `<｜DSML｜invoke>` element.
#[derive(Debug, Clone, PartialEq)]
pub struct DsmlCall {
    /// The tool name from `name="..."`.
    pub name: String,
    /// The assembled parameters as a JSON object.
    pub input: Value,
}

/// Why an envelope could not be parsed.
#[derive(Debug, Clone, PartialEq)]
pub enum DsmlError {
    /// The text carried no `invoke` element at all.
    NoInvoke,
    /// An `invoke` element carried no `name="..."` attribute.
    MissingName,
}

impl std::fmt::Display for DsmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DsmlError::NoInvoke => write!(f, "DSML envelope carried no invoke element"),
            DsmlError::MissingName => write!(f, "DSML invoke element carried no name attribute"),
        }
    }
}

/// How a chunk of streamed text splits into output, envelopes and a held tail.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scan {
    /// Text that is safe to forward to the transcript now.
    pub emit: String,
    /// Complete, closed envelopes ready for [`parse_envelope`].
    pub envelopes: Vec<String>,
    /// Tail that may be the start of an envelope; feed it back in next time.
    pub hold: String,
}

/// The opening markers that start an envelope.
///
/// The ASCII form is far likelier to occur in ordinary prose or a code block, so
/// it is only honoured at the start of a line (see [`marker_at`]).
const FULLWIDTH_OPEN: &str = "<\u{FF5C}DSML\u{FF5C}tool_calls>";
const ASCII_OPEN: &str = "<|tool_call|>";

/// The closing marker that matches each opening marker.
fn closing_for(open: &str) -> &'static str {
    if open == ASCII_OPEN {
        "</|tool_call|>"
    } else {
        "</\u{FF5C}DSML\u{FF5C}tool_calls>"
    }
}

/// Whether an opening marker starting at byte offset `at` is a real envelope.
///
/// The fullwidth form is specific enough to trust anywhere. The ASCII form is
/// accepted only at the very start of the buffer or right after a newline, so
/// plain text that merely mentions `<|tool_call|>` still reaches the user.
fn marker_at(text: &str, at: usize, marker: &str) -> bool {
    if marker != ASCII_OPEN {
        return true;
    }
    at == 0 || text[..at].ends_with('\n')
}

/// Find the earliest opening marker in `text`, returning its offset and which
/// marker it was.
fn find_open(text: &str) -> Option<(usize, &'static str)> {
    let mut best: Option<(usize, &'static str)> = None;
    for marker in [FULLWIDTH_OPEN, ASCII_OPEN] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(marker) {
            let at = from + rel;
            if marker_at(text, at, marker) {
                if best.is_none_or(|(b, _)| at < b) {
                    best = Some((at, marker));
                }
                break;
            }
            // Skip this occurrence and keep looking: an ASCII marker mid-line is
            // ordinary text, but a later one may still start a real envelope.
            from = at + marker.len();
        }
    }
    best
}

/// The length of the longest suffix of `text` that is a proper prefix of any
/// opening marker.
///
/// This is what must be held back between chunks: an envelope split across an
/// SSE boundary would otherwise have its first half forwarded to the transcript
/// as ordinary text. Offsets walk `char_indices`, because `｜` is three bytes and
/// slicing on a byte index would split it and panic.
fn partial_open_suffix_len(text: &str) -> usize {
    for (idx, _) in text.char_indices() {
        let tail = &text[idx..];
        if tail.is_empty() {
            break;
        }
        for marker in [FULLWIDTH_OPEN, ASCII_OPEN] {
            // A full marker is not a partial; `find_open` handles that case.
            if tail.len() < marker.len() && marker.starts_with(tail) {
                return text.len() - idx;
            }
        }
    }
    0
}

/// Split streamed text into what may be forwarded, what is a complete envelope,
/// and what must be held until the next chunk arrives.
///
/// Feed the held tail back in front of the next chunk. Text that merely contains
/// an opening marker's prefix is held only until the next chunk proves it is not
/// an envelope, so the delay is bounded by one marker's length.
pub fn scan(text: &str) -> Scan {
    let mut out = Scan::default();
    let mut rest = text;

    while let Some((at, open)) = find_open(rest) {
        let close = closing_for(open);
        let after_open = at + open.len();
        let Some(rel_close) = rest[after_open..].find(close) else {
            // Opening marker seen but no closing one yet: forward everything
            // before it and hold the rest for the next chunk.
            out.emit.push_str(&rest[..at]);
            out.hold.push_str(&rest[at..]);
            return out;
        };
        let end = after_open + rel_close + close.len();
        out.emit.push_str(&rest[..at]);
        out.envelopes.push(rest[at..end].to_string());
        rest = &rest[end..];
    }

    // No further envelope starts here. Hold back only a tail that could be the
    // beginning of one.
    let hold_len = partial_open_suffix_len(rest);
    let split = rest.len() - hold_len;
    out.emit.push_str(&rest[..split]);
    out.hold.push_str(&rest[split..]);
    out
}

/// Read the value of `attr="..."` from a tag's attribute text.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let start = tag.find(&needle)? + needle.len();
    let rel_end = tag[start..].find('"')?;
    Some(tag[start..start + rel_end].to_string())
}

/// Parse the parameters of one `invoke` body into a JSON object.
fn parse_parameters(body: &str) -> Value {
    let open_prefix = format!("<{BAR}DSML{BAR}parameter");
    let close_tag = format!("</{BAR}DSML{BAR}parameter>");
    let mut params = Map::new();
    let mut rest = body;

    while let Some(at) = rest.find(&open_prefix) {
        let after_prefix = at + open_prefix.len();
        // The attribute text runs to the tag's own closing `>`.
        let Some(rel_gt) = rest[after_prefix..].find('>') else {
            break;
        };
        let attrs = &rest[after_prefix..after_prefix + rel_gt];
        let value_start = after_prefix + rel_gt + 1;
        let Some(rel_close) = rest[value_start..].find(&close_tag) else {
            break;
        };
        let raw_value = &rest[value_start..value_start + rel_close];

        if let Some(name) = attr_value(attrs, "name") {
            let is_raw_string = attr_value(attrs, "string").as_deref() == Some("true");
            let value = if is_raw_string {
                Value::String(raw_value.to_string())
            } else {
                match serde_json::from_str::<Value>(raw_value.trim()) {
                    Ok(v) => v,
                    Err(e) => {
                        // Keep the text rather than dropping the parameter: a
                        // tool can answer a bad value with an error the model
                        // reads, but a missing parameter says nothing.
                        warn!(
                            parameter = %name,
                            error = %e,
                            "DSML parameter declared string=\"false\" but is not valid JSON; keeping raw text"
                        );
                        Value::String(raw_value.to_string())
                    }
                }
            };
            params.insert(name, value);
        }

        rest = &rest[value_start + rel_close + close_tag.len()..];
    }

    Value::Object(params)
}

/// Parse one complete envelope into the calls it carries.
///
/// An envelope may hold several `invoke` elements; each becomes one [`DsmlCall`].
pub fn parse_envelope(envelope: &str) -> Result<Vec<DsmlCall>, DsmlError> {
    let invoke_prefix = format!("<{BAR}DSML{BAR}invoke");
    let invoke_close = format!("</{BAR}DSML{BAR}invoke>");
    let mut calls = Vec::new();
    let mut rest = envelope;

    while let Some(at) = rest.find(&invoke_prefix) {
        let after_prefix = at + invoke_prefix.len();
        let Some(rel_gt) = rest[after_prefix..].find('>') else {
            break;
        };
        let attrs = &rest[after_prefix..after_prefix + rel_gt];
        let body_start = after_prefix + rel_gt + 1;
        // A trailing `invoke` with no closing tag still carries usable
        // parameters, so parse to the end of the envelope in that case.
        let (body, next) = match rest[body_start..].find(&invoke_close) {
            Some(rel_close) => (
                &rest[body_start..body_start + rel_close],
                body_start + rel_close + invoke_close.len(),
            ),
            None => (&rest[body_start..], rest.len()),
        };

        let name = attr_value(attrs, "name").ok_or(DsmlError::MissingName)?;
        calls.push(DsmlCall {
            name,
            input: parse_parameters(body),
        });
        rest = &rest[next..];
    }

    if calls.is_empty() {
        return Err(DsmlError::NoInvoke);
    }
    Ok(calls)
}

/// Lift DSML envelopes out of a non-streaming response's content blocks.
///
/// The streaming path has its own guard in `OpenAiChatDecoder`; this is the same
/// repair for a whole response that arrived at once. Each text block keeps only
/// its prose, and every envelope it carried becomes a `ToolUse` block, so the
/// turn loop dispatches the tool instead of showing the user raw markup.
///
/// A text block that becomes empty is dropped. Content is returned unchanged
/// when no envelope is present, which is every non-DeepSeek response.
pub fn lift_envelopes(content: Vec<ContentBlock>) -> Vec<ContentBlock> {
    if !content.iter().any(|b| match b {
        ContentBlock::Text { text } => !scan(text).envelopes.is_empty(),
        _ => false,
    }) {
        return content;
    }

    let mut out = Vec::with_capacity(content.len());
    let mut calls_seen = 0usize;

    for block in content {
        let ContentBlock::Text { text } = &block else {
            out.push(block);
            continue;
        };
        let parsed = scan(text);
        if parsed.envelopes.is_empty() {
            out.push(block);
            continue;
        }

        // Whatever the scan held back was never an envelope, since the whole
        // response is present; it is ordinary text.
        let prose = format!("{}{}", parsed.emit, parsed.hold);
        if !prose.is_empty() {
            out.push(ContentBlock::Text { text: prose });
        }
        for envelope in &parsed.envelopes {
            match parse_envelope(envelope) {
                Ok(calls) => {
                    for call in calls {
                        out.push(ContentBlock::ToolUse {
                            id: format!("call_dsml_{}", calls_seen),
                            name: call.name,
                            input: call.input,
                            thought_signature: None,
                        });
                        calls_seen += 1;
                    }
                }
                Err(e) => {
                    warn!("Failed to parse DSML envelope: {}; keeping it as text", e);
                    out.push(ContentBlock::Text {
                        text: envelope.clone(),
                    });
                }
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One parameter as `(name, string_attr, value)`.
    type Param<'a> = (&'a str, &'a str, &'a str);
    /// One call as `(tool_name, parameters)`.
    type Call<'a> = (&'a str, &'a [Param<'a>]);

    /// Build a fullwidth envelope from the calls it should carry.
    fn envelope(calls: &[Call<'_>]) -> String {
        let mut s = format!("<{BAR}DSML{BAR}tool_calls>");
        for (tool, params) in calls {
            s.push_str(&format!("<{BAR}DSML{BAR}invoke name=\"{tool}\">"));
            for (name, is_string, value) in *params {
                s.push_str(&format!(
                    "<{BAR}DSML{BAR}parameter name=\"{name}\" string=\"{is_string}\">{value}</{BAR}DSML{BAR}parameter>"
                ));
            }
            s.push_str(&format!("</{BAR}DSML{BAR}invoke>"));
        }
        s.push_str(&format!("</{BAR}DSML{BAR}tool_calls>"));
        s
    }

    #[test]
    fn a_single_invoke_parses_into_one_call() {
        let env = envelope(&[("get_weather", &[("location", "true", "Paris")])]);
        let calls = parse_envelope(&env).expect("parsed");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].input, json!({ "location": "Paris" }));
    }

    #[test]
    fn several_invokes_in_one_envelope_each_become_a_call() {
        let env = envelope(&[
            ("get_weather", &[("location", "true", "Paris")]),
            ("get_time", &[("tz", "true", "UTC")]),
        ]);
        let calls = parse_envelope(&env).expect("parsed");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[1].name, "get_time");
        assert_eq!(calls[1].input, json!({ "tz": "UTC" }));
    }

    #[test]
    fn the_string_attribute_selects_raw_text_or_json() {
        // string="true" keeps the text verbatim, even when it looks like JSON.
        let raw = envelope(&[("t", &[("v", "true", "[1, 2]")])]);
        assert_eq!(
            parse_envelope(&raw).expect("parsed")[0].input,
            json!({ "v": "[1, 2]" })
        );
        // string="false" decodes the value as JSON.
        let decoded = envelope(&[("t", &[("v", "false", "[1, 2]")])]);
        assert_eq!(
            parse_envelope(&decoded).expect("parsed")[0].input,
            json!({ "v": [1, 2] })
        );
    }

    #[test]
    fn a_json_parameter_that_does_not_parse_keeps_its_raw_text() {
        // Dropping the parameter would leave the tool with no way to report the
        // problem, so the text survives as a string.
        let env = envelope(&[("t", &[("v", "false", "{not json")])]);
        assert_eq!(
            parse_envelope(&env).expect("parsed")[0].input,
            json!({ "v": "{not json" })
        );
    }

    #[test]
    fn text_without_an_envelope_passes_straight_through() {
        let s = scan("just some prose");
        assert_eq!(s.emit, "just some prose");
        assert!(s.envelopes.is_empty());
        assert!(s.hold.is_empty());
    }

    #[test]
    fn narration_around_an_envelope_survives() {
        let env = envelope(&[("t", &[("v", "true", "x")])]);
        let s = scan(&format!("before {env} after"));
        assert_eq!(s.emit, "before  after");
        assert_eq!(s.envelopes.len(), 1);
        assert!(s.hold.is_empty());
    }

    #[test]
    fn an_envelope_split_across_chunks_reassembles() {
        let env = envelope(&[("get_weather", &[("location", "true", "Paris")])]);
        // Split at every byte boundary that lands on a char boundary; each split
        // must reassemble to exactly one envelope and must not panic.
        for (idx, _) in env.char_indices() {
            let (head, tail) = env.split_at(idx);
            let first = scan(head);
            let carried = format!("{}{}", first.hold, tail);
            let second = scan(&carried);
            let envelopes: Vec<String> = first
                .envelopes
                .into_iter()
                .chain(second.envelopes)
                .collect();
            assert_eq!(envelopes.len(), 1, "split at {idx} lost the envelope");
            assert_eq!(envelopes[0], env, "split at {idx} corrupted the envelope");
            // No markup may escape to the transcript.
            assert!(
                first.emit.is_empty(),
                "split at {idx} leaked: {}",
                first.emit
            );
            assert!(
                second.emit.is_empty(),
                "split at {idx} leaked: {}",
                second.emit
            );
        }
    }

    #[test]
    fn a_partial_opening_marker_is_held_not_forwarded() {
        // The first half of the fullwidth marker must not reach the transcript.
        let partial = &FULLWIDTH_OPEN[..FULLWIDTH_OPEN.len() - 3];
        let s = scan(&format!("text {partial}"));
        assert_eq!(s.emit, "text ");
        assert_eq!(s.hold, partial);
    }

    #[test]
    fn plain_text_that_merely_mentions_a_marker_still_reaches_the_user() {
        // An ASCII marker mid-line is prose, not protocol.
        let s = scan("write <|tool_call|> to call a tool");
        assert_eq!(s.emit, "write <|tool_call|> to call a tool");
        assert!(s.envelopes.is_empty());
        assert!(s.hold.is_empty());
    }

    #[test]
    fn an_ascii_envelope_is_recognised_at_the_start_of_a_line() {
        let body = format!("<{BAR}DSML{BAR}invoke name=\"t\"></{BAR}DSML{BAR}invoke>");
        let env = format!("<|tool_call|>{body}</|tool_call|>");
        let s = scan(&format!("intro\n{env}"));
        assert_eq!(s.emit, "intro\n");
        assert_eq!(s.envelopes.len(), 1);
        assert_eq!(
            parse_envelope(&s.envelopes[0]).expect("parsed")[0].name,
            "t"
        );
    }

    #[test]
    fn an_unclosed_envelope_is_held_rather_than_emitted() {
        let open = format!("<{BAR}DSML{BAR}tool_calls><{BAR}DSML{BAR}invoke name=\"t\">");
        let s = scan(&format!("before {open}"));
        assert_eq!(s.emit, "before ");
        assert_eq!(s.hold, open);
        assert!(s.envelopes.is_empty());
    }

    #[test]
    fn an_envelope_without_an_invoke_is_an_error() {
        let env = format!("<{BAR}DSML{BAR}tool_calls></{BAR}DSML{BAR}tool_calls>");
        assert_eq!(parse_envelope(&env), Err(DsmlError::NoInvoke));
    }

    // ---- lift_envelopes (non-streaming path) --------------------------------

    #[test]
    fn a_non_streaming_envelope_becomes_a_tool_use_block() {
        let env = envelope(&[("get_weather", &[("location", "true", "Paris")])]);
        let lifted = lift_envelopes(vec![ContentBlock::Text {
            text: format!("Checking. {env} Done."),
        }]);

        assert_eq!(lifted.len(), 2, "got {lifted:?}");
        match &lifted[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Checking.  Done."),
            other => panic!("expected the prose to survive, got {other:?}"),
        }
        match &lifted[1] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "get_weather");
                assert_eq!(*input, json!({ "location": "Paris" }));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn content_without_an_envelope_is_returned_unchanged() {
        let lifted = lift_envelopes(vec![ContentBlock::Text {
            text: "just prose".to_string(),
        }]);
        assert_eq!(lifted.len(), 1);
        match &lifted[0] {
            ContentBlock::Text { text } => assert_eq!(text, "just prose"),
            other => panic!("expected the text block untouched, got {other:?}"),
        }
    }
}
