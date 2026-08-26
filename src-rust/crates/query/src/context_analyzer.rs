//! Context window analysis.
//!
//! Two numbers describe a session's context, and they come from different
//! places. The API reports what it actually counted for the last request, which
//! is exact but covers only the messages that existed then. This module
//! estimates the current message list and splits it by category, which is
//! approximate but current and decomposable.
//!
//! Neither the system prompt nor the tool definitions can be counted here.
//! Nothing records their size, and re-assembling the system prompt outside
//! `build_system_prompt` would drift from what a run sends. So this module
//! reports the conversation, and the caller reports the measured total beside
//! it rather than inventing a number for the difference.

use mikmik_core::types::{ContentBlock, Message, MessageContent, ToolResultContent};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A category of the conversation's token use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextCategory {
    ConversationHistory,
    ToolResults,
    Attachments,
}

impl ContextCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ConversationHistory => "Conversation",
            Self::ToolResults => "Tool results",
            Self::Attachments => "Attachments",
        }
    }
}

/// An estimated token breakdown of the current message list.
#[derive(Debug, Clone, Default)]
pub struct ContextAnalysis {
    pub conversation_history_tokens: u64,
    pub tool_results_tokens: u64,
    pub attachments_tokens: u64,
    /// The three categories added together.
    pub total_tokens: u64,
    /// How much of the total a summariser could plausibly reclaim, 0.0 to 1.0.
    pub compressibility: f64,
}

impl ContextAnalysis {
    pub fn category_tokens(&self, cat: ContextCategory) -> u64 {
        match cat {
            ContextCategory::ConversationHistory => self.conversation_history_tokens,
            ContextCategory::ToolResults => self.tool_results_tokens,
            ContextCategory::Attachments => self.attachments_tokens,
        }
    }

    /// Share of the estimated total this category holds, as a percentage.
    pub fn category_pct(&self, cat: ContextCategory) -> f64 {
        if self.total_tokens == 0 {
            return 0.0;
        }
        (self.category_tokens(cat) as f64 / self.total_tokens as f64) * 100.0
    }
}

/// What to do about a context that is filling up.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionStrategy {
    /// Summarise the whole history.
    FullCompact { expected_reduction_pct: f64 },
    /// Summarise the oldest messages only.
    PartialCompact {
        messages_to_compact: usize,
        expected_reduction_pct: f64,
    },
    /// Collapse repeated file reads before summarising anything.
    CollapseReads { expected_reduction_pct: f64 },
    /// Nothing needed.
    None,
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Characters a block contributes to the request body.
///
/// The single counter in this crate. `compact` sums it to decide when to
/// compact, and `analyze_context` sums it per category, so the two cannot
/// disagree about how large a conversation is.
pub fn block_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(t) => t.len(),
            ToolResultContent::Blocks(blocks) => blocks.iter().map(block_chars).sum(),
        },
        ContentBlock::Thinking { thinking, .. } => thinking.len(),
        ContentBlock::RedactedThinking { data } => data.len(),
        _ => 200, // images and documents carry no text to measure
    }
}

/// Characters a whole message contributes.
pub fn message_chars(message: &Message) -> usize {
    match &message.content {
        MessageContent::Text(t) => t.len(),
        MessageContent::Blocks(blocks) => blocks.iter().map(block_chars).sum(),
    }
}

/// Turn a character count into an estimated token count.
///
/// Roughly four characters per token, padded by a third because the estimate
/// runs low on structured content.
pub fn chars_to_tokens(chars: usize) -> u64 {
    ((chars / 4) * 4 / 3) as u64
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// Does this message carry a tool result?
///
/// The turn loop appends every tool result as a user message holding only
/// `ToolResult` blocks, so the block type is the only reliable signal. Reading
/// the message's text instead finds nothing at all, because a `ToolResult`
/// block is not a `Text` block.
fn is_tool_result(message: &Message) -> bool {
    matches!(&message.content, MessageContent::Blocks(blocks)
        if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })))
}

/// Does this message carry an injected attachment?
fn is_attachment(message: &Message) -> bool {
    let marks = ["[Attachment:", "[IDE:", "[Pasted", "<file path=\""];
    match &message.content {
        MessageContent::Text(text) => marks.iter().any(|m| text.contains(m)),
        MessageContent::Blocks(blocks) => blocks.iter().any(|b| match b {
            ContentBlock::Text { text } => marks.iter().any(|m| text.contains(m)),
            _ => false,
        }),
    }
}

/// Estimate the current message list and split it by category.
pub fn analyze_context(messages: &[Message]) -> ContextAnalysis {
    let mut conversation_history_tokens = 0u64;
    let mut tool_results_tokens = 0u64;
    let mut attachments_tokens = 0u64;

    for message in messages {
        let tokens = chars_to_tokens(message_chars(message));
        if is_tool_result(message) {
            tool_results_tokens += tokens;
        } else if is_attachment(message) {
            attachments_tokens += tokens;
        } else {
            conversation_history_tokens += tokens;
        }
    }

    let total_tokens = conversation_history_tokens + tool_results_tokens + attachments_tokens;

    // A summariser reclaims most of a tool result and about half of a
    // conversation turn. An attachment is the file the user asked about, so
    // treat it as incompressible.
    let compressibility = if total_tokens == 0 {
        0.0
    } else {
        let compressible =
            tool_results_tokens as f64 * 0.9 + conversation_history_tokens as f64 * 0.5;
        compressible / total_tokens as f64
    };

    ContextAnalysis {
        conversation_history_tokens,
        tool_results_tokens,
        attachments_tokens,
        total_tokens,
        compressibility,
    }
}

/// Recommend a compaction strategy.
///
/// `filled_tokens` is what the context actually holds, which is the measured
/// figure from the last request when there is one. It is not the analysis
/// total: the analysis covers the messages only, and the system prompt and the
/// tool definitions fill the window too.
pub fn suggest_compaction(
    analysis: &ContextAnalysis,
    filled_tokens: u64,
    context_limit: u64,
    message_count: usize,
) -> CompactionStrategy {
    if context_limit == 0 || filled_tokens == 0 {
        return CompactionStrategy::None;
    }

    let usage = filled_tokens as f64 / context_limit as f64;
    if usage < 0.75 {
        return CompactionStrategy::None;
    }

    if usage > 0.90 {
        return CompactionStrategy::FullCompact {
            expected_reduction_pct: analysis.compressibility * 70.0,
        };
    }

    // Between the two thresholds, collapsing repeated reads is cheaper than a
    // summary, but only when tool results are what fills the window.
    let tool_result_share = if analysis.total_tokens == 0 {
        0.0
    } else {
        analysis.tool_results_tokens as f64 / analysis.total_tokens as f64
    };
    if tool_result_share > 0.4 {
        return CompactionStrategy::CollapseReads {
            expected_reduction_pct: tool_result_share * 70.0,
        };
    }

    CompactionStrategy::PartialCompact {
        messages_to_compact: (message_count / 2).max(1),
        expected_reduction_pct: 40.0,
    }
}

impl CompactionStrategy {
    /// One line of advice, or nothing when the context has room.
    pub fn advice(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::FullCompact {
                expected_reduction_pct,
            } => Some(format!(
                "Run /compact to summarise the history (~{expected_reduction_pct:.0}% smaller)."
            )),
            Self::CollapseReads {
                expected_reduction_pct,
            } => Some(format!(
                "Tool results dominate. Run /compact to reclaim ~{expected_reduction_pct:.0}%."
            )),
            Self::PartialCompact {
                messages_to_compact,
                expected_reduction_pct,
            } => Some(format!(
                "Run /compact to summarise the oldest {messages_to_compact} messages \
                 (~{expected_reduction_pct:.0}% smaller)."
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

const BAR_WIDTH: usize = 40;

fn bar(pct: f64) -> String {
    let filled = ((pct / 100.0) * BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(BAR_WIDTH);
    "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled)
}

/// Render the context report `/context` prints.
///
/// `measured_input_tokens` is the API's own count for the last request, and 0
/// when no request has been sent yet. It is reported separately from the
/// estimate because the two describe different moments: the measurement covers
/// the messages that existed at the last request, the estimate covers the
/// messages that exist now.
pub fn format_context_report(
    analysis: &ContextAnalysis,
    model: &str,
    measured_input_tokens: u64,
    context_limit: u64,
    message_count: usize,
) -> String {
    let mut lines = Vec::new();

    lines.push("Context Window".to_string());
    lines.push("─".repeat(56));
    lines.push(format!("Model:          {model}"));
    lines.push(format!("Window:         {context_limit} tokens"));
    lines.push(String::new());

    if measured_input_tokens > 0 {
        let pct = measured_input_tokens as f64 / context_limit as f64 * 100.0;
        lines.push(format!(
            "Measured at the last request: {measured_input_tokens} tokens ({pct:.1}%)"
        ));
        lines.push(format!("[{}] {pct:.1}%", bar(pct)));
        lines.push(
            "  Counted by the API. Covers the system prompt and the tool definitions too."
                .to_string(),
        );
    } else {
        lines.push("Measured at the last request: nothing sent yet.".to_string());
    }
    lines.push(String::new());

    lines.push(format!(
        "Estimated now, from {message_count} messages: {} tokens",
        analysis.total_tokens
    ));
    if analysis.total_tokens == 0 {
        lines.push("  The conversation is empty.".to_string());
    } else {
        for cat in [
            ContextCategory::ConversationHistory,
            ContextCategory::ToolResults,
            ContextCategory::Attachments,
        ] {
            let tokens = analysis.category_tokens(cat);
            if tokens == 0 {
                continue;
            }
            let pct = analysis.category_pct(cat);
            lines.push(format!(
                "  {:<14} [{}] {pct:>5.1}%  {tokens} tokens",
                cat.label(),
                bar(pct)
            ));
        }
        lines.push(format!(
            "  Compressible: ~{:.0}% of the conversation.",
            analysis.compressibility * 100.0
        ));
    }

    let filled = measured_input_tokens.max(analysis.total_tokens);
    if let Some(advice) =
        suggest_compaction(analysis, filled, context_limit, message_count).advice()
    {
        lines.push(String::new());
        lines.push(advice);
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::types::{Message, MessageContent, Role, ToolResultContent};
    use serde_json::json;

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_string()),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            timestamp: None,
            tool_durations: None,
        }
    }

    fn tool_result_msg(body: &str) -> Message {
        Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: ToolResultContent::Text(body.to_string()),
            is_error: None,
        }])
    }

    fn tool_use_msg(input: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "Read".to_string(),
                input: json!({ "file_path": input }),
                thought_signature: None,
            }]),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            timestamp: None,
            tool_durations: None,
        }
    }

    #[test]
    fn a_tool_result_is_counted_as_one() {
        // The turn loop stores a tool result as a user message of ToolResult
        // blocks, which carries no Text block at all. Reading the message's
        // text finds an empty string, so a text-based split reported zero tool
        // results however many ran.
        let messages = vec![tool_result_msg(&"x".repeat(4000))];
        let analysis = analyze_context(&messages);
        assert!(
            analysis.tool_results_tokens > 500,
            "a 4000-character tool result counted as {} tokens",
            analysis.tool_results_tokens
        );
        assert_eq!(
            analysis.conversation_history_tokens, 0,
            "a tool result must not land in the conversation category"
        );
    }

    #[test]
    fn a_tool_call_counts_its_arguments() {
        // `get_all_text` drops a ToolUse block, so a turn of nothing but tool
        // calls used to weigh nothing.
        let messages = vec![tool_use_msg(&"/some/long/path".repeat(200))];
        let analysis = analyze_context(&messages);
        assert!(
            analysis.conversation_history_tokens > 500,
            "a tool call with a 3000-character argument counted as {} tokens",
            analysis.conversation_history_tokens
        );
    }

    #[test]
    fn the_categories_add_up_to_the_total() {
        let messages = vec![
            text_msg(Role::User, "hello"),
            tool_result_msg("result body"),
            text_msg(Role::User, "[Pasted text #1 +40 lines]"),
        ];
        let analysis = analyze_context(&messages);
        assert_eq!(
            analysis.total_tokens,
            analysis.conversation_history_tokens
                + analysis.tool_results_tokens
                + analysis.attachments_tokens
        );
        assert!(analysis.attachments_tokens > 0, "a paste is an attachment");
    }

    #[test]
    fn compaction_advice_follows_what_the_window_holds() {
        let analysis = ContextAnalysis {
            conversation_history_tokens: 10_000,
            total_tokens: 10_000,
            compressibility: 0.5,
            ..Default::default()
        };
        // The messages are small, but the window is nearly full because the
        // system prompt and the tool definitions fill it too. Judging by the
        // analysis total alone would advise nothing.
        assert!(matches!(
            suggest_compaction(&analysis, 195_000, 200_000, 20),
            CompactionStrategy::FullCompact { .. }
        ));
        assert_eq!(
            suggest_compaction(&analysis, 10_000, 200_000, 20),
            CompactionStrategy::None
        );
    }

    #[test]
    fn the_report_names_the_window_it_was_given() {
        let analysis = analyze_context(&[text_msg(Role::User, "hi")]);
        let report = format_context_report(&analysis, "claude-opus-5", 250_000, 1_000_000, 1);
        assert!(
            report.contains("1000000 tokens"),
            "the report must print the window it was given:\n{report}"
        );
        assert!(
            report.contains("250000 tokens (25.0%)"),
            "25% of a 1M window is not the 200k default:\n{report}"
        );
    }

    #[test]
    fn the_report_says_so_before_the_first_request() {
        let analysis = analyze_context(&[]);
        let report = format_context_report(&analysis, "claude-opus-5", 0, 200_000, 0);
        assert!(report.contains("nothing sent yet"));
        assert!(report.contains("The conversation is empty."));
    }
}
