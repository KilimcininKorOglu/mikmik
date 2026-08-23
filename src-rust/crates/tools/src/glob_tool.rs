// Glob tool: fast file pattern matching.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use tracing::debug;

pub struct GlobTool;

#[derive(Debug, Deserialize)]
struct GlobInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_GLOB
    }

    fn description(&self) -> &str {
        "Fast file pattern matching tool that works with any codebase size. \
         Supports glob patterns like \"**/*.rs\" or \"src/**/*.ts\". Returns \
         matching file paths sorted by modification time. Use this tool when \
         you need to find files by name patterns. Files excluded by .gitignore \
         or .ignore are skipped unless the includeIgnoredFiles setting is on."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match files against"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. Accepts &<root-name> to search another workspace root. Defaults to the working directory."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: GlobInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let base_dir = match params.path.as_deref() {
            Some(path) => match ctx.resolve_path(path) {
                Ok(path) => path,
                Err(message) => return ToolResult::error(message),
            },
            None => ctx.working_dir.clone(),
        };

        if let Err(e) = ctx.check_permission_for_path(
            self.name(),
            &format!("Glob {} in {}", params.pattern, base_dir.display()),
            base_dir.clone(),
            true,
        ) {
            return ToolResult::error(e.to_string());
        }

        debug!(pattern = %params.pattern, dir = %base_dir.display(), "Running glob");

        if !base_dir.exists() || !base_dir.is_dir() {
            return ToolResult::error(format!("Directory not found: {}", base_dir.display()));
        }

        // Build the full glob pattern
        let full_pattern = base_dir.join(&params.pattern);
        let pattern_str = full_pattern.to_string_lossy().to_string();

        // On Windows, normalize backslashes to forward slashes for the glob crate
        let pattern_str = pattern_str.replace('\\', "/");

        // The walk decides which files the pattern gets to see; the pattern
        // itself is still matched by the glob crate, so a pattern keeps meaning
        // what it meant. `require_literal_separator` is the one option that has
        // to be set: `glob()` walked the tree component by component, so `*`
        // could never cross a `/`, while `matches_path` runs against the whole
        // path and would let `*.rs` swallow `src/lib.rs`. `**` crosses
        // directories either way.
        let options = glob::MatchOptions {
            require_literal_separator: true,
            ..glob::MatchOptions::new()
        };
        let pattern = match glob::Pattern::new(&pattern_str) {
            Ok(pattern) => pattern,
            Err(e) => return ToolResult::error(format!("Invalid glob pattern: {}", e)),
        };

        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in
            crate::ignore_aware_walk(&base_dir, ctx.config.effective_include_ignored_files())
        {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !pattern.matches_path_with(path, options) {
                continue;
            }
            if !ctx.path_is_within_workspace(path) {
                if let Err(e) = ctx.check_permission_for_path(
                    self.name(),
                    &format!("Glob result {}", path.display()),
                    path.to_path_buf(),
                    true,
                ) {
                    return ToolResult::error(e.to_string());
                }
            }
            entries.push(path.to_path_buf());
        }

        if entries.is_empty() {
            return ToolResult::success(format!(
                "No files matched pattern \"{}\" in {}",
                params.pattern,
                base_dir.display()
            ));
        }

        // Sort by modification time (most recent first) — fall back to name sort
        let mut entries_with_time: Vec<(PathBuf, std::time::SystemTime)> = entries
            .into_iter()
            .filter_map(|p| {
                let mtime = std::fs::metadata(&p).ok()?.modified().ok()?;
                Some((p, mtime))
            })
            .collect();

        entries_with_time.sort_by_key(|b| std::cmp::Reverse(b.1));

        let total = entries_with_time.len();
        let max_results = 250;
        let truncated = total > max_results;

        let mut output = String::new();
        for (path, _) in entries_with_time.iter().take(max_results) {
            output.push_str(&path.display().to_string());
            output.push('\n');
        }

        if truncated {
            output.push_str(&format!(
                "\n... and {} more files (showing first {})\n",
                total - max_results,
                max_results,
            ));
        }

        ToolResult::success(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::config::Config;
    use mikmik_core::permissions::AutoPermissionHandler;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// A tree with one ignored directory, one hidden directory, and a `.git`.
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let write = |rel: &str, body: &str| {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(path, body).expect("write");
        };

        write(".gitignore", "build/\n");
        write("top.txt", "top");
        write("src/nested.txt", "nested");
        write("build/artifact.txt", "artifact");
        write(".github/workflows/ci.txt", "ci");
        write(".git/config", "gitdir");
        write("top.rs", "fn main() {}");
        write("src/lib.rs", "pub fn f() {}");

        dir
    }

    fn ctx_for(root: &Path, include_ignored: bool) -> ToolContext {
        let config = Config {
            include_ignored_files: Some(include_ignored),
            ..Default::default()
        };
        ToolContext {
            working_dir: root.to_path_buf(),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: Arc::new(AutoPermissionHandler {
                mode: mikmik_core::config::PermissionMode::Default,
            }),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "test-glob".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: Arc::new(AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config,
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            plan_approval_tx: None,
            tool_output_tx: None,
            plan_mode_tx: None,
            advisor_note_tx: None,
            advisor_name: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            current_call: None,
            editor: None,
        }
    }

    async fn run(root: &Path, pattern: &str, include_ignored: bool) -> String {
        let ctx = ctx_for(root, include_ignored);
        let result = GlobTool.execute(json!({ "pattern": pattern }), &ctx).await;
        assert!(!result.is_error, "glob failed: {}", result.content);
        result.content
    }

    #[tokio::test]
    async fn a_gitignored_directory_is_skipped() {
        let dir = tree();
        let out = run(dir.path(), "**/*.txt", false).await;

        assert!(out.contains("top.txt"), "{out}");
        assert!(out.contains("nested.txt"), "{out}");
        assert!(!out.contains("artifact.txt"), "build/ is ignored: {out}");
    }

    #[tokio::test]
    async fn the_setting_brings_the_ignored_directory_back() {
        let dir = tree();
        let out = run(dir.path(), "**/*.txt", true).await;

        assert!(out.contains("artifact.txt"), "{out}");
    }

    #[tokio::test]
    async fn a_hidden_directory_is_still_searched() {
        // Being hidden is not an ignore rule, and .github/workflows holds real
        // source that a search has to reach.
        let dir = tree();
        let out = run(dir.path(), "**/*.txt", false).await;

        assert!(out.contains("ci.txt"), "{out}");
    }

    #[tokio::test]
    async fn the_git_directory_is_never_searched() {
        let dir = tree();
        let out = run(dir.path(), "**/*", false).await;

        assert!(!out.contains(".git/config"), "{out}");
    }

    #[tokio::test]
    async fn pattern_semantics_are_unchanged() {
        let dir = tree();

        let recursive = run(dir.path(), "**/*.rs", false).await;
        assert!(recursive.contains("top.rs"), "{recursive}");
        assert!(recursive.contains("lib.rs"), "{recursive}");

        let shallow = run(dir.path(), "*.rs", false).await;
        assert!(shallow.contains("top.rs"), "{shallow}");
        assert!(!shallow.contains("lib.rs"), "one level only: {shallow}");
    }
}
