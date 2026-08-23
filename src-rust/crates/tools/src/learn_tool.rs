//! Learn tool: record one durable lesson in the memory directory.
//!
//! The model can already write a memory file with `Write`, and the system
//! prompt tells it how. That is the right shape for a topic file, and the wrong
//! shape for a single sentence: the model has to invent a filename, write
//! frontmatter, check whether a near-duplicate is already there, and add a line
//! to the index. In practice it either skips the check and leaves five files
//! saying the same thing, or it skips the whole thing.
//!
//! This tool takes the sentence and does the bookkeeping. One file, newest
//! first, no duplicates, a bounded number of entries, and credentials masked on
//! the way in.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

/// The one file this tool writes.
pub const LEARNED_FILENAME: &str = "learned.md";

/// Entries kept. The oldest is dropped when a new one arrives.
///
/// The file is a memory file, so its body is loaded whole when a search picks
/// it. An unbounded log would eventually be the largest thing the model reads.
const MAX_ENTRIES: usize = 100;

/// Characters kept from a lesson.
const MAX_LESSON_CHARS: usize = 2000;

/// Characters kept from the optional context.
const MAX_CONTEXT_CHARS: usize = 400;

/// Written once, when the file is created.
const FRONTMATTER: &str = "---\n\
name: Learned lessons\n\
description: Durable lessons this project taught, newest first\n\
type: project\n\
---\n";

pub struct LearnTool;

#[derive(Debug, Deserialize)]
struct LearnInput {
    lesson: String,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

/// One recorded lesson, as it appears in the file.
struct Entry {
    /// The whole block, heading included, without a trailing newline.
    text: String,
    /// The lesson line, normalised for comparison.
    key: String,
}

/// Lower-case, whitespace collapsed. Two lessons that differ only in spacing or
/// capitalisation are the same lesson.
fn normalise(line: &str) -> String {
    line.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Keep at most `limit` characters, on a character boundary.
fn clip(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!("{}…", kept.trim_end())
}

/// Split a `learned.md` body into entries, newest first.
///
/// An entry is a `## ` heading and everything under it. Anything before the
/// first heading is dropped: the frontmatter is handled separately, and a
/// stray note between entries has no lesson line to compare.
fn parse_entries(body: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut current: Option<Vec<&str>> = None;

    for line in body.lines() {
        if line.starts_with("## ") {
            if let Some(lines) = current.take() {
                entries.push(build_entry(&lines));
            }
            current = Some(vec![line]);
        } else if let Some(lines) = current.as_mut() {
            lines.push(line);
        }
    }
    if let Some(lines) = current {
        entries.push(build_entry(&lines));
    }

    entries
}

/// The lesson is the first non-empty line under the heading.
fn build_entry(lines: &[&str]) -> Entry {
    let key = lines
        .iter()
        .skip(1)
        .find(|line| !line.trim().is_empty())
        .map(|line| normalise(line))
        .unwrap_or_default();
    Entry {
        text: lines.join("\n").trim_end().to_string(),
        key,
    }
}

/// Render the file from its entries.
fn render(entries: &[Entry]) -> String {
    let mut out = String::from(FRONTMATTER);
    for entry in entries {
        out.push('\n');
        out.push_str(&entry.text);
        out.push('\n');
    }
    out
}

/// Everything after the frontmatter block, or the whole text when there is none.
fn body_after_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    match rest.find("\n---\n") {
        Some(end) => &rest[end + "\n---\n".len()..],
        None => text,
    }
}

#[async_trait]
impl Tool for LearnTool {
    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_LEARN
    }

    fn description(&self) -> &str {
        "Record one durable lesson about this project, so a later session \
         starts knowing it. Use this for something that will still be true \
         next week: a convention, a constraint, a trap you fell into. Do not \
         use it for what you are doing right now. Lessons are kept newest \
         first, deduplicated, and loaded back through the Memory tool. For a \
         whole document rather than a sentence, write a memory file with Write \
         instead."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "lesson": {
                    "type": "string",
                    "description": format!(
                        "The lesson, in one or two sentences. Write it as a \
                         statement that stands on its own, because a later \
                         session reads it without this conversation. Kept to \
                         {MAX_LESSON_CHARS} characters."
                    )
                },
                "topic": {
                    "type": "string",
                    "description": "A few words naming what this is about, for the heading."
                },
                "context": {
                    "type": "string",
                    "description": format!(
                        "Optional. Where the lesson came from. Kept to \
                         {MAX_CONTEXT_CHARS} characters."
                    )
                }
            },
            "required": ["lesson"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: LearnInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        if params.lesson.trim().is_empty() {
            return ToolResult::error("lesson must not be empty");
        }

        let project_root = mikmik_core::session_storage::transcript_root_for(&ctx.working_dir);
        let memory_dir = mikmik_core::memdir::auto_memory_path(&project_root);
        let path = memory_dir.join(LEARNED_FILENAME);

        // Masked rather than refused. The tool is the model recording what it
        // understood, and refusing would lose the whole lesson over a value
        // that is not the point of it. `Write` refuses instead, because there
        // the content is the model's own and it can send it again without it.
        let lesson = mikmik_core::redact::redact_secrets(&clip(&params.lesson, MAX_LESSON_CHARS));
        let context = params
            .context
            .as_deref()
            .map(|text| mikmik_core::redact::redact_secrets(&clip(text, MAX_CONTEXT_CHARS)));

        let mut masked: Vec<&'static str> = lesson.classes.clone();
        if let Some(context) = &context {
            for class in &context.classes {
                if !masked.contains(class) {
                    masked.push(class);
                }
            }
        }
        if !masked.is_empty() {
            tracing::warn!(
                classes = %masked.join(", "),
                path = %path.display(),
                "Masked a credential on its way into a learned lesson"
            );
        }

        let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let mut entries = parse_entries(body_after_frontmatter(&existing));

        let key = normalise(&lesson.text);
        if entries.iter().any(|entry| entry.key == key) {
            return ToolResult::success(format!(
                "Already recorded, so nothing was written. {} holds this lesson.",
                path.display()
            ));
        }

        let heading = match params.topic.as_deref().map(str::trim) {
            Some(topic) if !topic.is_empty() => {
                format!("## {} — {}", chrono::Local::now().format("%Y-%m-%d"), topic)
            }
            _ => format!("## {}", chrono::Local::now().format("%Y-%m-%d")),
        };

        let mut text = format!("{heading}\n{}", lesson.text);
        if let Some(context) = &context {
            if !context.text.is_empty() {
                text.push_str(&format!("\n\n_context: {}_", context.text));
            }
        }

        entries.insert(0, Entry { text, key });
        let dropped = entries.len().saturating_sub(MAX_ENTRIES);
        entries.truncate(MAX_ENTRIES);

        if let Err(error) = tokio::fs::create_dir_all(&memory_dir).await {
            return ToolResult::error(format!(
                "Failed to create {}: {error}",
                memory_dir.display()
            ));
        }
        if let Err(error) = tokio::fs::write(&path, render(&entries)).await {
            return ToolResult::error(format!("Failed to write {}: {error}", path.display()));
        }

        let mut report = format!(
            "Recorded in {} ({} lessons).",
            path.display(),
            entries.len()
        );
        if dropped > 0 {
            report.push_str(&format!(
                " The {dropped} oldest dropped at the {MAX_ENTRIES}-lesson cap."
            ));
        }
        if !masked.is_empty() {
            report.push_str(&format!(
                " A credential was masked before writing ({}).",
                masked.join(", ")
            ));
        }
        ToolResult::success(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that redirect the memory directory.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct MemoryDirGuard {
        saved: Option<std::ffi::OsString>,
    }

    impl MemoryDirGuard {
        fn new(dir: &std::path::Path) -> Self {
            let saved = std::env::var_os("MIKMIK_MEMORY_PATH_OVERRIDE");
            std::env::set_var("MIKMIK_MEMORY_PATH_OVERRIDE", dir);
            Self { saved }
        }
    }

    impl Drop for MemoryDirGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var("MIKMIK_MEMORY_PATH_OVERRIDE", value),
                None => std::env::remove_var("MIKMIK_MEMORY_PATH_OVERRIDE"),
            }
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        _guard: MemoryDirGuard,
        ctx: ToolContext,
        learned: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let memory = dir.path().join("memory");
        let guard = MemoryDirGuard::new(&memory);
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        Fixture {
            _dir: dir,
            _guard: guard,
            ctx,
            learned: memory.join(LEARNED_FILENAME),
        }
    }

    async fn learn(ctx: &ToolContext, lesson: &str) -> ToolResult {
        LearnTool.execute(json!({ "lesson": lesson }), ctx).await
    }

    #[tokio::test]
    async fn a_lesson_lands_in_a_file_the_scan_can_index() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        let result = learn(&f.ctx, "Cargo commands run from src-rust.").await;
        assert!(!result.is_error, "{}", result.content);

        let written = std::fs::read_to_string(&f.learned).expect("read back");
        let (name, description, memory_type) =
            mikmik_core::memdir::parse_frontmatter_quick(&written);
        assert_eq!(name.as_deref(), Some("Learned lessons"));
        assert!(description.is_some());
        assert_eq!(memory_type, Some(mikmik_core::memdir::MemoryType::Project));
        assert!(written.contains("Cargo commands run from src-rust."));
    }

    /// Newest first, because a search loads the body whole and the reader
    /// stops early.
    #[tokio::test]
    async fn the_newest_lesson_comes_first() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        learn(&f.ctx, "first lesson").await;
        learn(&f.ctx, "second lesson").await;

        let written = std::fs::read_to_string(&f.learned).expect("read back");
        let first = written.find("second lesson").expect("newest missing");
        let second = written.find("first lesson").expect("oldest missing");
        assert!(first < second, "the oldest lesson was on top:\n{written}");
        assert_eq!(written.matches("name: Learned lessons").count(), 1);
    }

    /// Without this the model records the same thing every session and the
    /// file becomes the largest memory it owns.
    #[tokio::test]
    async fn the_same_lesson_is_not_recorded_twice() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        learn(&f.ctx, "Cargo commands run from src-rust.").await;
        let again = learn(&f.ctx, "  cargo COMMANDS   run from src-rust.  ").await;

        assert!(!again.is_error, "{}", again.content);
        assert!(
            again.content.contains("Already recorded"),
            "{}",
            again.content
        );
        let written = std::fs::read_to_string(&f.learned).expect("read back");
        assert_eq!(written.matches("run from src-rust").count(), 1, "{written}");
    }

    #[tokio::test]
    async fn the_oldest_lesson_drops_at_the_cap() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        for i in 0..MAX_ENTRIES {
            learn(&f.ctx, &format!("lesson number {i}")).await;
        }
        let written = std::fs::read_to_string(&f.learned).expect("read back");
        assert!(written.contains("lesson number 0"));

        let result = learn(&f.ctx, "one lesson too many").await;
        assert!(
            result.content.contains("oldest dropped"),
            "{}",
            result.content
        );

        let written = std::fs::read_to_string(&f.learned).expect("read back");
        assert!(!written.contains("lesson number 0"), "the cap did not fire");
        assert!(written.contains("one lesson too many"));
        assert_eq!(written.matches("\n## ").count(), MAX_ENTRIES);
    }

    #[tokio::test]
    async fn a_long_lesson_is_clipped() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let long = "x".repeat(MAX_LESSON_CHARS + 500);

        learn(&f.ctx, &long).await;

        let written = std::fs::read_to_string(&f.learned).expect("read back");
        assert!(written.contains('…'), "nothing was clipped");
        assert!(
            written.matches('x').count() <= MAX_LESSON_CHARS,
            "the clip let {} characters through",
            written.matches('x').count()
        );
    }

    /// The lesson is masked rather than refused: refusing would lose the whole
    /// lesson over a value that is not the point of it.
    #[tokio::test]
    async fn a_credential_in_a_lesson_is_masked() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        // Assembled at run time: a contiguous `ghp_AAAA…` in the source is a
        // GitHub token as far as push protection is concerned.
        let secret = format!("ghp{}{}", "_", "A".repeat(30));

        let result = learn(&f.ctx, &format!("the deploy token is {secret}")).await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("masked"), "{}", result.content);
        let written = std::fs::read_to_string(&f.learned).expect("read back");
        assert!(!written.contains(&secret), "{written}");
        assert!(written.contains("[REDACTED]"), "{written}");
    }

    #[tokio::test]
    async fn a_topic_and_a_context_are_both_kept() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        let result = LearnTool
            .execute(
                json!({
                    "lesson": "The release workflow refuses a tag it has already seen.",
                    "topic": "releases",
                    "context": "found while tagging",
                }),
                &f.ctx,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        let written = std::fs::read_to_string(&f.learned).expect("read back");
        assert!(written.contains("— releases"), "{written}");
        assert!(
            written.contains("_context: found while tagging_"),
            "{written}"
        );
    }

    #[tokio::test]
    async fn an_empty_lesson_is_rejected() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        assert!(learn(&f.ctx, "   ").await.is_error);
    }

    /// The file has to survive a round trip, or a second call would lose what
    /// the first one wrote.
    #[tokio::test]
    async fn parsing_what_was_rendered_gives_the_same_entries() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        learn(&f.ctx, "first lesson").await;
        learn(&f.ctx, "second lesson").await;
        learn(&f.ctx, "third lesson").await;

        let written = std::fs::read_to_string(&f.learned).expect("read back");
        let entries = parse_entries(body_after_frontmatter(&written));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].key, "third lesson");
        assert_eq!(entries[2].key, "first lesson");
    }
}
