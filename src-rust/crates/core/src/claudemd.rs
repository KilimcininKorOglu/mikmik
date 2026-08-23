//! AGENTS.md hierarchical memory loading.
//! Mirrors src/utils/claudemd.ts (1,479 lines).
//!
//! Priority order: managed > user > project > local
//! Supports @include directives, YAML frontmatter, and mtime-based caching.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Memory file type / priority scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// `~/.config/mikmik/rules/*.md` — global managed policy.
    Managed,
    /// `~/.config/mikmik/AGENTS.md` — user-level memory.
    User,
    /// `{project_root}/AGENTS.md` — project-level memory.
    Project,
    /// `{project_root}/.mikmik/AGENTS.md` — local override.
    Local,
}

impl MemoryScope {
    /// Label used in the prompt header.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// Frontmatter parsed from a AGENTS.md file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryFrontmatter {
    #[serde(default)]
    pub memory_type: Option<String>,
    #[serde(default)]
    pub priority: Option<u32>,
    /// Where a conditional rule watches: `text`, `thinking`, `tool`,
    /// `tool:Edit`, `tool:Edit(*.rs)`, comma-separated.
    #[serde(default)]
    pub scope: Option<String>,
    /// One line saying what the rule is about.
    #[serde(default)]
    pub description: Option<String>,
    /// Regular expressions that wake the rule.
    ///
    /// A file that carries one is a **conditional rule**: it leaves the prompt
    /// and speaks only when one of these matches. A file without one keeps
    /// today's behaviour and is always in the prompt.
    #[serde(default)]
    pub condition: Vec<String>,
    /// File-path gate. A rule with globs only matches a call that names a file
    /// one of them covers.
    #[serde(default)]
    pub globs: Vec<String>,
    /// `remind` (default) or `block`.
    #[serde(default)]
    pub on_match: Option<String>,
    /// `once` (default), `always`, or a number of turns.
    #[serde(default)]
    pub repeat: Option<String>,
}

/// Loaded memory file with metadata.
#[derive(Debug, Clone)]
pub struct MemoryFileInfo {
    pub path: PathBuf,
    pub scope: MemoryScope,
    pub content: String,
    pub frontmatter: MemoryFrontmatter,
    pub mtime: Option<SystemTime>,
}

// ---------------------------------------------------------------------------
// YAML frontmatter parsing
// ---------------------------------------------------------------------------

/// Read one scalar YAML value.
///
/// A quoted value keeps neither its quotes nor its escapes. This matters for a
/// rule's regular expression: `"\\.unwrap\\(\\)"` is the YAML spelling of
/// `\.unwrap\(\)`, and handing the regex engine the doubled backslashes would
/// compile a pattern that matches a literal backslash.
///
/// Only the two escapes YAML gives a double-quoted scalar are read, plus the
/// one a single-quoted scalar has. Anything else is left alone, so an unknown
/// escape reaches the regex engine as written.
fn read_scalar(value: &str) -> String {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() < 2 {
        return trimmed.to_string();
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    let inner = &trimmed[1..trimmed.len() - 1];

    if first == b'\'' && last == b'\'' {
        // A single-quoted scalar has exactly one escape: a doubled quote.
        return inner.replace("''", "'");
    }
    if first != b'"' || last != b'"' {
        return trimmed.to_string();
    }

    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            // Not an escape this reader knows. Both characters are kept, so a
            // regex escape such as `\d` survives.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Split an inline list such as `*.rs, *.toml` on commas.
fn split_inline_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(read_scalar)
        .filter(|part| !part.is_empty())
        .collect()
}

/// Strip YAML frontmatter (--- ... ---) from content and parse it.
/// Returns (frontmatter, body_without_frontmatter).
///
/// A minimal reader, not a YAML parser: `key: value` on one line, and the block
/// list form where a key with no inline value is followed by `- item` lines.
/// Those two shapes are what a memory file and a rule file use.
pub fn parse_frontmatter(content: &str) -> (MemoryFrontmatter, &str) {
    if !content.starts_with("---") {
        return (MemoryFrontmatter::default(), content);
    }
    let after_first = &content[3..];
    let Some(end) = after_first.find("\n---") else {
        return (MemoryFrontmatter::default(), content);
    };
    let yaml = after_first[..end].trim();
    let body = &after_first[end + 4..];

    let mut fm = MemoryFrontmatter::default();
    let lines: Vec<&str> = yaml.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;
        // A stray list item belongs to no key; a comment belongs to nobody.
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        let Some((key, inline)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();

        let mut values: Vec<String> = Vec::new();
        if inline.trim().is_empty() {
            while index < lines.len() {
                let Some(item) = lines[index].trim().strip_prefix('-') else {
                    break;
                };
                index += 1;
                let item = read_scalar(item);
                if !item.is_empty() {
                    values.push(item);
                }
            }
        } else {
            values.push(read_scalar(inline));
        }

        match key {
            "memory_type" => fm.memory_type = values.into_iter().next(),
            "priority" => fm.priority = values.first().and_then(|v| v.parse().ok()),
            "scope" => fm.scope = values.into_iter().next(),
            "description" => fm.description = values.into_iter().next(),
            "on_match" => fm.on_match = values.into_iter().next(),
            "repeat" => fm.repeat = values.into_iter().next(),
            // Never split a condition on commas: a regular expression carries
            // them, `fmt\.Sprintf\("%s:%d", host` for one.
            "condition" => fm.condition = values,
            // A glob list also accepts the inline comma-separated form.
            "globs" => {
                fm.globs = match values.as_slice() {
                    [single] => split_inline_list(single),
                    _ => values,
                }
            }
            _ => {}
        }
    }

    (fm, body.trim_start_matches('\n'))
}

// ---------------------------------------------------------------------------
// @include directive expansion
// ---------------------------------------------------------------------------

/// Maximum @include nesting depth.
const MAX_INCLUDE_DEPTH: usize = 10;

/// Expand @include directives in content.
/// Circular references are detected via `visited` set.
pub fn expand_includes(
    content: &str,
    base_dir: &Path,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> String {
    if depth >= MAX_INCLUDE_DEPTH {
        return content.to_string();
    }

    let mut result = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(path_str) = trimmed.strip_prefix("@include ") {
            let path_str = path_str.trim();
            // Resolve relative to base_dir; expand ~ to home dir.
            let include_path = if path_str.starts_with('~') {
                dirs::home_dir().unwrap_or_default().join(&path_str[2..])
            } else if Path::new(path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                base_dir.join(path_str)
            };

            let canonical = include_path.canonicalize().unwrap_or(include_path.clone());
            if visited.contains(&canonical) {
                result.push_str(&format!(
                    "<!-- circular @include {} skipped -->\n",
                    path_str
                ));
                continue;
            }
            if let Ok(included) = std::fs::read_to_string(&include_path) {
                visited.insert(canonical);
                let expanded = expand_includes(
                    &included,
                    include_path.parent().unwrap_or(base_dir),
                    visited,
                    depth + 1,
                );
                result.push_str(&expanded);
                result.push('\n');
            } else {
                result.push_str(&format!("<!-- @include {} not found -->\n", path_str));
            }
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Loading API
// ---------------------------------------------------------------------------

/// Load a single memory file: strip frontmatter, expand `@include`s.
///
/// No size limit. A memory file is something the user wrote on purpose, and
/// silently dropping the second half of it is worse than a large prompt.
pub fn load_memory_file(path: &Path, scope: MemoryScope) -> Option<MemoryFileInfo> {
    let meta = std::fs::metadata(path).ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let mtime = meta.modified().ok();

    let (frontmatter, body) = parse_frontmatter(&raw);
    let mut visited = HashSet::new();
    visited.insert(path.canonicalize().unwrap_or(path.to_path_buf()));
    let content = expand_includes(
        body,
        path.parent().unwrap_or(Path::new(".")),
        &mut visited,
        0,
    );

    Some(MemoryFileInfo {
        path: path.to_path_buf(),
        scope,
        content,
        frontmatter,
        mtime,
    })
}

/// Which of the two memory filenames a session reads.
///
/// Two independent switches rather than one three-way choice: a project can
/// hold both files, and the user may want either one, the other, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryFilenames {
    pub agents_md: bool,
    pub claude_md: bool,
}

impl MemoryFilenames {
    /// Read the pair out of a `Config`.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            agents_md: config.effective_agents_md_enabled(),
            claude_md: config.effective_claude_md_enabled(),
        }
    }

    /// The filenames to try, in the order they reach the prompt.
    fn names(self) -> Vec<&'static str> {
        let mut names = Vec::with_capacity(2);
        if self.agents_md {
            names.push("AGENTS.md");
        }
        if self.claude_md {
            names.push("CLAUDE.md");
        }
        names
    }
}

impl Default for MemoryFilenames {
    fn default() -> Self {
        Self {
            agents_md: true,
            claude_md: false,
        }
    }
}

/// Load memory files from a directory for a given scope.
///
/// `AGENTS.md` comes first (universal standard), then `CLAUDE.md`
/// (Claude-specific additions). Either may be switched off or absent.
fn load_scope_files(
    dir: &Path,
    scope: MemoryScope,
    filenames: MemoryFilenames,
    files: &mut Vec<MemoryFileInfo>,
) {
    for name in filenames.names() {
        let path = dir.join(name);
        if path.exists() {
            if let Some(f) = load_memory_file(&path, scope) {
                files.push(f);
            }
        }
    }
}

/// Load every `*.md` in a rules directory, in filename order.
///
/// Filename order rather than directory order, so two machines that read the
/// same directory produce the same prompt.
fn load_rules_dir(dir: &Path, scope: MemoryScope, files: &mut Vec<MemoryFileInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    paths.sort();
    for path in paths {
        if let Some(f) = load_memory_file(&path, scope) {
            files.push(f);
        }
    }
}

/// Load all memory files for the given project root, in prompt order.
///
/// At each scope `AGENTS.md` is loaded first (universal standard), followed by
/// `CLAUDE.md` if present (Claude-specific context). Either or both may exist.
///
/// Returned list is ordered Managed → User → Project → Local, and within one
/// scope by `priority` ascending. Later entries reach the model later, so the
/// narrower scope wins where two files say different things.
///
/// `filenames` decides which of `AGENTS.md` and `CLAUDE.md` is read. The
/// Managed scope ignores it: those files carry neither name.
pub fn load_all_memory_files(
    project_root: &Path,
    filenames: MemoryFilenames,
) -> Vec<MemoryFileInfo> {
    let mut files = Vec::new();

    // 1. Managed: <mikmik home>/rules/*.md
    {
        let mikmik = crate::config::Settings::config_dir();
        load_rules_dir(&mikmik.join("rules"), MemoryScope::Managed, &mut files);

        // 2. User: <mikmik home>/AGENTS.md then <mikmik home>/CLAUDE.md
        load_scope_files(&mikmik, MemoryScope::User, filenames, &mut files);
    }

    // 3. Project: {project_root}/AGENTS.md then {project_root}/CLAUDE.md
    load_scope_files(project_root, MemoryScope::Project, filenames, &mut files);

    // 4. Local: {project_root}/.mikmik/AGENTS.md, then CLAUDE.md, then the
    //    project's own rules directory.
    let local_dir = project_root.join(".mikmik");
    load_scope_files(&local_dir, MemoryScope::Local, filenames, &mut files);
    load_rules_dir(&local_dir.join("rules"), MemoryScope::Local, &mut files);

    // Stable, and keyed on the scope first: the push order above is already
    // the scope order, so this only reorders within a scope. A file with no
    // `priority` sorts last there, which leaves an explicit priority in
    // charge, and the tie case keeps AGENTS.md ahead of CLAUDE.md and the
    // managed files in the alphabetical order they were read in.
    files.sort_by_key(|f| (f.scope, f.frontmatter.priority.unwrap_or(u32::MAX)));

    files
}

/// Whether this file is a conditional rule rather than plain memory.
///
/// A conditional rule waits for its `condition` to match something the model
/// writes. Putting it in the prompt as well would pay for it on every turn,
/// which is the cost the condition exists to avoid.
pub fn is_conditional_rule(file: &MemoryFileInfo) -> bool {
    !file.frontmatter.condition.is_empty()
}

/// Concatenate all memory file contents into a single system-prompt fragment.
///
/// Each file is headed with its scope and path. The model is told where an
/// instruction came from, which is what lets it say "your project's AGENTS.md
/// says X" rather than asserting X with no provenance.
///
/// A conditional rule is left out. See [`is_conditional_rule`].
pub fn build_memory_prompt(files: &[MemoryFileInfo]) -> String {
    files
        .iter()
        .filter(|f| !is_conditional_rule(f))
        .filter(|f| !f.content.trim().is_empty())
        .map(|f| {
            format!(
                "# Memory ({}, from {})\n{}",
                f.scope.as_str(),
                f.path.display(),
                f.content.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that redirect the config root.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point the config root at a temporary directory for the duration of a
    /// test, so `load_all_memory_files` does not read the real
    /// `~/.config/mikmik` for its Managed and User scopes.
    struct HomeGuard {
        saved: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl HomeGuard {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let saved = std::env::var_os("MIKMIK_HOME");
            std::env::set_var("MIKMIK_HOME", dir.path());
            Self { saved, _dir: dir }
        }

        fn path(&self) -> &Path {
            self._dir.path()
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var("MIKMIK_HOME", value),
                None => std::env::remove_var("MIKMIK_HOME"),
            }
        }
    }

    /// Both filenames on, which is what most of these tests exercise.
    fn both() -> MemoryFilenames {
        MemoryFilenames {
            agents_md: true,
            claude_md: true,
        }
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, content).expect("write");
    }

    #[test]
    fn parse_frontmatter_basic() {
        let content = "---\nmemory_type: project\npriority: 10\n---\nHello world";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.memory_type.as_deref(), Some("project"));
        assert_eq!(fm.priority, Some(10));
        assert_eq!(body.trim(), "Hello world");
    }

    #[test]
    fn parse_frontmatter_none() {
        let content = "No frontmatter here";
        let (fm, body) = parse_frontmatter(content);
        assert!(fm.memory_type.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn load_scope_prefers_agents_then_claude() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = HomeGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("AGENTS.md"), "agents content");
        write(&tmp.path().join("CLAUDE.md"), "claude content");

        let files = load_all_memory_files(tmp.path(), both());
        // Filter to just the project-scope files from our temp dir.
        let project: Vec<_> = files
            .iter()
            .filter(|f| f.path.starts_with(tmp.path()))
            .collect();
        assert_eq!(
            project.len(),
            2,
            "both AGENTS.md and CLAUDE.md should be loaded"
        );
        assert!(
            project[0].path.ends_with("AGENTS.md"),
            "AGENTS.md must come first"
        );
        assert!(
            project[1].path.ends_with("CLAUDE.md"),
            "CLAUDE.md must follow"
        );
    }

    #[test]
    fn load_scope_claudemd_only_fallback() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = HomeGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("CLAUDE.md"), "claude only");

        let files = load_all_memory_files(tmp.path(), both());
        let project: Vec<_> = files
            .iter()
            .filter(|f| f.path.starts_with(tmp.path()))
            .collect();
        assert_eq!(project.len(), 1);
        assert!(project[0].path.ends_with("CLAUDE.md"));
    }

    /// Each switch acts on its own filename, at every scope.
    #[test]
    fn each_filename_switch_acts_on_its_own_file() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = HomeGuard::new();
        let project = tempfile::tempdir().unwrap();

        write(&home.path().join("AGENTS.md"), "USER-AGENTS");
        write(&home.path().join("CLAUDE.md"), "USER-CLAUDE");
        write(&project.path().join("AGENTS.md"), "PROJECT-AGENTS");
        write(&project.path().join("CLAUDE.md"), "PROJECT-CLAUDE");

        let prompt = |names: MemoryFilenames| {
            build_memory_prompt(&load_all_memory_files(project.path(), names))
        };

        let agents_only = prompt(MemoryFilenames {
            agents_md: true,
            claude_md: false,
        });
        assert!(agents_only.contains("USER-AGENTS"));
        assert!(agents_only.contains("PROJECT-AGENTS"));
        assert!(!agents_only.contains("CLAUDE"), "{agents_only}");

        let claude_only = prompt(MemoryFilenames {
            agents_md: false,
            claude_md: true,
        });
        assert!(claude_only.contains("USER-CLAUDE"));
        assert!(claude_only.contains("PROJECT-CLAUDE"));
        assert!(!claude_only.contains("AGENTS"), "{claude_only}");

        let both_on = prompt(both());
        assert!(both_on.contains("USER-AGENTS") && both_on.contains("USER-CLAUDE"));

        let neither = prompt(MemoryFilenames {
            agents_md: false,
            claude_md: false,
        });
        assert_eq!(neither, "", "both switches off must read nothing");
    }

    /// A user who has said nothing keeps today's behaviour.
    #[test]
    fn the_default_reads_agents_md_and_not_claude_md() {
        let defaults = MemoryFilenames::default();
        assert!(defaults.agents_md);
        assert!(!defaults.claude_md);

        let config = crate::config::Config::default();
        assert_eq!(MemoryFilenames::from_config(&config), defaults);
    }

    /// Every documented location, in the documented order.
    #[test]
    fn all_four_scopes_are_read_in_order() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = HomeGuard::new();
        let project = tempfile::tempdir().unwrap();

        write(&home.path().join("rules/10-first.md"), "MANAGED-B");
        write(&home.path().join("rules/01-zeroth.md"), "MANAGED-A");
        write(&home.path().join("AGENTS.md"), "USER-AGENTS");
        write(&home.path().join("CLAUDE.md"), "USER-CLAUDE");
        write(&project.path().join("AGENTS.md"), "PROJECT-AGENTS");
        write(&project.path().join("CLAUDE.md"), "PROJECT-CLAUDE");
        write(&project.path().join(".mikmik/AGENTS.md"), "LOCAL-AGENTS");
        write(&project.path().join(".mikmik/CLAUDE.md"), "LOCAL-CLAUDE");

        let prompt = build_memory_prompt(&load_all_memory_files(project.path(), both()));
        let order: Vec<&str> = [
            "MANAGED-A",
            "MANAGED-B",
            "USER-AGENTS",
            "USER-CLAUDE",
            "PROJECT-AGENTS",
            "PROJECT-CLAUDE",
            "LOCAL-AGENTS",
            "LOCAL-CLAUDE",
        ]
        .into_iter()
        .collect();

        let mut cursor = 0;
        for marker in &order {
            let at = prompt[cursor..]
                .find(marker)
                .unwrap_or_else(|| panic!("{marker} missing or out of order:\n{prompt}"));
            cursor += at + marker.len();
        }
    }

    /// The docs promise the lower number is prepended first.
    #[test]
    fn priority_orders_files_inside_one_scope() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = HomeGuard::new();
        let project = tempfile::tempdir().unwrap();

        write(
            &home.path().join("rules/a.md"),
            "---\npriority: 10\n---\nTEN",
        );
        write(
            &home.path().join("rules/b.md"),
            "---\npriority: 5\n---\nFIVE",
        );
        write(&home.path().join("rules/c.md"), "NOPRIORITY");

        let prompt = build_memory_prompt(&load_all_memory_files(project.path(), both()));
        let five = prompt.find("FIVE").expect("FIVE missing");
        let ten = prompt.find("TEN").expect("TEN missing");
        let none = prompt.find("NOPRIORITY").expect("NOPRIORITY missing");

        assert!(five < ten, "priority 5 must precede priority 10:\n{prompt}");
        assert!(ten < none, "a file with no priority must sort last");
    }

    /// A large memory file is the user's decision, not something to truncate.
    #[test]
    fn a_large_file_and_a_large_include_pass_whole() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().unwrap();

        let filler = "x".repeat(60 * 1024);
        write(
            &project.path().join("big.md"),
            &format!("{filler}\nINCLUDE-END"),
        );
        write(
            &project.path().join("AGENTS.md"),
            &format!("{filler}\nFILE-END\n@include ./big.md\n"),
        );

        let prompt = build_memory_prompt(&load_all_memory_files(project.path(), both()));

        assert!(prompt.contains("FILE-END"), "a 60 KB file was cut");
        assert!(prompt.contains("INCLUDE-END"), "a 60 KB @include was cut");
    }

    #[test]
    fn frontmatter_is_stripped_from_the_prompt() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().unwrap();
        write(
            &project.path().join("AGENTS.md"),
            "---\nmemory_type: project\npriority: 3\n---\nBODY-TEXT\n",
        );

        let prompt = build_memory_prompt(&load_all_memory_files(project.path(), both()));

        assert!(prompt.contains("BODY-TEXT"));
        assert!(
            !prompt.contains("memory_type:"),
            "frontmatter leaked:\n{prompt}"
        );
        assert!(
            !prompt.contains("priority:"),
            "frontmatter leaked:\n{prompt}"
        );
    }

    #[test]
    fn a_conditional_rule_stays_out_of_the_prompt() {
        // The whole point of a condition: the rule costs nothing until the
        // model writes something that matches it.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = HomeGuard::new();
        let project = tempfile::tempdir().unwrap();
        write(
            &home.path().join("rules/no-unwrap.md"),
            "---\ncondition: \"\\\\.unwrap\\\\(\\\\)\"\n---\nDO-NOT-UNWRAP\n",
        );
        write(
            &home.path().join("rules/always.md"),
            "---\ndescription: plain memory\n---\nALWAYS-PRESENT\n",
        );

        let files = load_all_memory_files(project.path(), both());
        let prompt = build_memory_prompt(&files);

        assert!(prompt.contains("ALWAYS-PRESENT"));
        assert!(
            !prompt.contains("DO-NOT-UNWRAP"),
            "a conditional rule reached the prompt:\n{prompt}"
        );
        assert_eq!(files.iter().filter(|f| is_conditional_rule(f)).count(), 1);
    }

    #[test]
    fn a_projects_own_rules_directory_is_read() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().unwrap();
        write(
            &project.path().join(".mikmik/rules/house-style.md"),
            "---\ndescription: x\n---\nPROJECT-RULE-BODY\n",
        );

        let prompt = build_memory_prompt(&load_all_memory_files(project.path(), both()));
        assert!(prompt.contains("PROJECT-RULE-BODY"), "{prompt}");
    }

    #[test]
    fn a_quoted_value_loses_its_quotes_and_its_escapes() {
        // `"\\.unwrap\\(\\)"` is the YAML spelling of `\.unwrap\(\)`. Handing
        // the doubled backslashes to the regex engine compiles a pattern that
        // matches a literal backslash and nothing a rule cares about.
        let (fm, _) = parse_frontmatter("---\ncondition: \"\\\\.unwrap\\\\(\\\\)\"\n---\nbody\n");
        assert_eq!(fm.condition, vec![r"\.unwrap\(\)".to_string()]);
    }

    #[test]
    fn a_single_quoted_value_keeps_its_backslashes() {
        let (fm, _) = parse_frontmatter("---\ncondition: 'runtime\\.SetFinalizer'\n---\nbody\n");
        assert_eq!(fm.condition, vec![r"runtime\.SetFinalizer".to_string()]);
    }

    #[test]
    fn a_block_list_gives_one_entry_per_line() {
        let (fm, body) = parse_frontmatter(
            "---\ncondition:\n  - \"once_cell::\"\n  - \"OnceLock::new\"\nscope: \"tool:Edit\"\n---\nbody\n",
        );
        assert_eq!(fm.condition.len(), 2, "{:?}", fm.condition);
        assert_eq!(fm.condition[1], "OnceLock::new");
        assert_eq!(fm.scope.as_deref(), Some("tool:Edit"));
        assert_eq!(body.trim(), "body");
    }

    #[test]
    fn a_condition_holding_a_comma_is_one_regex() {
        // Splitting on commas would cut `fmt\.Sprintf\("%s:%d", host` in half
        // and compile two patterns that match nothing.
        let (fm, _) =
            parse_frontmatter("---\ncondition: 'fmt\\.Sprintf\\(\"%s:%d\", host'\n---\nbody\n");
        assert_eq!(fm.condition.len(), 1, "{:?}", fm.condition);
    }

    #[test]
    fn globs_accept_the_inline_comma_form() {
        let (fm, _) = parse_frontmatter("---\nglobs: *.rs, *.toml\n---\nbody\n");
        assert_eq!(fm.globs, vec!["*.rs".to_string(), "*.toml".to_string()]);
    }

    #[test]
    fn every_file_is_headed_with_its_scope_and_path() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = HomeGuard::new();
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join("AGENTS.md");
        write(&path, "BODY");

        let prompt = build_memory_prompt(&load_all_memory_files(project.path(), both()));

        assert!(
            prompt.contains(&format!("# Memory (project, from {})", path.display())),
            "no provenance header:\n{prompt}"
        );
    }

    #[test]
    fn expand_includes_circular() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.md");
        let b = tmp.path().join("b.md");
        std::fs::write(&a, "@include b.md\n").unwrap();
        std::fs::write(&b, "@include a.md\ncontent\n").unwrap();
        let result = expand_includes(
            "@include a.md\n",
            tmp.path(),
            &mut std::collections::HashSet::new(),
            0,
        );
        // Should not infinite-loop; circular reference comment present.
        assert!(result.contains("circular") || result.contains("content"));
    }
}
