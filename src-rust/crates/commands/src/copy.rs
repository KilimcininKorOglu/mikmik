// `/copy` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct CopyCommand;

/// How `/copy` should render the message it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    /// The message's text with nothing added. What `/copy` has always done.
    Raw,
    /// Markdown, with the role as a heading and tool calls in fenced blocks.
    Markdown,
    /// Markdown formatting stripped out.
    Plaintext,
    /// Only the fenced code blocks.
    Code,
    /// The message as a JSON object, cost included.
    Json,
}

impl CopyFormat {
    fn from_word(word: &str) -> Option<Self> {
        match word {
            "markdown" | "md" => Some(Self::Markdown),
            "text" | "plain" | "plaintext" => Some(Self::Plaintext),
            "code" => Some(Self::Code),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    fn render(self, message: &mikmik_core::types::Message) -> String {
        use mikmik_tui::message_copy;
        match self {
            Self::Raw => message.get_all_text(),
            Self::Markdown => message_copy::copy_as_markdown(message),
            Self::Plaintext => message_copy::copy_as_plaintext(message),
            Self::Code => message_copy::copy_code_blocks(message),
            Self::Json => message_copy::copy_as_json(message),
        }
    }
}

/// What the user asked for, or the word that made no sense.
///
/// Order does not matter: `/copy code 2` and `/copy 2 code` mean the same
/// thing, because neither reading is ambiguous.
pub fn parse_copy_args(args: &str) -> Result<(CopyFormat, usize), String> {
    let mut format = CopyFormat::Raw;
    let mut nth = 1usize;

    for word in args.split_whitespace() {
        if let Ok(n) = word.parse::<usize>() {
            nth = n.max(1);
            continue;
        }
        match CopyFormat::from_word(&word.to_ascii_lowercase()) {
            Some(f) => format = f,
            // Refused rather than ignored: a typo that silently copied the raw
            // text would look like the format simply did nothing.
            None => {
                return Err(format!(
                    "{word:?} is not a copy format. Use markdown, text, code or json, \
                     optionally with a number for an older response."
                ))
            }
        }
    }

    Ok((format, nth))
}

// ---- /copy ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for CopyCommand {
    fn name(&self) -> &str {
        "copy"
    }
    fn description(&self) -> &str {
        "Copy the last assistant response to the clipboard"
    }
    fn help(&self) -> &str {
        "Usage: /copy [format] [n]\n\n\
         Copies the most recent assistant response to the system clipboard.\n\
         Pass a number to copy the Nth most-recent response instead.\n\n\
         Formats:\n\
         \x20 (none)    the response text, unchanged\n\
         \x20 markdown  role heading, thinking blocks and tool calls kept\n\
         \x20 text      markdown formatting stripped out\n\
         \x20 code      only the fenced code blocks\n\
         \x20 json      the message as JSON, cost included"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let (format, n) = match parse_copy_args(args) {
            Ok(parsed) => parsed,
            Err(message) => return CommandResult::Error(message),
        };

        // Find the Nth most recent assistant message
        let assistant_msgs: Vec<&mikmik_core::types::Message> = ctx
            .messages
            .iter()
            .rev()
            .filter(|m| m.role == mikmik_core::types::Role::Assistant)
            .take(n)
            .collect();

        let msg = match assistant_msgs.last() {
            Some(m) => m,
            None => {
                return CommandResult::Message(
                    "No assistant messages found in conversation.".to_string(),
                )
            }
        };

        let text = format.render(msg);
        if text.is_empty() {
            return CommandResult::Message("Last assistant message is empty.".to_string());
        }

        // Try system clipboard via arboard
        #[cfg(not(target_os = "linux"))]
        {
            match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.clone())) {
                Ok(()) => {
                    let preview: String = text.chars().take(80).collect();
                    let ellipsis = if text.len() > 80 { "…" } else { "" };
                    return CommandResult::Message(format!(
                        "Copied {} chars to clipboard.\nPreview: {}{}",
                        text.len(),
                        preview,
                        ellipsis
                    ));
                }
                Err(e) => {
                    tracing::warn!("Clipboard write failed: {}", e);
                    // Fall through to file fallback
                }
            }
        }

        // Fallback: write to a temp file and inform the user
        let tmp_path = std::env::temp_dir().join("claude_copy.md");
        match std::fs::write(&tmp_path, &text) {
            Ok(()) => {
                let preview: String = text.chars().take(80).collect();
                let ellipsis = if text.len() > 80 { "…" } else { "" };
                CommandResult::Message(format!(
                    "Clipboard not available; saved {} chars to {}\nPreview: {}{}",
                    text.len(),
                    tmp_path.display(),
                    preview,
                    ellipsis
                ))
            }
            Err(e) => CommandResult::Error(format!("Failed to copy: {}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::types::{ContentBlock, Message, MessageContent, Role};

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::Text {
                text: text.to_string(),
            }]),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            timestamp: None,
            tool_durations: None,
        }
    }

    #[test]
    fn no_argument_copies_the_text_unchanged() {
        // The default is what /copy has always done. Every named format adds a
        // role prefix or a wrapper, so making one of them the default would
        // change what existing users get.
        assert_eq!(parse_copy_args(""), Ok((CopyFormat::Raw, 1)));
        assert_eq!(
            CopyFormat::Raw.render(&assistant("hello")),
            "hello".to_string()
        );
    }

    #[test]
    fn a_number_alone_still_picks_an_older_response() {
        assert_eq!(parse_copy_args("3"), Ok((CopyFormat::Raw, 3)));
        assert_eq!(parse_copy_args("0"), Ok((CopyFormat::Raw, 1)), "0 means 1");
    }

    #[test]
    fn every_format_name_resolves() {
        for (word, expected) in [
            ("markdown", CopyFormat::Markdown),
            ("md", CopyFormat::Markdown),
            ("text", CopyFormat::Plaintext),
            ("plain", CopyFormat::Plaintext),
            ("code", CopyFormat::Code),
            ("json", CopyFormat::Json),
            ("JSON", CopyFormat::Json),
        ] {
            assert_eq!(parse_copy_args(word), Ok((expected, 1)), "{word}");
        }
    }

    #[test]
    fn a_format_and_a_number_can_be_given_in_either_order() {
        assert_eq!(parse_copy_args("code 2"), Ok((CopyFormat::Code, 2)));
        assert_eq!(parse_copy_args("2 code"), Ok((CopyFormat::Code, 2)));
    }

    #[test]
    fn an_unknown_word_is_refused_and_names_the_valid_ones() {
        let err = parse_copy_args("yaml").expect_err("yaml is not a format");
        assert!(err.contains("markdown"), "{err}");
        assert!(err.contains("json"), "{err}");
    }

    #[test]
    fn each_format_renders_through_its_own_function() {
        let msg = assistant("intro\n\n```rust\nfn main() {}\n```");

        assert!(CopyFormat::Markdown.render(&msg).contains("**Assistant**"));
        assert!(CopyFormat::Plaintext.render(&msg).starts_with("Assistant:"));

        let code = CopyFormat::Code.render(&msg);
        assert!(code.contains("fn main()"), "{code}");
        assert!(!code.contains("intro"), "code only: {code}");

        let json = CopyFormat::Json.render(&msg);
        assert!(json.contains("\"role\": \"assistant\""), "{json}");
    }
}
