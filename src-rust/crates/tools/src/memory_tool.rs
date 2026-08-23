// Memory tool: load the bodies of the memory files that match a query.
//
// The system prompt already lists what the project's memory directory holds
// (name, type, description, age). Loading every body there would spend the
// whole directory's tokens on every turn, so the manifest names the files and
// this tool fetches the few that matter.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

/// Files returned when the caller does not ask for a number.
const DEFAULT_MAX_FILES: usize = 3;

/// Ceiling on `max_files`, whatever the caller asks for.
///
/// Each file is a whole document, so a large answer crowds out the
/// conversation it was meant to inform.
const MAX_FILES_LIMIT: usize = 10;

pub struct MemoryTool;

#[derive(Debug, Deserialize)]
struct MemoryInput {
    query: String,
    #[serde(default)]
    max_files: Option<usize>,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_MEMORY
    }

    fn description(&self) -> &str {
        "Load the full text of memory files about a topic. The system prompt \
         lists which memory files exist; use this to read the ones that look \
         relevant. Scored against each file's name, description, filename and \
         body, with the name weighing most, so search by topic rather than by \
         exact wording."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What you are trying to remember, in a few words."
                },
                "max_files": {
                    "type": "integer",
                    "description": format!(
                        "How many files to load. Defaults to {DEFAULT_MAX_FILES}, \
                         capped at {MAX_FILES_LIMIT}."
                    ),
                    "minimum": 1
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: MemoryInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        if params.query.trim().is_empty() {
            return ToolResult::error("query must not be empty");
        }

        let project_root = mikmik_core::session_storage::transcript_root_for(&ctx.working_dir);
        let memory_dir = mikmik_core::memdir::auto_memory_path(&project_root);

        let max_files = params
            .max_files
            .unwrap_or(DEFAULT_MAX_FILES)
            .clamp(1, MAX_FILES_LIMIT);

        let matches = mikmik_core::memdir::find_relevant_memories_simple(
            &memory_dir,
            &params.query,
            max_files,
        );

        if matches.is_empty() {
            return ToolResult::success(format!(
                "No memory file matches \"{}\". The directory is {}.",
                params.query.trim(),
                memory_dir.display()
            ));
        }

        // The freshness note leads each body rather than trailing it: by the
        // time the model has read a stale claim it has already been believed.
        let body = matches
            .iter()
            .map(|file| {
                format!(
                    "{}## {}\n\n{}",
                    mikmik_core::memdir::memory_freshness_note(file.meta.modified_secs),
                    file.meta.filename,
                    file.content.trim()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        ToolResult::success(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// Serialises the tests that redirect the memory directory.
    ///
    /// Async-aware: each test holds it across the tool's own `await`.
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

    /// Write a memory file and backdate it by `age_days`.
    fn write_memory(dir: &std::path::Path, name: &str, body: &str, age_days: u64) {
        std::fs::create_dir_all(dir).expect("create dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write memory");

        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("reopen memory");
        let when = SystemTime::now() - Duration::from_secs(age_days * 86400);
        file.set_modified(when).expect("backdate");
    }

    async fn run(query: &str, cwd: &std::path::Path) -> ToolResult {
        let ctx = crate::test_support::allow_all_context(cwd.to_path_buf());
        MemoryTool.execute(json!({ "query": query }), &ctx).await
    }

    #[tokio::test]
    async fn a_matching_file_comes_back_whole() {
        let _lock = ENV_LOCK.lock().await;
        let cwd = tempfile::tempdir().expect("tempdir");
        let memory = cwd.path().join("memory");
        write_memory(
            &memory,
            "deploy.md",
            "---\nname: Deploy\ndescription: How releases reach production\n---\nTag, then wait for CI.",
            0,
        );
        let _guard = MemoryDirGuard::new(&memory);

        let result = run("releases", cwd.path()).await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("Tag, then wait for CI."),
            "the body was not loaded:\n{}",
            result.content
        );
    }

    /// A stale memory must announce its age before the model reads the claim.
    #[tokio::test]
    async fn an_old_file_is_prefixed_with_its_freshness_note() {
        let _lock = ENV_LOCK.lock().await;
        let cwd = tempfile::tempdir().expect("tempdir");
        let memory = cwd.path().join("memory");
        write_memory(
            &memory,
            "layout.md",
            "---\nname: Layout\ndescription: Where the crates live\n---\nold claim",
            47,
        );
        let _guard = MemoryDirGuard::new(&memory);

        let result = run("crates", cwd.path()).await;

        let note_end = result
            .content
            .find("</system-reminder>")
            .unwrap_or_else(|| panic!("no freshness note:\n{}", result.content));
        let claim = result.content.find("old claim").expect("body missing");
        assert!(note_end < claim, "the note trailed the claim it qualifies");
        assert!(result.content.contains("47 days old"));
    }

    #[tokio::test]
    async fn a_query_that_matches_nothing_says_where_to_look() {
        let _lock = ENV_LOCK.lock().await;
        let cwd = tempfile::tempdir().expect("tempdir");
        let memory = cwd.path().join("memory");
        write_memory(&memory, "deploy.md", "---\nname: Deploy\n---\nbody", 0);
        let _guard = MemoryDirGuard::new(&memory);

        let result = run("kubernetes", cwd.path()).await;

        assert!(!result.is_error, "an empty result is not a failure");
        assert!(result.content.contains(&memory.display().to_string()));
    }

    #[tokio::test]
    async fn an_empty_query_is_rejected() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let result = run("   ", cwd.path()).await;
        assert!(result.is_error);
    }
}
