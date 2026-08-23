// FileRead tool: read files with optional line range, image support, PDF page ranges.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct FileReadTool;

#[derive(Debug, Deserialize)]
struct FileReadInput {
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for FileReadTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_FILE_READ
    }

    fn description(&self) -> &str {
        "Reads a file from the local filesystem. You can access any file directly. \
         By default reads up to 2000 lines from the beginning. Results are returned \
         with line numbers starting at 1. This tool can read images (PNG, JPG) and \
         PDF files."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to read: absolute, relative to the working directory, or &<root-name>/<relative-path> for another workspace root"
                },
                "offset": {
                    "type": "number",
                    "description": "The line number to start reading from (1-based). Only provide if the file is too large to read at once."
                },
                "limit": {
                    "type": "number",
                    "description": "The number of lines to read. Only provide if the file is too large to read at once."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: FileReadInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let path = match ctx.resolve_path(&params.file_path) {
            Ok(path) => path,
            Err(message) => return ToolResult::error(message),
        };
        debug!(path = %path.display(), "Reading file");

        // Permission check
        if let Err(e) = ctx.check_permission_for_path(
            self.name(),
            &format!("Read {}", path.display()),
            path.clone(),
            true,
        ) {
            return ToolResult::error(e.to_string());
        }

        // Check if file exists
        if !path.exists() {
            return ToolResult::error(format!("File not found: {}", path.display()));
        }

        // Check if it's a directory
        if path.is_dir() {
            return ToolResult::error(format!(
                "{} is a directory, not a file. Use Bash with `ls` to list directory contents.",
                path.display()
            ));
        }

        // Detect binary / image files by extension
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let image_exts = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico"];
        if image_exts.contains(&ext.as_str()) {
            return ToolResult::success(format!(
                "[Image file: {}. The image content has been captured for visual analysis.]",
                path.display()
            ));
        }

        if ext == "pdf" {
            return ToolResult::success(format!(
                "[PDF file: {}. Use the `pages` parameter to read specific page ranges.]",
                path.display()
            ));
        }

        // Read text file
        let content = match ctx.read_text(&path).await {
            Ok(c) => c,
            Err(e) => {
                // Might be binary
                if e.kind() == std::io::ErrorKind::InvalidData {
                    return ToolResult::error(format!(
                        "File appears to be binary and cannot be displayed as text: {}",
                        path.display()
                    ));
                }
                return ToolResult::error(format!("Failed to read file: {}", e));
            }
        };

        if content.is_empty() {
            return ToolResult::success(format!("[File {} exists but is empty]", path.display()));
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = params.offset.unwrap_or(0);
        let limit = params.limit.unwrap_or(2000);

        // Convert 1-based offset to 0-based index
        let start = if offset > 0 { offset - 1 } else { 0 };
        let end = (start + limit).min(total_lines);

        if start >= total_lines {
            return ToolResult::error(format!(
                "Offset {} exceeds total line count {} in {}",
                offset,
                total_lines,
                path.display()
            ));
        }

        let mut output = String::new();
        let width = format!("{}", end).len();

        for (i, line) in lines[start..end].iter().enumerate() {
            let line_num = start + i + 1;
            output.push_str(&format!("{:>width$}\t{}\n", line_num, line, width = width));
        }

        if end < total_lines {
            output.push_str(&format!(
                "\n... ({} more lines, {} total. Use offset/limit to read more.)\n",
                total_lines - end,
                total_lines
            ));
        }

        // Record what this read displayed, so an edit can be held to it. A read
        // that showed the whole file records no line set at all, because there
        // is then nothing the model has not seen. See `crate::edit_guard`.
        let displayed = if start == 0 && end == total_lines {
            None
        } else {
            Some((start + 1..=end).collect())
        };
        ctx.file_snapshots.lock().record(&path, &content, displayed);

        ToolResult::success(output)
    }
}

#[cfg(test)]
mod workspace_root_tests {
    use super::*;
    use crate::test_support::allow_all_context;

    /// A session rooted at one temp directory with a second one added.
    fn two_directory_context() -> (tempfile::TempDir, tempfile::TempDir, ToolContext) {
        let main = tempfile::tempdir().expect("main dir");
        let docs = tempfile::tempdir().expect("docs dir");
        std::fs::write(docs.path().join("spec.md"), "spec content").expect("write spec");

        let mut ctx = allow_all_context(main.path().to_path_buf());
        ctx.config.additional_dirs = vec![docs.path().to_path_buf()];
        (main, docs, ctx)
    }

    #[tokio::test]
    async fn a_file_in_another_root_is_read_by_root_name() {
        let (_main, docs, ctx) = two_directory_context();
        let root_name = docs
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .expect("root name");

        let result = FileReadTool
            .execute(
                serde_json::json!({ "file_path": format!("&{root_name}/spec.md") }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("spec content"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_mistyped_root_name_is_reported_to_the_caller() {
        let (_main, _docs, ctx) = two_directory_context();

        let result = FileReadTool
            .execute(serde_json::json!({ "file_path": "&nope/spec.md" }), &ctx)
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("&nope"), "{}", result.content);
        assert!(result.content.contains("&main"), "{}", result.content);
    }

    #[tokio::test]
    async fn a_plain_relative_path_still_resolves_against_the_working_directory() {
        let (main, _docs, ctx) = two_directory_context();
        std::fs::write(main.path().join("here.txt"), "local content").expect("write local");

        let result = FileReadTool
            .execute(serde_json::json!({ "file_path": "here.txt" }), &ctx)
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("local content"),
            "{}",
            result.content
        );
    }
}
