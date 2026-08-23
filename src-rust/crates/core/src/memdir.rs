//! Memory directory (memdir) system.
//!
//! Provides persistent, file-based memory across sessions.  Mirrors the
//! TypeScript modules under `src/memdir/`:
//!   - `memoryScan.ts`   → `scan_memory_dir`, `parse_frontmatter_quick`, `format_memory_manifest`
//!   - `memoryAge.ts`    → `memory_age_days`, `memory_freshness_text`, `memory_freshness_note`
//!   - `memdir.ts`       → `build_memory_prompt_content`, `load_memory_index`, `ensure_memory_dir_exists`
//!   - `paths.ts`        → `auto_memory_path`, `is_auto_memory_enabled`

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Memory type taxonomy
// ---------------------------------------------------------------------------

/// The four canonical memory types.
/// Matches the TypeScript `MemoryType` union in `memoryTypes.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// Information about the user's role, goals, and preferences.
    User,
    /// Guidance the user has given about how to approach work.
    Feedback,
    /// Information about ongoing work, goals, or incidents in the project.
    Project,
    /// Pointers to where information lives in external systems.
    Reference,
}

impl MemoryType {
    /// Parse a raw frontmatter value into a `MemoryType`.
    /// Returns `None` for missing or unrecognised values (legacy files degrade gracefully).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }

    /// Display as a lowercase string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

// ---------------------------------------------------------------------------
// Memory file metadata and content
// ---------------------------------------------------------------------------

/// Scanned metadata for a single memory file (without the full body).
/// Mirrors `MemoryHeader` in `memoryScan.ts`.
#[derive(Debug, Clone)]
pub struct MemoryFileMeta {
    /// Filename relative to the memory directory (e.g. `user_role.md`).
    pub filename: String,
    /// Absolute path to the file.
    pub path: PathBuf,
    /// `name:` frontmatter field.
    pub name: Option<String>,
    /// `description:` frontmatter field (used for relevance scoring).
    pub description: Option<String>,
    /// `type:` frontmatter field.
    pub memory_type: Option<MemoryType>,
    /// File modification time in seconds since UNIX epoch.
    pub modified_secs: u64,
}

/// A fully-loaded memory file including its body.
#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub meta: MemoryFileMeta,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Maximum number of memory files kept after sorting.
/// Matches `MAX_MEMORY_FILES` in `memoryScan.ts`.
const MAX_MEMORY_FILES: usize = 200;

/// Number of lines scanned for frontmatter.
/// Matches `FRONTMATTER_MAX_LINES` in `memoryScan.ts`.
const FRONTMATTER_MAX_LINES: usize = 30;

/// Scan a memory directory, returning metadata for all `.md` files
/// (excluding `MEMORY.md`), sorted newest-first, capped at `MAX_MEMORY_FILES`.
///
/// This is a synchronous scan used during system-prompt assembly.
/// Mirrors `scanMemoryFiles` in `memoryScan.ts` (async version; this is the
/// sync equivalent used at prompt-build time).
pub fn scan_memory_dir(dir: &Path) -> Vec<MemoryFileMeta> {
    let mut files: Vec<MemoryFileMeta> = Vec::new();

    if !dir.exists() {
        return files;
    }

    // Walk recursively using `walkdir`-style manual recursion to stay
    // dependency-free (only std).
    collect_md_files(dir, dir, &mut files);

    // Sort newest-first.
    files.sort_by_key(|b| std::cmp::Reverse(b.modified_secs));
    files.truncate(MAX_MEMORY_FILES);
    files
}

/// Recursively collect `.md` files (excluding `MEMORY.md`) from `current_dir`.
fn collect_md_files(base: &Path, current_dir: &Path, out: &mut Vec<MemoryFileMeta>) {
    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(base, &path, out);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if file_name == "MEMORY.md" {
                continue;
            }

            let modified_secs = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                .unwrap_or(0);

            let (name, description, memory_type) =
                if let Ok(content) = std::fs::read_to_string(&path) {
                    parse_frontmatter_quick(&content)
                } else {
                    (None, None, None)
                };

            // Relative path from the memory dir root.
            let relative = path
                .strip_prefix(base)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| file_name.clone());

            out.push(MemoryFileMeta {
                filename: relative,
                path,
                name,
                description,
                memory_type,
                modified_secs,
            });
        }
    }
}

/// Parse YAML frontmatter from the first `FRONTMATTER_MAX_LINES` lines without
/// a full YAML parser.  Returns `(name, description, memory_type)`.
///
/// Mirrors `parseFrontmatter` usage in `memoryScan.ts`.
pub fn parse_frontmatter_quick(
    content: &str,
) -> (Option<String>, Option<String>, Option<MemoryType>) {
    let mut name = None;
    let mut description = None;
    let mut memory_type = None;

    let lines: Vec<&str> = content.lines().take(FRONTMATTER_MAX_LINES).collect();

    // Frontmatter must start with `---`
    if lines.first().map(|l| l.trim() != "---").unwrap_or(true) {
        return (name, description, memory_type);
    }

    for line in &lines[1..] {
        if line.trim() == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(rest) = line.strip_prefix("description:") {
            description = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        } else if let Some(rest) = line.strip_prefix("type:") {
            memory_type = MemoryType::parse(rest.trim().trim_matches('"').trim_matches('\''));
        }
    }

    (name, description, memory_type)
}

/// Format memory headers as a text manifest: one entry per file with
/// `[type] filename (iso-timestamp, age): description`.
///
/// The age is carried alongside the timestamp rather than instead of it: the
/// timestamp is what a later reader can compare against a commit or a log,
/// and the age is what makes the model treat an old memory as suspect
/// without doing date arithmetic.
pub fn format_memory_manifest(memories: &[MemoryFileMeta]) -> String {
    memories
        .iter()
        .map(|m| {
            let tag = m
                .memory_type
                .as_ref()
                .map(|t| format!("[{}] ", t.as_str()))
                .unwrap_or_default();

            let when = format!(
                "{}, {}",
                format_unix_secs_iso(m.modified_secs),
                memory_age(m.modified_secs)
            );

            match &m.description {
                Some(desc) => format!("- {}{} ({}): {}", tag, m.filename, when, desc),
                None => format!("- {}{} ({})", tag, m.filename, when),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Minimal ISO-8601 formatter for a Unix timestamp (no external deps).
fn format_unix_secs_iso(secs: u64) -> String {
    // We use a very lightweight implementation to avoid pulling in chrono here
    // (chrono is already a workspace dep but we want this module to stay lean).
    // Accuracy to the day is sufficient for memory manifests.
    let days_since_epoch = secs / 86400;
    // Julian Day Number for 1970-01-01 is 2440588.
    let jdn = days_since_epoch as u32 + 2440588;
    let (y, m, d) = jdn_to_ymd(jdn);
    let hh = (secs % 86400) / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

/// Convert a Julian Day Number to (year, month, day).
fn jdn_to_ymd(jdn: u32) -> (u32, u32, u32) {
    let a = jdn + 32044;
    let b = (4 * a + 3) / 146097;
    let c = a - (146097 * b) / 4;
    let d = (4 * c + 3) / 1461;
    let e = c - (1461 * d) / 4;
    let m = (5 * e + 2) / 153;
    let day = e - (153 * m + 2) / 5 + 1;
    let month = m + 3 - 12 * (m / 10);
    let year = 100 * b + d - 4800 + m / 10;
    (year, month, day)
}

// ---------------------------------------------------------------------------
// Memory age / freshness
// ---------------------------------------------------------------------------

/// Days elapsed since `modified_secs`.  Floor-rounded; clamped to 0 for
/// future mtimes (clock skew).
///
/// Mirrors `memoryAgeDays` in `memoryAge.ts`.
pub fn memory_age_days(modified_secs: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    (now.saturating_sub(modified_secs)) / 86400
}

/// Human-readable age string.  Models are poor at date arithmetic — a raw
/// ISO timestamp does not trigger staleness reasoning the way "47 days ago" does.
///
/// Mirrors `memoryAge` in `memoryAge.ts`.
pub fn memory_age(modified_secs: u64) -> String {
    let d = memory_age_days(modified_secs);
    match d {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        n => format!("{} days ago", n),
    }
}

/// Plain-text staleness caveat for memories > 1 day old.
/// Returns an empty string for fresh memories (today / yesterday).
///
/// Mirrors `memoryFreshnessText` in `memoryAge.ts`.
pub fn memory_freshness_text(modified_secs: u64) -> String {
    let d = memory_age_days(modified_secs);
    if d <= 1 {
        return String::new();
    }
    format!(
        "This memory is {} days old. \
        Memories are point-in-time observations, not live state — \
        claims about code behavior or file:line citations may be outdated. \
        Verify against current code before asserting as fact.",
        d
    )
}

/// Per-memory staleness note wrapped in `<system-reminder>` tags.
/// Returns an empty string for memories ≤ 1 day old.
///
/// Mirrors `memoryFreshnessNote` in `memoryAge.ts`.
pub fn memory_freshness_note(modified_secs: u64) -> String {
    let text = memory_freshness_text(modified_secs);
    if text.is_empty() {
        return String::new();
    }
    format!("<system-reminder>{}</system-reminder>\n", text)
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Entrypoint filename within the memory directory.
pub const MEMORY_ENTRYPOINT: &str = "MEMORY.md";

/// Maximum number of lines loaded from `MEMORY.md`.
/// Matches `MAX_ENTRYPOINT_LINES` in `memdir.ts`.
pub const MAX_ENTRYPOINT_LINES: usize = 200;

/// Maximum bytes loaded from `MEMORY.md`.
/// Matches `MAX_ENTRYPOINT_BYTES` in `memdir.ts`.
pub const MAX_ENTRYPOINT_BYTES: usize = 25_000;

/// Compute the auto-memory directory path for a project root.
///
/// Resolution order:
/// 1. `MIKMIK_MEMORY_PATH_OVERRIDE` env var (full-path override).
/// 2. `<MIKMIK_REMOTE_MEMORY_DIR>/projects/<encoded-root>/memory/`
///    when `MIKMIK_REMOTE_MEMORY_DIR` is set.
/// 3. `~/.config/mikmik/projects/<encoded-root>/memory/` (default).
///
/// The project directory is the one
/// [`crate::session_storage::transcript_dir_in`] already derives, so a
/// project's transcripts and its memory live under one directory rather than
/// two siblings encoded differently.
///
/// Pass a root resolved with [`crate::session_storage::transcript_root_for`],
/// so a session started in a subdirectory sees the same memory as one started
/// at the repository root.
pub fn auto_memory_path(project_root: &Path) -> PathBuf {
    if let Ok(override_path) = std::env::var("MIKMIK_MEMORY_PATH_OVERRIDE") {
        if !override_path.is_empty() {
            return PathBuf::from(override_path);
        }
    }

    let memory_base = std::env::var("MIKMIK_REMOTE_MEMORY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::config::Settings::config_dir());

    crate::session_storage::transcript_dir_in(&memory_base, project_root).join("memory")
}

/// Whether the auto-memory system is enabled for this session.
///
/// Priority chain:
/// 1. `MIKMIK_DISABLE_AUTO_MEMORY` — truthy → OFF, falsy (but defined) → ON.
/// 2. `MIKMIK_SIMPLE` (--bare) → OFF.
/// 3. Remote mode without `MIKMIK_REMOTE_MEMORY_DIR` → OFF.
/// 4. `settings_enabled` parameter (from settings.json `autoMemoryEnabled`).
/// 5. Default: off.
///
/// Off by default because the feature writes to disk and adds a block to
/// every system prompt; an install that never asks for it keeps behaving the
/// way it did before.
pub fn is_auto_memory_enabled(settings_enabled: Option<bool>) -> bool {
    if let Ok(val) = std::env::var("MIKMIK_DISABLE_AUTO_MEMORY") {
        // Truthy values (non-empty, non-"0", non-"false") disable memory.
        match val.to_lowercase().as_str() {
            "" | "0" | "false" | "no" | "off" => return true, // defined-falsy → ON
            _ => return false,                                // truthy → OFF
        }
    }

    if std::env::var("MIKMIK_SIMPLE").is_ok() {
        return false;
    }

    if std::env::var("MIKMIK_REMOTE").is_ok() && std::env::var("MIKMIK_REMOTE_MEMORY_DIR").is_err()
    {
        return false;
    }

    settings_enabled.unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Index loading and truncation
// ---------------------------------------------------------------------------

/// Result of loading and (optionally) truncating the `MEMORY.md` entrypoint.
#[derive(Debug, Clone)]
pub struct EntrypointTruncation {
    pub content: String,
    pub line_count: usize,
    pub byte_count: usize,
    pub was_line_truncated: bool,
    pub was_byte_truncated: bool,
}

/// Truncate `MEMORY.md` content to `MAX_ENTRYPOINT_LINES` lines and
/// `MAX_ENTRYPOINT_BYTES` bytes, appending a warning when either cap fires.
///
/// Mirrors `truncateEntrypointContent` in `memdir.ts`.
pub fn truncate_entrypoint_content(raw: &str) -> EntrypointTruncation {
    let trimmed = raw.trim();
    let content_lines: Vec<&str> = trimmed.lines().collect();
    let line_count = content_lines.len();
    let byte_count = trimmed.len();

    let was_line_truncated = line_count > MAX_ENTRYPOINT_LINES;
    let was_byte_truncated = byte_count > MAX_ENTRYPOINT_BYTES;

    if !was_line_truncated && !was_byte_truncated {
        return EntrypointTruncation {
            content: trimmed.to_string(),
            line_count,
            byte_count,
            was_line_truncated: false,
            was_byte_truncated: false,
        };
    }

    let mut truncated = if was_line_truncated {
        content_lines[..MAX_ENTRYPOINT_LINES].join("\n")
    } else {
        trimmed.to_string()
    };

    if truncated.len() > MAX_ENTRYPOINT_BYTES {
        let cut_at = truncated[..MAX_ENTRYPOINT_BYTES]
            .rfind('\n')
            .unwrap_or(MAX_ENTRYPOINT_BYTES);
        truncated.truncate(cut_at);
    }

    let reason = match (was_line_truncated, was_byte_truncated) {
        (true, false) => format!("{} lines (limit: {})", line_count, MAX_ENTRYPOINT_LINES),
        (false, true) => format!(
            "{} bytes (limit: {}) — index entries are too long",
            byte_count, MAX_ENTRYPOINT_BYTES
        ),
        _ => format!("{} lines and {} bytes", line_count, byte_count),
    };

    truncated.push_str(&format!(
        "\n\n> WARNING: {} is {}. Only part of it was loaded. \
        Keep index entries to one line under ~200 chars; move detail into topic files.",
        MEMORY_ENTRYPOINT, reason
    ));

    EntrypointTruncation {
        content: truncated,
        line_count,
        byte_count,
        was_line_truncated,
        was_byte_truncated,
    }
}

/// Load and truncate the `MEMORY.md` index from `memory_dir`.
/// Returns `None` when the file does not exist or is empty.
///
/// Mirrors the entrypoint-reading path in `buildMemoryPrompt` / `loadMemoryPrompt`.
pub fn load_memory_index(memory_dir: &Path) -> Option<EntrypointTruncation> {
    let index_path = memory_dir.join(MEMORY_ENTRYPOINT);
    if !index_path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&index_path).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(truncate_entrypoint_content(&raw))
}

// ---------------------------------------------------------------------------
// System-prompt memory content builder
// ---------------------------------------------------------------------------

/// Build the memory content string to inject into the system prompt's
/// `<memory>` block.
///
/// Three parts: where the directory is, the `MEMORY.md` index, and a manifest
/// of the topic files. The path comes first because the model writes memories
/// with the ordinary `Write` and `Edit` tools, and without the path it has
/// nowhere to put one. The manifest lists what exists without spending the
/// tokens to load every body; a body is fetched on demand.
///
/// Returns an empty string for a directory with neither an index nor a topic
/// file, so an untouched installation adds no block at all.
///
/// Called during `build_system_prompt` → `SystemPromptOptions::memory_content`.
pub fn build_memory_prompt_content(memory_dir: &Path) -> String {
    let index = load_memory_index(memory_dir);
    let files = scan_memory_dir(memory_dir);

    if index.is_none() && files.is_empty() {
        return String::new();
    }

    let mut parts: Vec<String> = vec![format!(
        "Memory directory: {}\nFor one durable lesson, call the `Learn` tool; \
         it deduplicates and files it for you. For a whole document, write a \
         `.md` file there with `name`, `description` and `type` frontmatter, \
         and add one line for it to {}.",
        memory_dir.display(),
        MEMORY_ENTRYPOINT
    )];

    if let Some(index) = index {
        parts.push(format!(
            "## Memory Index ({})\n{}",
            MEMORY_ENTRYPOINT, index.content
        ));
    }

    if !files.is_empty() {
        parts.push(format!(
            "## Memory Files\n{}",
            format_memory_manifest(&files)
        ));
    }

    parts.join("\n\n")
}

/// Ensure the memory directory exists, creating it (and any parents) if needed.
/// Errors are silently swallowed (the Write tool will surface them if needed).
///
/// Mirrors `ensureMemoryDirExists` in `memdir.ts`.
pub fn ensure_memory_dir_exists(memory_dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(memory_dir) {
        // Log at debug level so --debug shows why, but don't abort.
        tracing::debug!(
            dir = %memory_dir.display(),
            error = %e,
            "ensureMemoryDirExists failed"
        );
    }
}

// ---------------------------------------------------------------------------
// Simple relevance search (no LLM side-query)
// ---------------------------------------------------------------------------

/// What a query word is worth in each part of a memory file.
///
/// The frontmatter outranks the body because it is what the author chose to
/// call the file, while the body may mention a word once in passing. The body
/// still has to count for something: a memory whose frontmatter omits the word
/// scored zero before, so a file holding the answer was never loaded.
const SCORE_NAME: f32 = 2.0;
const SCORE_DESCRIPTION: f32 = 1.0;
const SCORE_FILENAME: f32 = 0.5;
const SCORE_BODY: f32 = 0.25;

/// Find and load the most relevant memory files for a query using a
/// lightweight keyword score.
///
/// The full model-backed side-query lives elsewhere; this is the cheaper
/// fallback for contexts where an API call is not available.
///
/// Each candidate is read once, here. Scoring the body needs the text anyway,
/// and the same string is handed back as the loaded content, so a file that
/// wins is no longer read twice.
pub fn find_relevant_memories_simple(
    memory_dir: &Path,
    query: &str,
    max_files: usize,
) -> Vec<MemoryFile> {
    let metas = scan_memory_dir(memory_dir);
    let query_lower = query.to_lowercase();
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();

    if query_words.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(f32, MemoryFile)> = metas
        .into_iter()
        .filter_map(|meta| {
            let content = std::fs::read_to_string(&meta.path).ok()?;
            let desc = meta.description.as_deref().unwrap_or("").to_lowercase();
            let name = meta.name.as_deref().unwrap_or("").to_lowercase();
            let filename = meta.filename.to_lowercase();
            let body = content.to_lowercase();

            let score: f32 = query_words
                .iter()
                .map(|word| {
                    let in_name = if name.contains(*word) {
                        SCORE_NAME
                    } else {
                        0.0
                    };
                    let in_desc = if desc.contains(*word) {
                        SCORE_DESCRIPTION
                    } else {
                        0.0
                    };
                    let in_file = if filename.contains(*word) {
                        SCORE_FILENAME
                    } else {
                        0.0
                    };
                    // Counted once per word, however often it occurs: a file
                    // that repeats one term fifty times is not fifty times the
                    // answer, and letting it add up would let any long file
                    // outrank a short one that names the topic.
                    let in_body = if body.contains(*word) {
                        SCORE_BODY
                    } else {
                        0.0
                    };
                    in_name + in_desc + in_file + in_body
                })
                .sum();

            if score > 0.0 {
                Some((score, MemoryFile { meta, content }))
            } else {
                None
            }
        })
        .collect();

    // Sort highest score first.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    scored
        .into_iter()
        .take(max_files)
        .map(|(_, file)| file)
        .collect()
}

// ---------------------------------------------------------------------------
// Team memory helpers
// ---------------------------------------------------------------------------

/// Return the team-memory sub-directory path.
/// Mirrors `getTeamMemPath` in `teamMemPaths.ts`.
pub fn team_memory_path(auto_memory_dir: &Path) -> PathBuf {
    auto_memory_dir.join("team")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    // Helpers ----------------------------------------------------------------

    fn make_temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // ---- parse_frontmatter_quick -------------------------------------------

    #[test]
    fn test_parse_frontmatter_full() {
        let content = "---\nname: My Memory\ndescription: A test description\ntype: feedback\n---\n\nBody text.";
        let (name, desc, mt) = parse_frontmatter_quick(content);
        assert_eq!(name.as_deref(), Some("My Memory"));
        assert_eq!(desc.as_deref(), Some("A test description"));
        assert_eq!(mt, Some(MemoryType::Feedback));
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let content = "Just plain text.";
        let (name, desc, mt) = parse_frontmatter_quick(content);
        assert!(name.is_none());
        assert!(desc.is_none());
        assert!(mt.is_none());
    }

    #[test]
    fn test_parse_frontmatter_quoted_values() {
        let content = "---\nname: \"Quoted Name\"\ndescription: 'Single quoted'\ntype: user\n---";
        let (name, desc, mt) = parse_frontmatter_quick(content);
        assert_eq!(name.as_deref(), Some("Quoted Name"));
        assert_eq!(desc.as_deref(), Some("Single quoted"));
        assert_eq!(mt, Some(MemoryType::User));
    }

    #[test]
    fn test_parse_frontmatter_unknown_type() {
        let content = "---\ntype: unknown_type\n---";
        let (_, _, mt) = parse_frontmatter_quick(content);
        assert!(mt.is_none());
    }

    // ---- memory_age_days ---------------------------------------------------

    #[test]
    fn test_memory_age_today() {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(memory_age_days(now_secs), 0);
    }

    #[test]
    fn test_memory_age_one_day_ago() {
        let yesterday = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(86_400);
        assert_eq!(memory_age_days(yesterday), 1);
    }

    #[test]
    fn test_memory_age_future_clamps_to_zero() {
        let far_future = u64::MAX;
        assert_eq!(memory_age_days(far_future), 0);
    }

    // ---- memory_freshness_text ---------------------------------------------

    #[test]
    fn test_freshness_text_fresh() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(memory_freshness_text(now).is_empty());
    }

    #[test]
    fn test_freshness_text_stale() {
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(10 * 86_400); // 10 days ago
        let text = memory_freshness_text(old);
        assert!(text.contains("10 days old"));
        assert!(text.contains("point-in-time"));
    }

    // ---- memory_freshness_note ---------------------------------------------

    #[test]
    fn test_freshness_note_fresh_is_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(memory_freshness_note(now).is_empty());
    }

    #[test]
    fn test_freshness_note_stale_has_tags() {
        let old = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(5 * 86_400);
        let note = memory_freshness_note(old);
        assert!(note.contains("<system-reminder>"));
        assert!(note.contains("</system-reminder>"));
    }

    // ---- truncate_entrypoint_content ---------------------------------------

    #[test]
    fn test_truncate_no_truncation_needed() {
        let content = "line1\nline2\nline3";
        let result = truncate_entrypoint_content(content);
        assert!(!result.was_line_truncated);
        assert!(!result.was_byte_truncated);
        assert_eq!(result.content, content);
    }

    #[test]
    fn test_truncate_line_limit() {
        let content = (0..=MAX_ENTRYPOINT_LINES)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_entrypoint_content(&content);
        assert!(result.was_line_truncated);
        assert!(result.content.contains("WARNING"));
    }

    // ---- load_memory_index -------------------------------------------------

    #[test]
    fn test_load_memory_index_nonexistent() {
        let dir = make_temp_dir();
        assert!(load_memory_index(dir.path()).is_none());
    }

    #[test]
    fn test_load_memory_index_empty() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "   ");
        assert!(load_memory_index(dir.path()).is_none());
    }

    #[test]
    fn test_load_memory_index_with_content() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "- [test.md](test.md) — something");
        let result = load_memory_index(dir.path()).unwrap();
        assert!(result.content.contains("test.md"));
    }

    // ---- build_memory_prompt_content ---------------------------------------

    /// An installation that never wrote a memory must add no block, or every
    /// prompt carries an empty heading.
    #[test]
    fn an_empty_memory_dir_produces_no_block() {
        let dir = make_temp_dir();
        assert_eq!(build_memory_prompt_content(dir.path()), "");
        assert_eq!(build_memory_prompt_content(Path::new("/nonexistent")), "");
    }

    #[test]
    fn the_block_carries_the_path_the_index_and_the_manifest() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "- see user_role.md for the user");
        write_file(
            dir.path(),
            "user_role.md",
            "---\nname: Role\ndescription: The user is a data scientist\ntype: user\n---\nbody",
        );
        write_file(dir.path(), "notes/api.md", "---\nname: API\n---\nbody");

        let block = build_memory_prompt_content(dir.path());

        // The model writes memories with Write/Edit, so it needs the path.
        assert!(
            block.contains(&dir.path().display().to_string()),
            "the block does not say where the directory is:\n{block}"
        );
        assert!(block.contains("data scientist"), "index missing:\n{block}");
        assert!(block.contains("[user] user_role.md"), "{block}");
        // A file in a subdirectory keeps its relative path.
        assert!(block.contains("notes/api.md"), "{block}");
        // MEMORY.md is the index, never a manifest entry of its own.
        assert!(!block.contains("- MEMORY.md"), "{block}");
    }

    /// A directory with topic files but no index still describes itself.
    #[test]
    fn the_manifest_alone_is_enough_for_a_block() {
        let dir = make_temp_dir();
        write_file(dir.path(), "one.md", "---\nname: One\n---\nbody");

        let block = build_memory_prompt_content(dir.path());

        assert!(block.contains("one.md"), "{block}");
        assert!(!block.contains("Memory Index"), "{block}");
    }

    // ---- scan_memory_dir ---------------------------------------------------

    #[test]
    fn test_scan_excludes_memory_md() {
        let dir = make_temp_dir();
        write_file(dir.path(), "MEMORY.md", "# index");
        write_file(dir.path(), "user_role.md", "---\nname: Role\n---");
        let metas = scan_memory_dir(dir.path());
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].filename, "user_role.md");
    }

    #[test]
    fn test_scan_empty_dir() {
        let dir = make_temp_dir();
        assert!(scan_memory_dir(dir.path()).is_empty());
    }

    #[test]
    fn test_scan_nonexistent_dir() {
        let path = PathBuf::from("/tmp/nonexistent_memory_dir_cc_rust_test_xyz");
        assert!(scan_memory_dir(&path).is_empty());
    }

    // ---- format_memory_manifest --------------------------------------------

    #[test]
    fn test_format_memory_manifest_with_description() {
        let meta = MemoryFileMeta {
            filename: "user_role.md".to_string(),
            path: PathBuf::from("user_role.md"),
            name: Some("User Role".to_string()),
            description: Some("The user is a data scientist".to_string()),
            memory_type: Some(MemoryType::User),
            modified_secs: 0,
        };
        let manifest = format_memory_manifest(&[meta]);
        assert!(manifest.contains("[user]"));
        assert!(manifest.contains("user_role.md"));
        assert!(manifest.contains("data scientist"));
    }

    #[test]
    fn test_format_memory_manifest_no_description() {
        let meta = MemoryFileMeta {
            filename: "ref.md".to_string(),
            path: PathBuf::from("ref.md"),
            name: None,
            description: None,
            memory_type: None,
            modified_secs: 0,
        };
        let manifest = format_memory_manifest(&[meta]);
        assert!(manifest.contains("ref.md"));
        // No description, so no separator colon after the parenthesis.
        assert!(!manifest.contains("): "));
    }

    /// A raw timestamp does not make a model doubt a memory; an age does.
    #[test]
    fn every_manifest_line_carries_the_files_age() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let described = MemoryFileMeta {
            filename: "old.md".to_string(),
            path: PathBuf::from("old.md"),
            name: None,
            description: Some("something".to_string()),
            memory_type: None,
            modified_secs: now - 47 * 86400,
        };
        let bare = MemoryFileMeta {
            filename: "fresh.md".to_string(),
            description: None,
            modified_secs: now,
            ..described.clone()
        };

        let manifest = format_memory_manifest(&[described, bare]);

        assert!(
            manifest.contains("47 days ago"),
            "a described file lost its age:\n{manifest}"
        );
        assert!(
            manifest.contains("today"),
            "a file with no description lost its age:\n{manifest}"
        );
    }

    // ---- MemoryType --------------------------------------------------------

    #[test]
    fn test_memory_type_roundtrip() {
        for (s, expected) in [
            ("user", MemoryType::User),
            ("feedback", MemoryType::Feedback),
            ("project", MemoryType::Project),
            ("reference", MemoryType::Reference),
        ] {
            let parsed = MemoryType::parse(s).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn test_memory_type_unknown_returns_none() {
        assert!(MemoryType::parse("bogus").is_none());
    }

    // ---- auto_memory_path --------------------------------------------------

    /// Memory and transcripts share one project directory. Two encodings of
    /// the same project root would leave a user with two sibling directories
    /// and no way to tell which belongs to which checkout.
    #[test]
    fn the_memory_dir_sits_inside_the_project_transcript_dir() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let base = make_temp_dir();
        let _guard = EnvGuard::set(
            "MIKMIK_REMOTE_MEMORY_DIR",
            base.path().to_str().unwrap_or(""),
        );
        let _no_override = EnvGuard::unset("MIKMIK_MEMORY_PATH_OVERRIDE");

        let project = Path::new("/home/user/project");
        let memory = auto_memory_path(project);
        let transcripts = crate::session_storage::transcript_dir_in(base.path(), project);

        assert_eq!(memory.parent(), Some(transcripts.as_path()));
        assert_eq!(memory.file_name().and_then(|n| n.to_str()), Some("memory"));
    }

    #[test]
    fn the_full_path_override_wins_over_the_base() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set("MIKMIK_MEMORY_PATH_OVERRIDE", "/tmp/elsewhere");

        assert_eq!(
            auto_memory_path(Path::new("/home/user/project")),
            PathBuf::from("/tmp/elsewhere")
        );
    }

    // ---- is_auto_memory_enabled -------------------------------------------

    /// Serialises the tests that mutate process-global environment variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Sets or clears one environment variable and restores it on drop.
    struct EnvGuard {
        key: &'static str,
        saved: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let saved = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, saved }
        }

        fn unset(key: &'static str) -> Self {
            let saved = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Clear every env var the priority chain reads, so the ambient
    /// environment cannot decide the answer.
    fn clear_memory_env() -> Vec<EnvGuard> {
        vec![
            EnvGuard::unset("MIKMIK_DISABLE_AUTO_MEMORY"),
            EnvGuard::unset("MIKMIK_SIMPLE"),
            EnvGuard::unset("MIKMIK_REMOTE"),
        ]
    }

    #[test]
    fn auto_memory_is_off_until_the_setting_asks_for_it() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = clear_memory_env();

        assert!(!is_auto_memory_enabled(None), "an unset key turned it on");
        assert!(!is_auto_memory_enabled(Some(false)));
        assert!(is_auto_memory_enabled(Some(true)));
    }

    #[test]
    fn the_disable_env_var_beats_the_setting_in_both_directions() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = clear_memory_env();

        {
            let _off = EnvGuard::set("MIKMIK_DISABLE_AUTO_MEMORY", "1");
            assert!(!is_auto_memory_enabled(Some(true)));
        }
        {
            // Defined-but-falsy is an explicit "do not disable", so it turns
            // the feature on even where the setting is absent.
            let _on = EnvGuard::set("MIKMIK_DISABLE_AUTO_MEMORY", "0");
            assert!(is_auto_memory_enabled(None));
        }
    }

    #[test]
    fn bare_mode_silences_auto_memory() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = clear_memory_env();
        let _bare = EnvGuard::set("MIKMIK_SIMPLE", "1");

        assert!(!is_auto_memory_enabled(Some(true)));
    }

    // ---- find_relevant_memories_simple -------------------------------------

    /// The filenames a search returned, in rank order.
    fn ranked(dir: &Path, query: &str) -> Vec<String> {
        find_relevant_memories_simple(dir, query, 10)
            .into_iter()
            .map(|file| file.meta.filename)
            .collect()
    }

    /// The case the frontmatter-only score got wrong: the answer is in the
    /// body, and nothing in the frontmatter mentions the word.
    #[test]
    fn a_word_that_appears_only_in_the_body_still_finds_the_file() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "deploy.md",
            "---\nname: Deploy\ndescription: How releases reach production\n---\n\
             The release workflow refuses a tag lower than the highest existing one.",
        );

        assert_eq!(ranked(dir.path(), "workflow"), vec!["deploy.md"]);
    }

    /// A body hit must not outrank a file that names the topic, or a passing
    /// mention would push the right answer out of a three-file result.
    #[test]
    fn the_frontmatter_still_outranks_the_body() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "named.md",
            "---\nname: Cargo\ndescription: The build\n---\nNothing else here.",
        );
        write_file(
            dir.path(),
            "mentions.md",
            "---\nname: Editors\ndescription: Which editor opens what\n---\n\
             The cargo command runs from src-rust.",
        );

        assert_eq!(ranked(dir.path(), "cargo"), vec!["named.md", "mentions.md"]);
    }

    /// Body score is counted once per word. A file that repeats one term is
    /// not more relevant for repeating it, and letting it add up would let any
    /// long file outrank a short one that names the topic.
    #[test]
    fn repeating_a_word_does_not_raise_the_score() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "once.md",
            "---\nname: One\ndescription: d\n---\nrelay",
        );
        write_file(
            dir.path(),
            "many.md",
            "---\nname: Two\ndescription: d\n---\nrelay relay relay relay relay relay",
        );

        // Both score the same, so the newest-first order from the scan decides
        // and both come back. What matters is that neither is dropped and the
        // repeated one has not climbed above a frontmatter match.
        let found = ranked(dir.path(), "relay");
        assert_eq!(found.len(), 2, "{found:?}");

        write_file(
            dir.path(),
            "titled.md",
            "---\nname: Relay\ndescription: d\n---\nnothing",
        );
        assert_eq!(
            ranked(dir.path(), "relay").first().map(String::as_str),
            Some("titled.md"),
            "a repeated body word beat a name match"
        );
    }

    #[test]
    fn a_file_that_matches_nowhere_is_still_left_out() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "deploy.md",
            "---\nname: Deploy\ndescription: releases\n---\ntag then wait",
        );

        assert!(ranked(dir.path(), "kubernetes").is_empty());
    }

    /// The body is the loaded content, so the single read has to serve both.
    #[test]
    fn the_body_that_was_scored_is_the_body_that_comes_back() {
        let dir = make_temp_dir();
        write_file(
            dir.path(),
            "deploy.md",
            "---\nname: Deploy\ndescription: releases\n---\nTag, then wait for CI.",
        );

        let found = find_relevant_memories_simple(dir.path(), "releases", 10);
        assert_eq!(found.len(), 1);
        assert!(found[0].content.contains("Tag, then wait for CI."));
    }
}
