//! Learn tool: record one durable lesson in the memory directory.
//!
//! The model can already write a memory file with `Write`, and the system
//! prompt tells it how. That is the right shape for a topic file, and the wrong
//! shape for a single sentence: the model has to invent a filename, write
//! frontmatter, check whether a near-duplicate is already there, and add a line
//! to the index. In practice it either skips the check and leaves five files
//! saying the same thing, or it skips the whole thing.
//!
//! This tool takes the sentence and hands it to the session's memory backend,
//! which does the bookkeeping (one place, newest first, no duplicates, a
//! bounded number of entries, credentials masked on the way in). `Retain` is
//! the fact twin; both ride the backend so a sqlite session records the same
//! lesson into the database instead of a file.

use crate::memory_backend::{backend_for, file::LEARN};
use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct LearnTool;

#[derive(Debug, Deserialize)]
struct LearnInput {
    lesson: String,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    context: Option<String>,
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
         use it for what you are doing right now, and do not use it for a plain \
         fact about the code (use Retain for that). Lessons are kept newest \
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
                         session reads it without this conversation. Kept to {} \
                         characters.",
                        LEARN.max_item_chars
                    )
                },
                "topic": {
                    "type": "string",
                    "description": "A few words naming what this is about, for the heading."
                },
                "context": {
                    "type": "string",
                    "description": format!(
                        "Optional. Where the lesson came from. Kept to {} characters.",
                        LEARN.max_context_chars
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

        backend_for(ctx.config.memory_backend.as_deref(), &memory_dir)
            .append_lesson(
                &params.lesson,
                params.topic.as_deref(),
                params.context.as_deref(),
            )
            .await
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
            learned: memory.join(LEARN.filename),
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

        for i in 0..LEARN.cap {
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
        assert_eq!(written.matches("\n## ").count(), LEARN.cap);
    }

    #[tokio::test]
    async fn a_long_lesson_is_clipped() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let long = "x".repeat(LEARN.max_item_chars + 500);

        learn(&f.ctx, &long).await;

        let written = std::fs::read_to_string(&f.learned).expect("read back");
        assert!(written.contains('…'), "nothing was clipped");
        assert!(
            written.matches('x').count() <= LEARN.max_item_chars,
            "the clip let {} characters through",
            written.matches('x').count()
        );
    }

    #[tokio::test]
    async fn a_credential_in_a_lesson_is_masked() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
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
}
