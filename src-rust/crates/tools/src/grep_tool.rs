//! Grep tool: content search on ripgrep's own engine.
//!
//! `grep-regex` compiles the pattern into a line-oriented matcher and
//! `grep-searcher` does the reading. That is what ripgrep itself runs, and it
//! decides three things this tool used to get wrong by reading each file into
//! a `String`:
//!
//! - a large file is streamed through a line buffer rather than allocated
//!   whole, and every line is no longer copied into a `Vec`;
//! - a file that is not valid UTF-8 is searched instead of skipped, so an
//!   ASCII match in a latin-1 file is found. Only a file holding a NUL byte is
//!   treated as binary and abandoned, which is ripgrep's own rule;
//! - context lines come from the engine, so `--` lands between groups rather
//!   than after every match.
//!
//! The walk runs across every core. Results are sorted by path before they are
//! rendered, so the same search answers the same way twice.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tracing::debug;

pub struct GrepTool;

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, rename = "type")]
    file_type: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default = "default_output_mode")]
    output_mode: String,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default, rename = "-i")]
    case_insensitive: bool,
    #[serde(default, rename = "-n")]
    show_line_numbers: Option<bool>,
    #[serde(default)]
    head_limit: Option<usize>,
    #[serde(default)]
    multiline: bool,
}

fn default_output_mode() -> String {
    "files_with_matches".to_string()
}

/// Map file type shorthand to extensions (similar to ripgrep --type).
fn extensions_for_type(t: &str) -> Vec<&'static str> {
    match t {
        "rust" | "rs" => vec!["rs"],
        "js" => vec!["js", "jsx", "mjs", "cjs"],
        "ts" => vec!["ts", "tsx", "mts", "cts"],
        "py" | "python" => vec!["py", "pyi"],
        "go" => vec!["go"],
        "java" => vec!["java"],
        "c" => vec!["c", "h"],
        "cpp" => vec!["cpp", "hpp", "cc", "hh", "cxx"],
        "rb" | "ruby" => vec!["rb"],
        "php" => vec!["php"],
        "swift" => vec!["swift"],
        "kt" | "kotlin" => vec!["kt", "kts"],
        "css" => vec!["css", "scss", "sass", "less"],
        "html" => vec!["html", "htm"],
        "json" => vec!["json"],
        "yaml" | "yml" => vec!["yaml", "yml"],
        "toml" => vec!["toml"],
        "xml" => vec!["xml"],
        "md" | "markdown" => vec!["md", "markdown"],
        "sh" | "shell" | "bash" => vec!["sh", "bash", "zsh"],
        _ => vec![],
    }
}

/// What the caller asked to see. An unknown string reads as the default, which
/// is what the old string comparison did through its catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    FilesWithMatches,
    Count,
    Content,
}

impl Mode {
    fn parse(value: &str) -> Self {
        match value {
            "content" => Self::Content,
            "count" => Self::Count,
            _ => Self::FilesWithMatches,
        }
    }
}

/// One file's answer.
///
/// `lines` is empty unless the mode is `Content`; the other two modes need the
/// count alone, and rendering a line nobody asked for is the allocation this
/// rewrite is meant to stop.
struct FileHits {
    path: PathBuf,
    count: usize,
    lines: Vec<String>,
}

/// Collects one file's matches as `grep-searcher` streams them.
///
/// The callbacks arrive in file order: context, match, context, then a break
/// between groups. Rendering here rather than afterwards keeps the matched
/// bytes borrowed from the engine's own buffer.
struct Collect<'a> {
    path: &'a Path,
    mode: Mode,
    show_line_numbers: bool,
    count: usize,
    lines: Vec<String>,
}

impl Collect<'_> {
    fn push(&mut self, line_number: Option<u64>, bytes: &[u8]) {
        if self.mode != Mode::Content {
            return;
        }
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\n', '\r']);
        let prefix = match (self.show_line_numbers, line_number) {
            (true, Some(number)) => format!("{}:{}:", self.path.display(), number),
            _ => format!("{}:", self.path.display()),
        };
        self.lines.push(format!("{prefix}{text}"));
    }
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.count += 1;
        let mut number = mat.line_number();
        for line in mat.lines() {
            self.push(number, line);
            number = number.map(|value| value + 1);
        }
        // One match settles the answer in this mode, so the rest of the file
        // is work nobody reads.
        Ok(self.mode != Mode::FilesWithMatches)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        self.push(context.line_number(), context.bytes());
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        if self.mode == Mode::Content {
            self.lines.push("--".to_string());
        }
        Ok(true)
    }
}

/// Search one file, answering `None` when nothing matched.
fn search_one(
    path: &Path,
    matcher: &RegexMatcher,
    searcher: &mut Searcher,
    mode: Mode,
    show_line_numbers: bool,
) -> Option<FileHits> {
    let mut sink = Collect {
        path,
        mode,
        show_line_numbers,
        count: 0,
        lines: Vec::new(),
    };
    // A file that cannot be read is skipped rather than reported: a walk over a
    // working tree meets sockets, dangling symlinks and files the user may not
    // read, and none of them is what the caller asked about.
    searcher.search_path(matcher, path, &mut sink).ok()?;
    if sink.count == 0 {
        return None;
    }
    Some(FileHits {
        path: path.to_path_buf(),
        count: sink.count,
        lines: sink.lines,
    })
}

/// A searcher configured for this request.
///
/// Built once per walker thread: `Searcher` owns the line buffer, so sharing
/// one would serialise the walk it is meant to parallelise.
fn build_searcher(context_lines: usize, multiline: bool) -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .before_context(context_lines)
        .after_context(context_lines)
        .multi_line(multiline)
        // ripgrep's own default: stop at the first NUL rather than print a
        // binary file's bytes into the model's context.
        .binary_detection(BinaryDetection::quit(0))
        .build()
}

/// Whether this file passes the `type` and `glob` filters.
fn passes_filters(path: &Path, type_exts: &[&str], glob_pattern: Option<&glob::Pattern>) -> bool {
    if !type_exts.is_empty() {
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !type_exts.contains(&ext) {
            return false;
        }
    }
    if let Some(pattern) = glob_pattern {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !pattern.matches(name) {
            return false;
        }
    }
    true
}

#[async_trait]
impl Tool for GrepTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_GREP
    }

    fn description(&self) -> &str {
        "A powerful search tool built on regex. Supports full regex syntax. \
         Filter files with the `glob` parameter or `type` parameter. Output \
         modes: \"content\" shows matching lines, \"files_with_matches\" shows \
         only file paths (default), \"count\" shows match counts. Files excluded \
         by .gitignore or .ignore are skipped unless the includeIgnoredFiles \
         setting is on."
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
                    "description": "The regular expression pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in. Accepts &<root-name> to search another workspace root. Defaults to the working directory."
                },
                "type": {
                    "type": "string",
                    "description": "File type to search (e.g. js, py, rust, go)"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.js\")"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode (default: files_with_matches)"
                },
                "context": {
                    "type": "number",
                    "description": "Number of context lines before and after each match"
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case insensitive search"
                },
                "-n": {
                    "type": "boolean",
                    "description": "Show line numbers (for content mode)"
                },
                "head_limit": {
                    "type": "number",
                    "description": "Limit output to first N entries (default 250)"
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Enable multiline mode where . matches newlines"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: GrepInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let search_path = match params.path.as_deref() {
            Some(path) => match ctx.resolve_path(path) {
                Ok(path) => path,
                Err(message) => return ToolResult::error(message),
            },
            None => ctx.working_dir.clone(),
        };

        if let Err(e) = ctx.check_permission_for_path(
            self.name(),
            &format!("Grep {} in {}", params.pattern, search_path.display()),
            search_path.clone(),
            true,
        ) {
            return ToolResult::error(e.to_string());
        }

        debug!(pattern = %params.pattern, path = %search_path.display(), "Running grep");

        let mode = Mode::parse(&params.output_mode);
        let head_limit = params.head_limit.unwrap_or(250);
        let context_lines = params.context.unwrap_or(0);
        let show_line_numbers = params.show_line_numbers.unwrap_or(true);

        let matcher = match RegexMatcherBuilder::new()
            .case_insensitive(params.case_insensitive)
            .dot_matches_new_line(params.multiline)
            .multi_line(params.multiline)
            .build(&params.pattern)
        {
            Ok(matcher) => matcher,
            Err(e) => return ToolResult::error(format!("Invalid regex: {}", e)),
        };

        // If the search path is a single file, just search it.
        if search_path.is_file() {
            let mut searcher = build_searcher(context_lines, params.multiline);
            return match search_one(
                &search_path,
                &matcher,
                &mut searcher,
                mode,
                show_line_numbers,
            ) {
                Some(hits) => ToolResult::success(render_single(&hits, mode)),
                None => {
                    ToolResult::success(format!("No matches found in {}", search_path.display()))
                }
            };
        }

        let type_exts: Vec<&str> = params
            .file_type
            .as_deref()
            .map(extensions_for_type)
            .unwrap_or_default();
        // Compiled once. The old code rebuilt this pattern for every file it
        // walked and dropped the error, so an invalid glob silently matched
        // everything.
        let glob_pattern = match params.glob.as_deref().map(glob::Pattern::new).transpose() {
            Ok(pattern) => pattern,
            Err(e) => return ToolResult::error(format!("Invalid glob: {}", e)),
        };

        // A file outside the workspace is not searched inside the walker: the
        // permission check can put a question to the user, and a prompt raised
        // from one of several walker threads has no turn to belong to.
        let hits = parking_lot::Mutex::new(Vec::<FileHits>::new());
        let deferred = parking_lot::Mutex::new(Vec::<PathBuf>::new());

        crate::ignore_aware_walk_parallel(
            &search_path,
            ctx.config.effective_include_ignored_files(),
        )
        .run(|| {
            let mut searcher = build_searcher(context_lines, params.multiline);
            let matcher = &matcher;
            let type_exts = &type_exts;
            let glob_pattern = glob_pattern.as_ref();
            let hits = &hits;
            let deferred = &deferred;
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let path = entry.path();
                if !passes_filters(path, type_exts, glob_pattern) {
                    return ignore::WalkState::Continue;
                }
                if !ctx.path_is_within_workspace(path) {
                    deferred.lock().push(path.to_path_buf());
                    return ignore::WalkState::Continue;
                }
                if let Some(found) =
                    search_one(path, matcher, &mut searcher, mode, show_line_numbers)
                {
                    hits.lock().push(found);
                }
                ignore::WalkState::Continue
            })
        });

        let mut hits = hits.into_inner();

        let mut deferred = deferred.into_inner();
        deferred.sort();
        if !deferred.is_empty() {
            let mut searcher = build_searcher(context_lines, params.multiline);
            for path in deferred {
                if let Err(e) = ctx.check_permission_for_path(
                    self.name(),
                    &format!("Grep result {}", path.display()),
                    path.clone(),
                    true,
                ) {
                    return ToolResult::error(e.to_string());
                }
                if let Some(found) =
                    search_one(&path, &matcher, &mut searcher, mode, show_line_numbers)
                {
                    hits.push(found);
                }
            }
        }

        if hits.is_empty() {
            return ToolResult::success(format!(
                "No matches found for pattern \"{}\" in {}",
                params.pattern,
                search_path.display()
            ));
        }

        // Sorted before rendering: the walk runs on several threads, so the
        // order files arrive in is whatever the disk answered first.
        hits.sort_by(|left, right| left.path.cmp(&right.path));
        ToolResult::success(render(hits, mode, head_limit))
    }
}

/// Render the whole result set, stopping once `head_limit` entries are out.
///
/// An entry is a file in the two summary modes and a match in `content` mode,
/// which is the accounting the tool has always used. A file is added whole, so
/// the last one can carry the count past the limit rather than cut a match
/// away from its context.
fn render(hits: Vec<FileHits>, mode: Mode, head_limit: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut entries = 0usize;
    for file in hits {
        if entries >= head_limit {
            break;
        }
        match mode {
            Mode::FilesWithMatches => {
                out.push(file.path.display().to_string());
                entries += 1;
            }
            Mode::Count => {
                out.push(format!("{}:{}", file.path.display(), file.count));
                entries += 1;
            }
            Mode::Content => {
                out.extend(file.lines);
                entries += file.count;
            }
        }
    }
    out.join("\n")
}

/// Render a search the caller aimed at one named file.
///
/// The path is already known there, so it is stripped back off each line: what
/// stays is `<number>:<text>` when line numbers are on and the text alone when
/// they are off.
fn render_single(hits: &FileHits, mode: Mode) -> String {
    match mode {
        Mode::FilesWithMatches => hits.path.display().to_string(),
        Mode::Count => format!("{}:{}", hits.path.display(), hits.count),
        Mode::Content => {
            let prefix = format!("{}:", hits.path.display());
            hits.lines
                .iter()
                .map(|line| line.strip_prefix(&prefix).unwrap_or(line).to_string())
                .collect::<Vec<_>>()
                .join("\n")
        }
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

    /// A tree holding the same needle in an ignored, a hidden and a plain spot.
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

        write(".gitignore", "dist/\n");
        write("src/main.rs", "// NEEDLE in source\n");
        write("dist/bundle.js", "// NEEDLE in build output\n");
        write(".github/workflows/ci.yml", "# NEEDLE in workflow\n");
        write(".git/COMMIT_EDITMSG", "NEEDLE in git metadata\n");

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
            session_id: "test-grep".to_string(),
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
            inbox: Default::default(),
        }
    }

    async fn search(root: &Path, include_ignored: bool) -> String {
        let ctx = ctx_for(root, include_ignored);
        let result = GrepTool.execute(json!({ "pattern": "NEEDLE" }), &ctx).await;
        assert!(!result.is_error, "grep failed: {}", result.content);
        result.content
    }

    #[tokio::test]
    async fn a_gitignored_directory_is_skipped() {
        let dir = tree();
        let out = search(dir.path(), false).await;

        assert!(out.contains("main.rs"), "{out}");
        assert!(!out.contains("bundle.js"), "dist/ is ignored: {out}");
    }

    #[tokio::test]
    async fn the_setting_brings_the_ignored_directory_back() {
        let dir = tree();
        let out = search(dir.path(), true).await;

        assert!(out.contains("bundle.js"), "{out}");
    }

    #[tokio::test]
    async fn a_hidden_directory_is_now_searched() {
        // The old fixed list dropped every hidden directory, so a workflow file
        // was unreachable even though nothing ignores it.
        let dir = tree();
        let out = search(dir.path(), false).await;

        assert!(out.contains("ci.yml"), "{out}");
    }

    #[tokio::test]
    async fn the_git_directory_is_never_searched() {
        let dir = tree();
        let out = search(dir.path(), false).await;

        assert!(!out.contains("COMMIT_EDITMSG"), "{out}");
    }

    // -----------------------------------------------------------------------
    // The engine underneath
    // -----------------------------------------------------------------------

    /// Run one search and return its output, failing the test on a tool error.
    async fn run(root: &Path, input: Value) -> String {
        let ctx = ctx_for(root, false);
        let result = GrepTool.execute(input, &ctx).await;
        assert!(!result.is_error, "grep failed: {}", result.content);
        result.content
    }

    #[tokio::test]
    async fn a_file_that_is_not_utf8_is_still_searched() {
        // The old path read every file with `read_to_string` and skipped
        // whatever failed, so an ASCII match sitting next to one latin-1 byte
        // was unreachable. Only a NUL makes a file binary.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut bytes = b"caf\xe9 NEEDLE here\n".to_vec();
        bytes.extend_from_slice(b"second line\n");
        std::fs::write(dir.path().join("latin1.txt"), &bytes).expect("write");
        std::fs::write(dir.path().join("binary.bin"), b"NEEDLE\x00after").expect("write");

        let out = run(dir.path(), json!({ "pattern": "NEEDLE" })).await;

        assert!(out.contains("latin1.txt"), "{out}");
        assert!(
            !out.contains("binary.bin"),
            "a binary file was searched: {out}"
        );
    }

    #[tokio::test]
    async fn context_lines_come_out_around_the_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nNEEDLE\nfour\nfive\n").expect("write");

        let out = run(
            dir.path(),
            json!({ "pattern": "NEEDLE", "output_mode": "content", "context": 1 }),
        )
        .await;

        let numbers: Vec<&str> = out
            .lines()
            .filter_map(|line| line.rsplit(':').nth(1))
            .collect();
        assert_eq!(numbers, vec!["2", "3", "4"], "{out}");
        assert!(out.contains("two"), "{out}");
        assert!(!out.contains("five"), "context reached too far: {out}");
    }

    #[tokio::test]
    async fn content_mode_carries_the_line_number_and_can_drop_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "first\nNEEDLE\n").expect("write");

        let with = run(
            dir.path(),
            json!({ "pattern": "NEEDLE", "output_mode": "content" }),
        )
        .await;
        assert!(with.ends_with("a.txt:2:NEEDLE"), "{with}");

        let without = run(
            dir.path(),
            json!({ "pattern": "NEEDLE", "output_mode": "content", "-n": false }),
        )
        .await;
        assert!(without.ends_with("a.txt:NEEDLE"), "{without}");
    }

    #[tokio::test]
    async fn count_mode_counts_every_match_in_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "NEEDLE\nx\nNEEDLE\nNEEDLE\n").expect("write");

        let out = run(
            dir.path(),
            json!({ "pattern": "NEEDLE", "output_mode": "count" }),
        )
        .await;

        assert!(out.ends_with(":3"), "{out}");
    }

    #[tokio::test]
    async fn the_result_order_does_not_depend_on_the_disk() {
        // The walk runs on several threads now, so without the sort the same
        // search would answer in a different order run to run.
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["c.txt", "a.txt", "b.txt", "d.txt", "e.txt"] {
            std::fs::write(dir.path().join(name), "NEEDLE\n").expect("write");
        }

        let first = run(dir.path(), json!({ "pattern": "NEEDLE" })).await;
        let second = run(dir.path(), json!({ "pattern": "NEEDLE" })).await;

        assert_eq!(first, second);
        let names: Vec<&str> = first
            .lines()
            .filter_map(|line| line.rsplit('/').next())
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);
    }

    #[tokio::test]
    async fn a_glob_that_cannot_be_read_is_refused() {
        // The old code rebuilt the pattern per file and dropped the error, so
        // an unusable glob quietly matched every file instead.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "NEEDLE\n").expect("write");

        let ctx = ctx_for(dir.path(), false);
        let result = GrepTool
            .execute(json!({ "pattern": "NEEDLE", "glob": "a[" }), &ctx)
            .await;

        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.contains("Invalid glob"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_glob_filters_the_files_that_are_searched() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "NEEDLE\n").expect("write");
        std::fs::write(dir.path().join("b.rs"), "NEEDLE\n").expect("write");

        let out = run(dir.path(), json!({ "pattern": "NEEDLE", "glob": "*.rs" })).await;

        assert!(out.contains("b.rs"), "{out}");
        assert!(!out.contains("a.txt"), "{out}");
    }

    #[tokio::test]
    async fn head_limit_bounds_the_files_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            std::fs::write(dir.path().join(name), "NEEDLE\n").expect("write");
        }

        let out = run(dir.path(), json!({ "pattern": "NEEDLE", "head_limit": 2 })).await;

        assert_eq!(out.lines().count(), 2, "{out}");
    }

    #[tokio::test]
    async fn a_single_named_file_answers_without_its_own_path_on_every_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "first\nNEEDLE\n").expect("write");

        let ctx = ctx_for(dir.path(), false);
        let result = GrepTool
            .execute(
                json!({ "pattern": "NEEDLE", "path": file.display().to_string(), "output_mode": "content" }),
                &ctx,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "2:NEEDLE");
    }
}
