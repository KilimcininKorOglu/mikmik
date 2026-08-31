//! Retain tool: record one durable fact in the memory directory.
//!
//! `Retain` is the fact twin of `Learn`. `Learn` records a reusable lesson (a
//! convention, a constraint, a trap); `Retain` records a plain fact the model
//! established mid-run and wants a later session to start knowing (a port, an
//! owner, a path, a decision). They write to separate stores so facts and
//! lessons never collide, but both hand the sentence to the session's memory
//! backend, which does the bookkeeping (newest first, no duplicates, a bounded
//! number of entries, credentials masked on the way in).

use crate::memory_backend::{backend_for, file::RETAIN};
use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct RetainTool;

#[derive(Debug, Deserialize)]
struct RetainInput {
    fact: String,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

#[async_trait]
impl Tool for RetainTool {
    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_RETAIN
    }

    fn description(&self) -> &str {
        "Retain one durable fact about this project, so a later session starts \
         knowing it. Use this for a concrete fact you established: a port, an \
         owner, a file path, a decision that was made. For a reusable lesson (a \
         convention, a constraint, a trap), use Learn instead. Facts are kept \
         newest first, deduplicated, and loaded back through the Memory tool. \
         For a whole document rather than a sentence, write a memory file with \
         Write instead."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fact": {
                    "type": "string",
                    "description": format!(
                        "The fact, in one or two sentences. Write it as a \
                         statement that stands on its own, because a later \
                         session reads it without this conversation. Kept to {} \
                         characters.",
                        RETAIN.max_item_chars
                    )
                },
                "topic": {
                    "type": "string",
                    "description": "A few words naming what this is about, for the heading."
                },
                "context": {
                    "type": "string",
                    "description": format!(
                        "Optional. Where the fact came from. Kept to {} characters.",
                        RETAIN.max_context_chars
                    )
                }
            },
            "required": ["fact"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: RetainInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        if params.fact.trim().is_empty() {
            return ToolResult::error("fact must not be empty");
        }

        let project_root = mikmik_core::session_storage::transcript_root_for(&ctx.working_dir);
        let memory_dir = mikmik_core::memdir::auto_memory_path(&project_root);

        backend_for(ctx.config.memory_backend.as_deref(), &memory_dir)
            .retain_fact(
                &params.fact,
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
        facts: std::path::PathBuf,
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
            facts: memory.join(RETAIN.filename),
        }
    }

    async fn retain(ctx: &ToolContext, fact: &str) -> ToolResult {
        RetainTool.execute(json!({ "fact": fact }), ctx).await
    }

    #[tokio::test]
    async fn a_fact_lands_in_its_own_file_the_scan_can_index() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        let result = retain(&f.ctx, "The relay binds 127.0.0.1:8350.").await;
        assert!(!result.is_error, "{}", result.content);

        let written = std::fs::read_to_string(&f.facts).expect("read back");
        let (name, description, memory_type) =
            mikmik_core::memdir::parse_frontmatter_quick(&written);
        assert_eq!(name.as_deref(), Some("Retained facts"));
        assert!(description.is_some());
        assert_eq!(memory_type, Some(mikmik_core::memdir::MemoryType::Project));
        assert!(written.contains("The relay binds 127.0.0.1:8350."));
    }

    #[tokio::test]
    async fn the_newest_fact_comes_first() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        retain(&f.ctx, "first fact").await;
        retain(&f.ctx, "second fact").await;

        let written = std::fs::read_to_string(&f.facts).expect("read back");
        let first = written.find("second fact").expect("newest missing");
        let second = written.find("first fact").expect("oldest missing");
        assert!(first < second, "the oldest fact was on top:\n{written}");
        assert_eq!(written.matches("name: Retained facts").count(), 1);
    }

    #[tokio::test]
    async fn the_same_fact_is_not_recorded_twice() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        retain(&f.ctx, "The relay binds 127.0.0.1:8350.").await;
        let again = retain(&f.ctx, "  the RELAY   binds 127.0.0.1:8350.  ").await;

        assert!(!again.is_error, "{}", again.content);
        assert!(
            again.content.contains("Already recorded"),
            "{}",
            again.content
        );
        let written = std::fs::read_to_string(&f.facts).expect("read back");
        assert_eq!(
            written.matches("binds 127.0.0.1:8350").count(),
            1,
            "{written}"
        );
    }

    #[tokio::test]
    async fn the_oldest_fact_drops_at_the_cap() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        for i in 0..RETAIN.cap {
            retain(&f.ctx, &format!("fact number {i}")).await;
        }
        assert!(std::fs::read_to_string(&f.facts)
            .expect("read back")
            .contains("fact number 0"));

        let result = retain(&f.ctx, "one fact too many").await;
        assert!(
            result.content.contains("oldest dropped"),
            "{}",
            result.content
        );

        let written = std::fs::read_to_string(&f.facts).expect("read back");
        assert!(!written.contains("fact number 0"), "the cap did not fire");
        assert!(written.contains("one fact too many"));
        assert_eq!(written.matches("\n## ").count(), RETAIN.cap);
    }

    #[tokio::test]
    async fn a_credential_in_a_fact_is_masked() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        let secret = format!("ghp{}{}", "_", "A".repeat(30));

        let result = retain(&f.ctx, &format!("the deploy token is {secret}")).await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("masked"), "{}", result.content);
        let written = std::fs::read_to_string(&f.facts).expect("read back");
        assert!(!written.contains(&secret), "{written}");
        assert!(written.contains("[REDACTED]"), "{written}");
    }

    #[tokio::test]
    async fn a_topic_and_a_context_are_both_kept() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();

        let result = RetainTool
            .execute(
                json!({
                    "fact": "The default relay port is 8350.",
                    "topic": "relay",
                    "context": "read from relay/src",
                }),
                &f.ctx,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        let written = std::fs::read_to_string(&f.facts).expect("read back");
        assert!(written.contains("— relay"), "{written}");
        assert!(
            written.contains("_context: read from relay/src_"),
            "{written}"
        );
    }

    #[tokio::test]
    async fn an_empty_fact_is_rejected() {
        let _lock = ENV_LOCK.lock().await;
        let f = fixture();
        assert!(retain(&f.ctx, "   ").await.is_error);
    }
}
