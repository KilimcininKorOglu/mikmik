// protocol::openai_chat — the OpenAI Chat Completions streaming wire format.
//
// `OpenAiChatDecoder` is the sans-IO decoder for the `chat/completions` SSE
// stream shared by OpenAI and the ~35 OpenAI-compatible vendors behind
// `providers/openai_compat.rs`. The logic here is a verbatim extraction of that
// adapter's former inline stream loop (#228): identical event ordering, tool-call
// block indexing, reasoning/thinking handling and finish/usage semantics — only
// now it is a reusable, unit-testable state machine instead of being welded into
// an `async_stream::stream!` block.

use std::collections::HashMap;

use mikmik_core::types::{ContentBlock, UsageInfo};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::protocol::LineStreamDecoder;
use crate::provider_types::StreamEvent;
use crate::providers::openai::OpenAiProvider;

/// Dedicated index for the Thinking content block emitted when a provider
/// streams a `reasoning_content` field (DeepSeek V4, etc.). Chosen to avoid
/// colliding with text (index 0) or tool calls (1 + tc_index).
const THINKING_BLOCK_INDEX: usize = usize::MAX - 100;

/// First content-block index used by a tool call recovered from a DSML envelope.
/// Chosen far above the OpenAI-style arm's `1 + tc_index` range and below
/// [`THINKING_BLOCK_INDEX`], so a stream carrying both kinds cannot collide.
const DSML_BLOCK_INDEX_BASE: usize = 1 << 20;

/// Extract Gemini's opaque thought signature from an OpenAI-compatible tool call.
///
/// Gemini served over `chat/completions` carries the signature as
/// `extra_content.google.thought_signature` (Vertex uses the `vertex`
/// namespace). The bare string is stored on `ContentBlock::ToolUse`, matching
/// the native GenAI path, so the next turn's replay can re-emit it; without it
/// a Gemini 3.x tool-call continuation fails with a missing-signature error.
pub(crate) fn gemini_thought_signature(tool_call: &Value) -> Option<String> {
    let extra = tool_call.get("extra_content")?;
    ["google", "vertex"].into_iter().find_map(|namespace| {
        extra
            .get(namespace)
            .and_then(|ns| ns.get("thought_signature"))
            .and_then(|v| v.as_str())
            .filter(|sig| !sig.is_empty())
            .map(str::to_string)
    })
}

/// Streaming decoder for the OpenAI Chat Completions SSE format.
///
/// Construct with [`OpenAiChatDecoder::new`], feed each SSE line via
/// [`feed_line`](Self::feed_line), and after the byte stream ends call
/// [`finish`](Self::finish) to flush a trailing `MessageStop`.
pub struct OpenAiChatDecoder {
    /// Provider-specific reasoning field name (e.g. DeepSeek's
    /// `reasoning_content`), checked before the common fallbacks.
    reasoning_field: Option<String>,
    message_started: bool,
    message_id: String,
    model_name: String,
    thinking_open: bool,
    /// Keyed by content-block index → (tool_call_id, name, accumulated_args).
    tool_call_buffers: HashMap<usize, (String, String, String)>,
    /// Text held back because it may be the start of a DSML tool-call envelope
    /// split across SSE chunks. See [`crate::protocol::dsml`].
    dsml_buf: String,
    /// How many DSML calls this stream has produced, used to mint unique tool
    /// call ids and block indices.
    dsml_calls_seen: usize,
}

impl OpenAiChatDecoder {
    pub fn new(reasoning_field: Option<String>) -> Self {
        Self {
            reasoning_field,
            message_started: false,
            message_id: String::from("unknown"),
            model_name: String::new(),
            thinking_open: false,
            tool_call_buffers: HashMap::new(),
            dsml_buf: String::new(),
            dsml_calls_seen: 0,
        }
    }

    /// Emit the events for one DSML envelope's calls.
    ///
    /// A DSML call arrives fully assembled, so it maps onto the same pair of
    /// events an OpenAI-style tool call produces: a `ContentBlockStart` opening
    /// the block and a single `InputJsonDelta` carrying the whole argument JSON.
    /// Registering it in `tool_call_buffers` is what makes the `finish_reason`
    /// arm close the block like any other.
    fn emit_dsml_envelope(&mut self, envelope: &str, out: &mut Vec<StreamEvent>) {
        let calls = match crate::protocol::dsml::parse_envelope(envelope) {
            Ok(calls) => calls,
            Err(e) => {
                // The envelope is closed but unusable. Surfacing it as text is
                // what the user would have seen before this parser existed, and
                // it beats dropping the model's turn silently.
                warn!("Failed to parse DSML envelope: {}; forwarding as text", e);
                out.push(StreamEvent::TextDelta {
                    index: 0,
                    text: envelope.to_string(),
                });
                return;
            }
        };

        // Close any open thinking block first, for the same ordering guarantee
        // the OpenAI tool-call arm keeps.
        if self.thinking_open {
            out.push(StreamEvent::ContentBlockStop {
                index: THINKING_BLOCK_INDEX,
            });
            self.thinking_open = false;
        }

        for call in calls {
            // Start above every index the OpenAI-style arm may claim (1 + its
            // own tc_index), so the two schemes cannot collide in one stream.
            let block_index = DSML_BLOCK_INDEX_BASE + self.dsml_calls_seen;
            let id = format!("call_dsml_{}", self.dsml_calls_seen);
            self.dsml_calls_seen += 1;

            self.tool_call_buffers
                .insert(block_index, (id.clone(), call.name.clone(), String::new()));
            out.push(StreamEvent::ContentBlockStart {
                index: block_index,
                content_block: ContentBlock::ToolUse {
                    id,
                    name: call.name,
                    input: json!({}),
                    thought_signature: None,
                },
            });
            let args = call.input.to_string();
            if let Some((_, _, buf)) = self.tool_call_buffers.get_mut(&block_index) {
                buf.push_str(&args);
            }
            out.push(StreamEvent::InputJsonDelta {
                index: block_index,
                partial_json: args,
            });
        }
    }

    /// Flush any text held back mid-envelope as ordinary text.
    ///
    /// A stream that ends inside an envelope leaves markup in the buffer. It is
    /// forwarded rather than dropped, because swallowing it would lose the
    /// model's output with nothing to show for it.
    fn flush_dsml_buf(&mut self, out: &mut Vec<StreamEvent>) {
        if self.dsml_buf.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.dsml_buf);
        warn!(
            bytes = held.len(),
            "Stream ended inside a DSML envelope; forwarding the partial markup as text"
        );
        out.push(StreamEvent::TextDelta {
            index: 0,
            text: held,
        });
    }

    /// Feed one SSE line. See [`LineStreamDecoder::feed_line`].
    pub fn feed_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) -> bool {
        let line = line.trim_end_matches('\r').trim();

        if line.is_empty() || line.starts_with(':') {
            return false;
        }

        let data = match line.strip_prefix("data:") {
            Some(rest) => rest.trim(),
            None => return false,
        };

        if data == "[DONE]" {
            // A route that ends the stream without a finish_reason can still
            // leave envelope text held back; it belongs to the user's turn.
            self.flush_dsml_buf(out);
            out.push(StreamEvent::MessageStop);
            return true;
        }

        let chunk_json: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to parse SSE chunk: {}: {}", e, data);
                return false;
            }
        };

        if !self.message_started {
            if let Some(id) = chunk_json.get("id").and_then(|v| v.as_str()) {
                self.message_id = id.to_string();
            }
            if let Some(m) = chunk_json.get("model").and_then(|v| v.as_str()) {
                self.model_name = m.to_string();
            }
            out.push(StreamEvent::MessageStart {
                id: self.message_id.clone(),
                model: self.model_name.clone(),
                usage: UsageInfo::default(),
            });
            out.push(StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text {
                    text: String::new(),
                },
            });
            self.message_started = true;
        }

        let choices = match chunk_json.get("choices").and_then(|c| c.as_array()) {
            Some(c) => c,
            None => {
                if let Some(usage_val) = chunk_json.get("usage") {
                    let usage = OpenAiProvider::parse_usage_pub(Some(usage_val));
                    out.push(StreamEvent::MessageDelta {
                        stop_reason: None,
                        usage: Some(usage),
                    });
                }
                return false;
            }
        };

        let choice = match choices.first() {
            Some(c) => c,
            None => return false,
        };

        let delta = match choice.get("delta") {
            Some(d) => d,
            None => return false,
        };

        // Reasoning / thinking extraction.
        // Check the provider-specific field first (e.g. DeepSeek's
        // "reasoning_content"), then fall back to common field names used by
        // other providers (Copilot "reasoning_text", generic "reasoning", etc.).
        // This allows reasoning traces to show for any provider that emits them
        // without needing explicit per-provider configuration.
        {
            const COMMON_REASONING_FIELDS: &[&str] = &[
                "reasoning_content", // DeepSeek
                "reasoning_text",    // GitHub Copilot
                "reasoning",         // Generic / future
            ];
            let fields_to_check: Vec<&str> = if let Some(ref f) = self.reasoning_field {
                // Provider-specific field first, then common ones.
                let mut v = vec![f.as_str()];
                for common in COMMON_REASONING_FIELDS {
                    if *common != f.as_str() {
                        v.push(common);
                    }
                }
                v
            } else {
                COMMON_REASONING_FIELDS.to_vec()
            };
            for field in &fields_to_check {
                if let Some(reasoning) = delta.get(*field).and_then(|v| v.as_str()) {
                    if !reasoning.is_empty() {
                        // Open a dedicated Thinking block on first reasoning
                        // delta so the accumulator has a partial to append into
                        // (see StreamAccumulator::on_event). Without this start
                        // event the reasoning deltas would be dropped and the
                        // completed assistant message would not carry any
                        // ContentBlock::Thinking — which is what DeepSeek V4
                        // thinking mode requires the client to echo back on
                        // subsequent turns.
                        if !self.thinking_open {
                            out.push(StreamEvent::ContentBlockStart {
                                index: THINKING_BLOCK_INDEX,
                                content_block: ContentBlock::Thinking {
                                    thinking: String::new(),
                                    signature: String::new(),
                                },
                            });
                            self.thinking_open = true;
                        }
                        out.push(StreamEvent::ReasoningDelta {
                            index: THINKING_BLOCK_INDEX,
                            reasoning: reasoning.to_string(),
                        });
                        break;
                    }
                }
            }
        }

        // Text content delta.
        //
        // DeepSeek V4-family routes deliver tool calls as DSML envelopes inside
        // this field rather than as `tool_calls`, so the text is scanned before
        // it is forwarded: envelopes become tool calls, a fragment that may be
        // the start of one is held for the next chunk, and everything else goes
        // to the transcript unchanged (upstream issue #395).
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                self.dsml_buf.push_str(content);
                let scan = crate::protocol::dsml::scan(&self.dsml_buf);
                self.dsml_buf = scan.hold;

                if !scan.emit.is_empty() {
                    // Close any open thinking block before visible text starts
                    // streaming, so the blocks land in order in the final message:
                    // [Thinking, Text, ToolUse...].
                    if self.thinking_open {
                        out.push(StreamEvent::ContentBlockStop {
                            index: THINKING_BLOCK_INDEX,
                        });
                        self.thinking_open = false;
                    }
                    out.push(StreamEvent::TextDelta {
                        index: 0,
                        text: scan.emit,
                    });
                }
                for envelope in scan.envelopes {
                    self.emit_dsml_envelope(&envelope, out);
                }
            }
        }

        // Tool call deltas.
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            // Close any open thinking block before tool calls start (same
            // ordering guarantee as for text above).
            if self.thinking_open {
                out.push(StreamEvent::ContentBlockStop {
                    index: THINKING_BLOCK_INDEX,
                });
                self.thinking_open = false;
            }
            for tc in tool_calls {
                let tc_index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if let Some(tc_id) = tc.get("id").and_then(|v| v.as_str()) {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let block_index = 1 + tc_index;
                    self.tool_call_buffers.insert(
                        block_index,
                        (tc_id.to_string(), name.clone(), String::new()),
                    );
                    out.push(StreamEvent::ContentBlockStart {
                        index: block_index,
                        content_block: ContentBlock::ToolUse {
                            id: tc_id.to_string(),
                            name,
                            input: json!({}),
                            thought_signature: gemini_thought_signature(tc),
                        },
                    });
                }
                if let Some(args_frag) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                {
                    if !args_frag.is_empty() {
                        let block_index = 1 + tc_index;
                        if let Some((_, _, buf)) = self.tool_call_buffers.get_mut(&block_index) {
                            buf.push_str(args_frag);
                        }
                        out.push(StreamEvent::InputJsonDelta {
                            index: block_index,
                            partial_json: args_frag.to_string(),
                        });
                    }
                }
            }
        }

        // finish_reason.
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            if !finish_reason.is_empty() && finish_reason != "null" {
                // Anything still held mid-envelope belongs to the text block, so
                // it must go out before that block is closed below.
                self.flush_dsml_buf(out);
                // Flush any still-open thinking block first so it is finalized
                // into the assistant message.
                if self.thinking_open {
                    out.push(StreamEvent::ContentBlockStop {
                        index: THINKING_BLOCK_INDEX,
                    });
                    self.thinking_open = false;
                }
                out.push(StreamEvent::ContentBlockStop { index: 0 });
                let mut tc_indices: Vec<usize> = self.tool_call_buffers.keys().cloned().collect();
                tc_indices.sort();
                for idx in tc_indices {
                    out.push(StreamEvent::ContentBlockStop { index: idx });
                }

                let stop_reason = OpenAiProvider::map_finish_reason_pub(finish_reason);
                let usage_val = chunk_json.get("usage");
                let usage = usage_val.map(|u| OpenAiProvider::parse_usage_pub(Some(u)));

                out.push(StreamEvent::MessageDelta {
                    stop_reason: Some(stop_reason),
                    usage,
                });
            }
        }

        false
    }

    /// Flush a trailing `MessageStop` if the stream produced any content but
    /// ended without an explicit `[DONE]` sentinel.
    pub fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        // A stream that ends without a finish_reason may still hold envelope
        // text; forward it rather than losing it.
        self.flush_dsml_buf(out);
        if self.message_started {
            out.push(StreamEvent::MessageStop);
        }
    }
}

impl LineStreamDecoder for OpenAiChatDecoder {
    fn feed_line(&mut self, line: &str, out: &mut Vec<StreamEvent>) -> bool {
        OpenAiChatDecoder::feed_line(self, line, out)
    }

    fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        OpenAiChatDecoder::finish(self, out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a slice of lines and return every event produced (excluding the
    /// stop signal, which is asserted separately where it matters).
    fn drain(decoder: &mut OpenAiChatDecoder, lines: &[&str]) -> (Vec<StreamEvent>, bool) {
        let mut out = Vec::new();
        let mut done = false;
        for l in lines {
            if decoder.feed_line(l, &mut out) {
                done = true;
                break;
            }
        }
        (out, done)
    }

    #[test]
    fn text_stream_emits_start_delta_finish() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, done) = drain(
            &mut d,
            &[
                r#"data: {"id":"chatcmpl-1","model":"gpt-x","choices":[{"delta":{"content":"Hello"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":" world"}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );
        assert!(!done, "no [DONE] fed yet");

        // MessageStart carries the id/model; a text block opens at index 0.
        assert!(matches!(
            &events[0],
            StreamEvent::MessageStart { id, model, .. } if id == "chatcmpl-1" && model == "gpt-x"
        ));
        assert!(matches!(
            &events[1],
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text { .. }
            }
        ));

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { index: 0, text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");

        // finish_reason closes block 0 and emits a MessageDelta.
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 })));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::MessageDelta {
                stop_reason: Some(_),
                ..
            }
        )));

        // finish() flushes MessageStop since content was produced.
        let mut tail = Vec::new();
        d.finish(&mut tail);
        assert!(matches!(tail.as_slice(), [StreamEvent::MessageStop]));
    }

    #[test]
    fn done_sentinel_stops_and_emits_message_stop() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"content":"hi"}}]}"#,
                "data: [DONE]",
            ],
        );
        assert!(done, "[DONE] must stop the stream");
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
    }

    #[test]
    fn tool_call_arguments_assemble_across_lines() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, _done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":""}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Paris\"}"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );

        // The tool block opens at index 1 (1 + tc_index) with id + name.
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::ToolUse { id, name, .. }
            } if id == "call_1" && name == "get_weather"
        )));

        // The streamed argument fragments concatenate into valid JSON.
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::InputJsonDelta {
                    index: 1,
                    partial_json,
                } => Some(partial_json.clone()),
                _ => None,
            })
            .collect();
        let parsed: Value =
            serde_json::from_str(&args).expect("assembled tool args must be valid JSON");
        assert_eq!(parsed["city"], "Paris");

        // finish closes text block 0 and the tool block 1.
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockStop { index: 1 })));
    }

    /// The `thought_signature` carried by a `ContentBlockStart` tool block, if any.
    fn opened_signature(events: &[StreamEvent]) -> Option<String> {
        events.iter().find_map(|e| match e {
            StreamEvent::ContentBlockStart {
                content_block:
                    ContentBlock::ToolUse {
                        thought_signature, ..
                    },
                ..
            } => thought_signature.clone(),
            _ => None,
        })
    }

    #[test]
    fn a_gemini_tool_call_keeps_its_google_thought_signature() {
        // Gemini over chat/completions rides the signature on the same delta as
        // the tool-call id, under extra_content.google.
        let mut d = OpenAiChatDecoder::new(None);
        let (events, _done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""},"extra_content":{"google":{"thought_signature":"sig-google"}}}]}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert_eq!(opened_signature(&events).as_deref(), Some("sig-google"));
    }

    #[test]
    fn a_gemini_tool_call_keeps_its_vertex_thought_signature() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, _done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""},"extra_content":{"vertex":{"thought_signature":"sig-vertex"}}}]}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert_eq!(opened_signature(&events).as_deref(), Some("sig-vertex"));
    }

    #[test]
    fn a_plain_tool_call_has_no_thought_signature() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, _done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":""}}]}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert_eq!(opened_signature(&events), None);
    }

    #[test]
    fn reasoning_opens_thinking_block_then_text_closes_it() {
        let mut d = OpenAiChatDecoder::new(None);
        let (events, _done) = drain(
            &mut d,
            &[
                r#"data: {"id":"c","model":"m","choices":[{"delta":{"reasoning_content":"pondering"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"answer"}}]}"#,
            ],
        );

        // A Thinking block opens at THINKING_BLOCK_INDEX and receives the delta.
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::Thinking { .. }
            } if *index == THINKING_BLOCK_INDEX
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ReasoningDelta { index, reasoning } if *index == THINKING_BLOCK_INDEX && reasoning == "pondering"
        )));
        // Visible text closes the thinking block first, preserving block order.
        let stop_pos = events
            .iter()
            .position(|e| matches!(e, StreamEvent::ContentBlockStop { index } if *index == THINKING_BLOCK_INDEX));
        let text_pos = events
            .iter()
            .position(|e| matches!(e, StreamEvent::TextDelta { index: 0, .. }));
        assert!(stop_pos.is_some() && text_pos.is_some());
        assert!(stop_pos < text_pos, "thinking block must close before text");
    }

    /// A provider-specific reasoning field (DeepSeek-style) is honoured.
    #[test]
    fn custom_reasoning_field_is_checked_first() {
        let mut d = OpenAiChatDecoder::new(Some("thinking_blob".to_string()));
        let (events, _done) = drain(
            &mut d,
            &[r#"data: {"id":"c","model":"m","choices":[{"delta":{"thinking_blob":"hmm"}}]}"#],
        );
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ReasoningDelta { reasoning, .. } if reasoning == "hmm"
        )));
    }

    /// A usage-only chunk with no `choices` yields a usage MessageDelta and does
    /// not terminate the stream.
    #[test]
    fn usage_only_chunk_yields_message_delta() {
        let mut d = OpenAiChatDecoder::new(None);
        // Prime message_started so the usage-only branch is reached the same way
        // it is in a real stream.
        let mut out = Vec::new();
        d.feed_line(
            r#"data: {"id":"c","model":"m","choices":[{"delta":{"content":"x"}}]}"#,
            &mut out,
        );
        out.clear();
        let stop = d.feed_line(
            r#"data: {"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            &mut out,
        );
        assert!(!stop);
        assert!(matches!(
            out.as_slice(),
            [StreamEvent::MessageDelta {
                stop_reason: None,
                usage: Some(_)
            }]
        ));
    }

    // ---- DSML envelopes (issue #395) ---------------------------------------

    const BAR: &str = "\u{FF5C}";

    /// A complete fullwidth DSML envelope carrying one call.
    fn dsml_envelope() -> String {
        format!(
            "<{BAR}DSML{BAR}tool_calls><{BAR}DSML{BAR}invoke name=\"get_weather\">\
             <{BAR}DSML{BAR}parameter name=\"location\" string=\"true\">Paris</{BAR}DSML{BAR}parameter>\
             </{BAR}DSML{BAR}invoke></{BAR}DSML{BAR}tool_calls>"
        )
    }

    /// Wrap `text` as one `delta.content` SSE line.
    fn content_line(text: &str) -> String {
        format!(
            "data: {}",
            json!({
                "id": "c",
                "model": "deepseek-v4-pro",
                "choices": [{ "delta": { "content": text } }]
            })
        )
    }

    /// Collect every text fragment the decoder forwarded.
    fn text_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Collect every tool call the decoder opened, as `(name, arguments)`.
    fn tool_calls_of(events: &[StreamEvent]) -> Vec<(String, String)> {
        let mut calls = Vec::new();
        for e in events {
            if let StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::ToolUse { name, .. },
            } = e
            {
                let args = events
                    .iter()
                    .find_map(|ev| match ev {
                        StreamEvent::InputJsonDelta {
                            index: i,
                            partial_json,
                        } if i == index => Some(partial_json.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                calls.push((name.clone(), args));
            }
        }
        calls
    }

    #[test]
    fn a_dsml_envelope_becomes_a_tool_call_and_never_reaches_the_transcript() {
        let mut d = OpenAiChatDecoder::new(None);
        let (out, _) = drain(&mut d, &[&content_line(&dsml_envelope())]);

        let calls = tool_calls_of(&out);
        assert_eq!(calls.len(), 1, "expected one tool call, got {calls:?}");
        assert_eq!(calls[0].0, "get_weather");
        assert_eq!(
            serde_json::from_str::<Value>(&calls[0].1).expect("valid json"),
            json!({ "location": "Paris" })
        );
        assert!(
            !text_of(&out).contains("DSML"),
            "markup leaked to the transcript: {}",
            text_of(&out)
        );
    }

    #[test]
    fn two_invokes_in_one_envelope_open_two_tool_calls() {
        let envelope = format!(
            "<{BAR}DSML{BAR}tool_calls>\
             <{BAR}DSML{BAR}invoke name=\"a\"></{BAR}DSML{BAR}invoke>\
             <{BAR}DSML{BAR}invoke name=\"b\"></{BAR}DSML{BAR}invoke>\
             </{BAR}DSML{BAR}tool_calls>"
        );
        let mut d = OpenAiChatDecoder::new(None);
        let (out, _) = drain(&mut d, &[&content_line(&envelope)]);

        let names: Vec<String> = tool_calls_of(&out).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn an_envelope_split_across_sse_chunks_still_produces_one_tool_call() {
        let envelope = dsml_envelope();
        // Split inside the fullwidth bar's bytes is impossible across a JSON
        // string boundary, so split on char boundaries and check every one.
        for (idx, _) in envelope.char_indices().skip(1) {
            let (head, tail) = envelope.split_at(idx);
            let mut d = OpenAiChatDecoder::new(None);
            let (out, _) = drain(&mut d, &[&content_line(head), &content_line(tail)]);

            let calls = tool_calls_of(&out);
            assert_eq!(calls.len(), 1, "split at {idx} produced {calls:?}");
            assert_eq!(calls[0].0, "get_weather");
            assert!(
                !text_of(&out).contains("DSML"),
                "split at {idx} leaked: {}",
                text_of(&out)
            );
        }
    }

    #[test]
    fn narration_around_an_envelope_reaches_the_transcript() {
        let mut d = OpenAiChatDecoder::new(None);
        let line = content_line(&format!("Let me check. {} Done.", dsml_envelope()));
        let (out, _) = drain(&mut d, &[&line]);

        assert_eq!(text_of(&out), "Let me check.  Done.");
        assert_eq!(tool_calls_of(&out).len(), 1);
    }

    #[test]
    fn a_dsml_tool_call_is_closed_even_when_the_route_finishes_with_stop() {
        // A DSML route does not know a tool was called, so it reports
        // finish_reason "stop"; the block must still be closed.
        let mut d = OpenAiChatDecoder::new(None);
        let (out, _) = drain(
            &mut d,
            &[
                &content_line(&dsml_envelope()),
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );

        let opened: Vec<usize> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlock::ToolUse { .. },
                } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(opened.len(), 1);
        let closed: Vec<usize> = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockStop { index } => Some(*index),
                _ => None,
            })
            .collect();
        assert!(
            closed.contains(&opened[0]),
            "tool block {} was never closed; closed = {closed:?}",
            opened[0]
        );
    }

    #[test]
    fn an_unclosed_envelope_is_forwarded_as_text_rather_than_swallowed() {
        let partial = format!("<{BAR}DSML{BAR}tool_calls><{BAR}DSML{BAR}invoke name=\"t\">");
        let mut d = OpenAiChatDecoder::new(None);
        let mut out = Vec::new();
        d.feed_line(&content_line(&partial), &mut out);
        // Nothing is emitted while the envelope may still close.
        assert_eq!(text_of(&out), "");
        d.finish(&mut out);
        assert_eq!(text_of(&out), partial);
    }

    #[test]
    fn ordinary_text_is_unaffected_by_the_dsml_guard() {
        let mut d = OpenAiChatDecoder::new(None);
        let (out, _) = drain(&mut d, &[&content_line("Hello"), &content_line(" world")]);
        assert_eq!(text_of(&out), "Hello world");
        assert!(tool_calls_of(&out).is_empty());
    }
}
