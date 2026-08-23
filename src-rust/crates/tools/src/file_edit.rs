// FileEdit tool: exact string replacement with old/new strings (like sed but
// deterministic).  Mirrors the TypeScript Edit tool behaviour.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct FileEditTool;

#[derive(Debug, Deserialize)]
struct FileEditInput {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for FileEditTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_FILE_EDIT
    }

    fn description(&self) -> &str {
        "Performs exact string replacements in files. The edit will FAIL if \
         `old_string` is not unique in the file (unless `replace_all` is true). \
         You MUST read the file first before editing. Preserve the exact \
         indentation as it appears in the file."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to modify: absolute, relative to the working directory, or &<root-name>/<relative-path> for another workspace root"
                },
                "old_string": {
                    "type": "string",
                    "description": "The text to replace (must be unique in the file unless replace_all is true)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with (must be different from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences of old_string (default false)"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: FileEditInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // Validate old != new
        if params.old_string == params.new_string {
            return ToolResult::error("old_string and new_string must be different".to_string());
        }

        if params.old_string.is_empty() {
            return ToolResult::error("old_string must not be empty".to_string());
        }

        let path = match ctx.resolve_path(&params.file_path) {
            Ok(path) => path,
            Err(message) => return ToolResult::error(message),
        };
        debug!(path = %path.display(), "Editing file");

        // Permission check
        if let Err(e) = ctx.check_permission_for_path(
            self.name(),
            &format!("Edit {}", path.display()),
            path.clone(),
            false,
        ) {
            return ToolResult::error(e.to_string());
        }

        // Read current content
        let content = match ctx.read_text(&path).await {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::error(format!("Failed to read file {}: {}", path.display(), e));
            }
        };

        // Detect the file's original/dominant line ending BEFORE editing so we
        // can re-apply it on write (#225).  Matching is done against an
        // LF-normalized view so CRLF/LF differences never affect the match, but
        // only the lines the edit actually changes are ever rewritten.
        let eol = crate::line_endings::LineEnding::detect(&content);
        let normalized = content.replace("\r\n", "\n");
        let old_string = params.old_string.replace("\r\n", "\n");
        let new_string = params.new_string.replace("\r\n", "\n");

        // Count occurrences
        let count = normalized.matches(&old_string).count();

        if count == 0 {
            // Through the guard, so a third identical attempt stops repeating
            // advice that has already failed twice.
            return ToolResult::error(crate::edit_guard::describe_failed_match(
                ctx,
                &path,
                &old_string,
                format!(
                    "old_string not found in {}. Make sure the string matches exactly, \
                     including whitespace and indentation.",
                    path.display()
                ),
            ));
        }

        // Held to what this session read: the file must still be the one it
        // read, and at `strict` the changed lines must have been displayed.
        // Silent for a file this session never read.
        if let Some(refusal) =
            crate::edit_guard::check(ctx, &path, &content, &old_string, params.replace_all)
        {
            return ToolResult::error(refusal.message);
        }

        if count > 1 && !params.replace_all {
            return ToolResult::error(format!(
                "old_string appears {} times in {}. Either provide a larger string \
                 with more surrounding context to make it unique, or set replace_all \
                 to true to replace every occurrence.",
                count,
                path.display()
            ));
        }

        // Perform the replacement on the ORIGINAL bytes, preserving every
        // untouched region's line endings and re-rendering inserted lines with
        // the file's dominant line ending.
        let (new_content, _replacements) = crate::line_endings::replace_preserving_eol(
            &content,
            &old_string,
            &new_string,
            eol,
            params.replace_all,
        );

        // A write that produces the bytes already on disk. Reachable when
        // `old_string` and `new_string` differ only in their line endings,
        // because the equality check above runs before normalization. Reporting
        // success would tell the model its change landed when nothing moved.
        if new_content == content {
            return ToolResult::error(format!(
                "This edit leaves {} byte for byte as it is, so nothing was written. \
                 old_string and new_string differ only in line endings, which this \
                 tool normalizes before matching.",
                path.display()
            ));
        }

        // What this call adds, not the whole file: a memory file that already
        // carries a credential still has to be editable, and the edit that
        // removes it must not be the one that gets refused.
        if let Some(refusal) =
            crate::memory_guard::refuse_secret_write(ctx, &path, &params.new_string)
        {
            return ToolResult::error(refusal);
        }

        // Write back
        if let Err(e) = ctx.write_text(&path, new_content.as_bytes()).await {
            return ToolResult::error(format!("Failed to write file {}: {}", path.display(), e));
        }

        ctx.record_file_change(
            path.clone(),
            content.as_bytes(),
            new_content.as_bytes(),
            self.name(),
        );

        // Run any configured formatter for this file type.
        crate::try_format_file(&path.to_string_lossy(), ctx).await;

        // After the formatter, never before it: it rewrites the file, and a
        // record taken ahead of it would make the next edit read as stale.
        crate::edit_guard::record_applied_edit(
            ctx,
            &path,
            &content,
            &new_content,
            &old_string,
            &new_string,
            if params.replace_all { count } else { 1 },
        )
        .await;

        // Build a diff snippet for the response
        let replacements = if params.replace_all { count } else { 1 };
        // What the language server makes of the edit, appended to the result:
        // the model otherwise learns that its edit does not compile only if it
        // asks, and it usually does not.
        let lsp_note = crate::lsp_after_write::report_after_write(&path.to_string_lossy(), ctx)
            .await
            .map(|note| format!("\n{note}"))
            .unwrap_or_default();
        let msg = format!(
            "Successfully edited {} ({} replacement{}).{}",
            path.display(),
            replacements,
            if replacements != 1 { "s" } else { "" },
            lsp_note
        );

        mikmik_plugins::run_global_hook(
            mikmik_plugins::HookEventKind::FileChanged,
            Some(&path.to_string_lossy()),
            json!({
                "file_path": path.display().to_string(),
                "change": "edited",
                "replacements": replacements,
            }),
        )
        .await;

        ToolResult::success(msg).with_metadata(json!({
            "file_path": path.display().to_string(),
            "replacements": replacements,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;

    /// #225: editing a CRLF file must keep CRLF; only the edited line changes.
    #[tokio::test]
    async fn edit_crlf_file_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        let original = "line one\r\nline two\r\nline three\r\n";
        std::fs::write(&path, original).unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "line two",
                    "new_string": "LINE TWO",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "edit failed: {}", res.content);

        let after = std::fs::read_to_string(&path).unwrap();
        // Only the target changed; every other line kept its CRLF.
        assert_eq!(after, "line one\r\nLINE TWO\r\nline three\r\n");
        // No line ending was flipped to a bare LF.
        assert_eq!(after.matches('\n').count(), after.matches("\r\n").count());
    }

    /// #225: an LF file must stay LF (no stray CR introduced).
    #[tokio::test]
    async fn edit_lf_file_stays_lf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lf.txt");
        let original = "line one\nline two\nline three\n";
        std::fs::write(&path, original).unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "line two",
                    "new_string": "LINE TWO",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "edit failed: {}", res.content);

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "line one\nLINE TWO\nline three\n");
        assert!(!after.contains('\r'), "LF file gained a CR: {:?}", after);
    }

    // -----------------------------------------------------------------------
    // The guard, driven through the tool rather than the helper
    // -----------------------------------------------------------------------

    /// A session that read the file, then something else rewrote it.
    async fn read_then_change(guard: &str) -> (tempfile::TempDir, ToolContext, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "let a = 1;\nlet b = 2;\n").unwrap();

        let mut ctx = allow_all_context(dir.path().to_path_buf());
        ctx.config.edit_guard = Some(guard.to_string());

        let read = crate::FileReadTool
            .execute(json!({ "file_path": path.to_string_lossy() }), &ctx)
            .await;
        assert!(!read.is_error, "{}", read.content);

        // Somebody else moves the file under the session.
        std::fs::write(&path, "let a = 1;\nlet b = 99;\nlet c = 3;\n").unwrap();
        (dir, ctx, path)
    }

    /// The failure the guard exists for: the file moved, `old_string` still
    /// matches, and without the check the edit lands on a file the model never
    /// reasoned about.
    #[tokio::test]
    async fn the_guard_refuses_an_edit_to_a_file_that_changed_after_the_read() {
        let (_dir, ctx, path) = read_then_change("stale").await;

        let result = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "let a = 1;",
                    "new_string": "let a = 7;",
                }),
                &ctx,
            )
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("changed after"),
            "{}",
            result.content
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "let a = 1;\nlet b = 99;\nlet c = 3;\n",
            "the file was written despite the refusal"
        );
    }

    /// Off by default, so an upgrade changes nothing until the user asks.
    #[tokio::test]
    async fn the_default_lets_the_same_edit_through() {
        let (_dir, ctx, path) = read_then_change("off").await;
        assert_eq!(
            ctx.config.effective_edit_guard(),
            mikmik_core::file_snapshot::EditGuard::Off
        );

        let result = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "let a = 1;",
                    "new_string": "let a = 7;",
                }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("let a = 7;"));
    }

    /// A partial read leaves most of the file undisplayed. At `strict` an edit
    /// to a line the model never saw is refused and the line is quoted back.
    #[tokio::test]
    async fn strict_refuses_an_edit_to_a_line_the_read_never_showed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();

        let mut ctx = allow_all_context(dir.path().to_path_buf());
        ctx.config.edit_guard = Some("strict".to_string());

        let read = crate::FileReadTool
            .execute(
                json!({ "file_path": path.to_string_lossy(), "offset": 1, "limit": 2 }),
                &ctx,
            )
            .await;
        assert!(!read.is_error, "{}", read.content);

        let blind = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "four",
                    "new_string": "FOUR",
                }),
                &ctx,
            )
            .await;
        assert!(blind.is_error, "{}", blind.content);
        assert!(blind.content.contains("4: four"), "{}", blind.content);

        // A line the read did show is still editable.
        let seen = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "two",
                    "new_string": "TWO",
                }),
                &ctx,
            )
            .await;
        assert!(!seen.is_error, "{}", seen.content);
    }

    /// Two edits in a row against one file. The second must not be refused for
    /// the first one's change: that is the shape that would make the guard
    /// unusable in practice.
    #[tokio::test]
    async fn a_second_edit_after_the_first_is_not_refused_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let mut ctx = allow_all_context(dir.path().to_path_buf());
        ctx.config.edit_guard = Some("strict".to_string());

        let read = crate::FileReadTool
            .execute(json!({ "file_path": path.to_string_lossy() }), &ctx)
            .await;
        assert!(!read.is_error, "{}", read.content);

        for (old, new) in [("one", "ONE"), ("three", "THREE")] {
            let result = FileEditTool
                .execute(
                    json!({
                        "file_path": path.to_string_lossy(),
                        "old_string": old,
                        "new_string": new,
                    }),
                    &ctx,
                )
                .await;
            assert!(!result.is_error, "editing {old}: {}", result.content);
        }

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\ntwo\nTHREE\n");
    }

    /// `old_string` and `new_string` that differ only in line endings pass the
    /// inequality check above and then normalize to the same text. Reporting
    /// success would tell the model a change landed when nothing moved.
    #[tokio::test]
    async fn an_edit_that_writes_the_same_bytes_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let ctx = allow_all_context(dir.path().to_path_buf());

        let result = FileEditTool
            .execute(
                json!({
                    "file_path": path.to_string_lossy(),
                    "old_string": "one\ntwo",
                    "new_string": "one\r\ntwo",
                }),
                &ctx,
            )
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("byte for byte"),
            "{}",
            result.content
        );
    }

    /// The third identical failure stops repeating advice that has not worked.
    #[tokio::test]
    async fn a_third_identical_failure_says_to_read_instead() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "one\n").unwrap();

        let mut ctx = allow_all_context(dir.path().to_path_buf());
        ctx.config.edit_guard = Some("stale".to_string());

        let attempt = || async {
            FileEditTool
                .execute(
                    json!({
                        "file_path": path.to_string_lossy(),
                        "old_string": "nowhere",
                        "new_string": "somewhere",
                    }),
                    &ctx,
                )
                .await
                .content
        };

        assert!(!attempt().await.contains("attempt 3"));
        assert!(!attempt().await.contains("attempt 3"));
        let third = attempt().await;
        assert!(third.contains("attempt 3"), "{third}");
    }
}
