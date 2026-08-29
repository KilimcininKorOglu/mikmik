//! Agent discovery: load custom sub-agent definitions from markdown files on
//! disk, mirroring [`crate::skill_discovery`].
//!
//! An agent file is markdown with optional `---` frontmatter. The frontmatter
//! keys map onto [`AgentDefinition`] fields; the body after the frontmatter
//! becomes the agent's system-prompt prefix (`prompt`). Claude Code's
//! `.claude/agents/*.md` layout is accepted too, including its `tools:` list,
//! from which an `access` level is inferred when `access:` is absent.
//!
//! Sources, in increasing priority (a later source overrides an earlier one of
//! the same name), layered on top of the built-in and `settings.json` agents by
//! [`resolve_agents`]:
//!   1. Global `~/.claude/agents/`
//!   2. Global `~/.config/mikmik/agents/`
//!   3. Project `.claude/agents/`  — walk up from `cwd`, nearest wins
//!   4. Project `.mikmik/agents/`  — walk up from `cwd`, nearest wins

use crate::config::{default_agents, AgentDefinition, Settings};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The default `access` when a file names neither `access:` nor `tools:`.
const ACCESS_FULL: &str = "full";
/// Tools that a `search-only` agent keeps; the same set as the sub-agent
/// search allowlist. A Claude Code file whose `tools:` are all in this set is
/// treated as `search-only`.
const SEARCH_TOOLS: [&str; 5] = ["Grep", "Glob", "Read", "WebSearch", "WebFetch"];

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse one agent markdown file into a `(name, definition)` pair.
///
/// Returns `None` only when the file is empty after trimming. `name` comes from
/// the `name:` field, falling back to the file stem.
pub fn parse_agent_file(content: &str, path: &Path) -> Option<(String, AgentDefinition)> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    let (fields, body) = split_frontmatter(content);
    let name = fields
        .get("name")
        .cloned()
        .unwrap_or_else(|| file_stem(path));
    let def = AgentDefinition {
        description: fields.get("description").cloned(),
        model: fields.get("model").cloned(),
        temperature: fields.get("temperature").and_then(|v| v.parse().ok()),
        prompt: (!body.is_empty()).then_some(body),
        access: resolve_access(&fields),
        visible: fields.get("visible").map(|v| v != "false").unwrap_or(true),
        max_turns: fields
            .get("max_turns")
            .or_else(|| fields.get("maxTurns"))
            .and_then(|v| v.parse().ok()),
        color: fields.get("color").cloned(),
    };
    Some((name, def))
}

/// Split a `---` frontmatter block into `key: value` fields and the body after
/// it. A missing or unclosed block yields no fields and the whole content as
/// body. Line-based on purpose: the codebase parses frontmatter without
/// `serde_yaml`.
fn split_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let mut fields = HashMap::new();
    if let Some(after_open) = content.strip_prefix("---") {
        if let Some(close_pos) = after_open.find("\n---") {
            let frontmatter = &after_open[..close_pos];
            let rest = after_open[close_pos + 4..].trim_start_matches(['\r', '\n']);
            for line in frontmatter.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    let key = k.trim().to_string();
                    if !key.is_empty() {
                        let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
                        fields.insert(key, val);
                    }
                }
            }
            return (fields, rest.to_string());
        }
    }
    (fields, content.to_string())
}

/// Resolve the `access` level. An explicit `access:` field wins. Otherwise the
/// Claude Code `tools:` list is mapped: all-search-tools becomes `search-only`,
/// anything else (or an absent list) stays `full`, so an agent is never
/// silently given fewer tools than its file implies.
fn resolve_access(fields: &HashMap<String, String>) -> String {
    if let Some(access) = fields.get("access") {
        return access.clone();
    }
    match fields.get("tools") {
        Some(list) => infer_access_from_tools(list),
        None => ACCESS_FULL.to_string(),
    }
}

/// Map a comma-separated `tools:` list onto an `access` level.
fn infer_access_from_tools(list: &str) -> String {
    let tools: Vec<&str> = list
        .split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if !tools.is_empty() && tools.iter().all(|t| SEARCH_TOOLS.contains(t)) {
        "search-only".to_string()
    } else {
        ACCESS_FULL.to_string()
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string()
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Scan one directory for `*.md` agent files.
fn scan_agents_dir(dir: &Path) -> Vec<(String, AgentDefinition)> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::debug!(dir = %dir.display(), error = %err, "agent_discovery: read_dir failed");
            return out;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if let Some(agent) = parse_agent_file(&content, &path) {
                    out.push(agent);
                }
            }
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err, "agent_discovery: read failed");
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Top-level discovery
// ---------------------------------------------------------------------------

/// Every folder ancestor of `cwd`, farthest first, so appending each level's
/// agents leaves the nearest ancestor last and therefore winning.
fn ancestors_far_to_near(cwd: &Path) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut dir: &Path = cwd;
    loop {
        chain.push(dir.to_path_buf());
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => break,
        }
    }
    chain.reverse();
    chain
}

/// Discover folder agents in increasing priority order.
///
/// The result is ordered low → high, so folding it into a map with
/// last-writer-wins yields the documented precedence (global below project,
/// `.claude` below `.mikmik`, farther ancestor below nearer).
pub fn discover_agents(cwd: &Path) -> Vec<(String, AgentDefinition)> {
    let mut out: Vec<(String, AgentDefinition)> = Vec::new();

    // ---- Global (lowest): ~/.claude then ~/.config/mikmik ------------------
    if let Some(home) = dirs::home_dir() {
        out.extend(scan_agents_dir(&home.join(".claude").join("agents")));
    }
    out.extend(scan_agents_dir(&Settings::config_dir().join("agents")));

    // ---- Project: walk up, nearest ancestor wins ---------------------------
    for dir in ancestors_far_to_near(cwd) {
        out.extend(scan_agents_dir(&dir.join(".claude").join("agents")));
        out.extend(scan_agents_dir(&dir.join(".mikmik").join("agents")));
    }

    out
}

/// The full agent map the session uses: built-in defaults, then `settings.json`
/// agents, then folder agents, each layer overriding the previous by name.
pub fn resolve_agents(
    cwd: &Path,
    settings_agents: &HashMap<String, AgentDefinition>,
) -> HashMap<String, AgentDefinition> {
    let mut map = default_agents();
    for (name, def) in settings_agents {
        map.insert(name.clone(), def.clone());
    }
    for (name, def) in discover_agents(cwd) {
        map.insert(name, def);
    }
    map
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // ---- parse_agent_file ---------------------------------------------------

    #[test]
    fn parses_frontmatter_and_body() {
        let content = "---\nname: reviewer\ndescription: Reviews code\nmodel: anthropic/claude-haiku-4-5\naccess: read-only\nmax_turns: 12\ncolor: yellow\n---\n\nYou review changes.";
        let (name, def) = parse_agent_file(content, &PathBuf::from("reviewer.md")).unwrap();
        assert_eq!(name, "reviewer");
        assert_eq!(def.description.as_deref(), Some("Reviews code"));
        assert_eq!(def.model.as_deref(), Some("anthropic/claude-haiku-4-5"));
        assert_eq!(def.access, "read-only");
        assert_eq!(def.max_turns, Some(12));
        assert_eq!(def.color.as_deref(), Some("yellow"));
        assert_eq!(def.prompt.as_deref(), Some("You review changes."));
    }

    #[test]
    fn missing_name_falls_back_to_file_stem() {
        let content = "---\ndescription: No name\n---\nBody.";
        let (name, _) = parse_agent_file(content, &PathBuf::from("scout.md")).unwrap();
        assert_eq!(name, "scout");
    }

    #[test]
    fn no_frontmatter_makes_body_the_prompt() {
        let (name, def) = parse_agent_file("Just do it.", &PathBuf::from("doer.md")).unwrap();
        assert_eq!(name, "doer");
        assert_eq!(def.prompt.as_deref(), Some("Just do it."));
        assert_eq!(def.access, "full");
    }

    #[test]
    fn empty_file_returns_none() {
        assert!(parse_agent_file("   ", &PathBuf::from("empty.md")).is_none());
    }

    #[test]
    fn claude_tools_list_infers_search_only_access() {
        // A Claude Code file whose tools are all search tools maps to
        // search-only; a mixed list stays full so no tool is silently dropped.
        let search = "---\nname: finder\ntools: Read, Grep, Glob\n---\nFind things.";
        let (_, def) = parse_agent_file(search, &PathBuf::from("finder.md")).unwrap();
        assert_eq!(def.access, "search-only");

        let mixed = "---\nname: worker\ntools: Read, Bash\n---\nWork.";
        let (_, def) = parse_agent_file(mixed, &PathBuf::from("worker.md")).unwrap();
        assert_eq!(def.access, "full");
    }

    #[test]
    fn explicit_access_beats_tools_inference() {
        let content = "---\nname: x\naccess: full\ntools: Read, Grep\n---\nBody.";
        let (_, def) = parse_agent_file(content, &PathBuf::from("x.md")).unwrap();
        assert_eq!(def.access, "full");
    }

    // ---- scan_agents_dir ----------------------------------------------------

    #[test]
    fn scan_reads_only_markdown_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_file(tmp.path(), "b.md", "B body.");
        write_file(tmp.path(), "notes.txt", "ignored");
        let found = scan_agents_dir(tmp.path());
        assert_eq!(found.len(), 2);
        let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn scan_missing_dir_is_empty() {
        assert!(scan_agents_dir(Path::new("/nonexistent/agents/xyz")).is_empty());
    }

    // ---- resolve_agents precedence -----------------------------------------

    #[test]
    fn project_folder_overrides_settings_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".mikmik").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        // Override the built-in "build" agent from a project folder.
        write_file(
            &dir,
            "build.md",
            "---\nname: build\ndescription: repo build\n---\nRepo build agent.",
        );

        let mut settings = HashMap::new();
        settings.insert(
            "build".to_string(),
            AgentDefinition {
                description: Some("settings build".to_string()),
                ..Default::default()
            },
        );

        let map = resolve_agents(tmp.path(), &settings);
        // Project folder wins over both the built-in and the settings.json entry.
        assert_eq!(
            map.get("build").and_then(|d| d.description.as_deref()),
            Some("repo build")
        );
        // Untouched built-ins survive.
        assert!(map.contains_key("plan"));
        assert!(map.contains_key("explore"));
    }

    #[test]
    fn mikmik_folder_outranks_claude_folder_at_same_level() {
        let tmp = tempfile::tempdir().unwrap();
        let mikmik = tmp.path().join(".mikmik").join("agents");
        let claude = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&mikmik).unwrap();
        std::fs::create_dir_all(&claude).unwrap();
        write_file(
            &mikmik,
            "dup.md",
            "---\nname: dup\ndescription: from mikmik\n---\nBody.",
        );
        write_file(
            &claude,
            "dup.md",
            "---\nname: dup\ndescription: from claude\n---\nBody.",
        );

        let map = resolve_agents(tmp.path(), &HashMap::new());
        assert_eq!(
            map.get("dup").and_then(|d| d.description.as_deref()),
            Some("from mikmik")
        );
    }

    #[test]
    fn a_folder_agent_appears_in_the_map() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        write_file(
            &dir,
            "auditor.md",
            "---\nname: auditor\ndescription: Audits\ntools: Read, Grep\n---\nAudit the code.",
        );

        let map = resolve_agents(tmp.path(), &HashMap::new());
        let def = map.get("auditor").expect("auditor discovered");
        assert_eq!(def.access, "search-only");
        assert_eq!(def.prompt.as_deref(), Some("Audit the code."));
    }
}
