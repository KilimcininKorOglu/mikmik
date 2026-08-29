//! Hook discovery: load event hooks from a `hooks/` folder on disk.
//!
//! The live hook system runs `config.hooks: HashMap<HookEvent, Vec<HookEntry>>`
//! (see [`crate::hooks::run_hooks`]). Hooks reach it today only from the
//! `settings.json` `hooks` key. This module adds a folder source: every
//! `*.json`/`*.jsonc` file in a `hooks/` directory, each carrying the same flat
//! shape as the settings `hooks` value, so a user can lift the object out of
//! `settings.json` into a file unchanged.
//!
//! Two folders feed it, wired in [`crate::config::Settings::load_hierarchical_detailed`]:
//!   - Global `~/.config/mikmik/hooks/` — the user's own, applied ungated.
//!   - Project `<root>/.mikmik/hooks/` — repo-controlled, folded into the
//!     project settings so it passes the same project-trust gate as
//!     `settings.json` hooks (a shell command is an RCE surface).

use crate::config::{HookEntry, HookEvent};
use std::collections::HashMap;
use std::path::Path;

/// Event hooks keyed by event, the same shape as `config.hooks`.
pub type HookMap = HashMap<HookEvent, Vec<HookEntry>>;

/// Merge `src` into `dst`, appending each event's entries.
fn extend_hook_map(dst: &mut HookMap, src: HookMap) {
    for (event, entries) in src {
        dst.entry(event).or_default().extend(entries);
    }
}

/// Load every `*.json`/`*.jsonc` file in `dir` and merge them into one map.
///
/// Files are read in sorted path order so the merge is deterministic. A file
/// that cannot be read or parsed is logged and skipped, never fatal.
pub fn load_hook_dir(dir: &Path) -> HookMap {
    let mut out = HookMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("json") | Some("jsonc")
            )
        })
        .collect();
    files.sort();

    for path in files {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "hook_discovery: read failed");
                continue;
            }
        };
        let stripped = crate::config::strip_jsonc_comments(&content);
        match serde_json::from_str::<HookMap>(&stripped) {
            Ok(map) => extend_hook_map(&mut out, map),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "hook_discovery: parse failed");
            }
        }
    }
    out
}

/// Load the project hooks folder: the nearest `.mikmik/hooks/` directory
/// walking up from `cwd`.
///
/// Mirrors the walk in `Settings::find_project_settings`, and skips the global
/// config directory's own `hooks/` so a session run from inside the config
/// directory does not read its global hooks twice.
pub fn load_project_hooks(cwd: &Path) -> HookMap {
    let global_hooks = crate::config::Settings::config_dir().join("hooks");
    let mut dir = cwd;
    loop {
        let hooks_dir = dir.join(".mikmik").join("hooks");
        if hooks_dir != global_hooks && hooks_dir.is_dir() {
            return load_hook_dir(&hooks_dir);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return HookMap::new(),
        }
    }
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

    #[test]
    fn a_flat_hooks_file_parses_into_the_event_map() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(
            tmp.path(),
            "hooks.json",
            r#"{ "PreToolUse": [{ "command": "echo pre", "tool_filter": "Bash", "blocking": true }],
                "Stop": [{ "command": "echo stop" }] }"#,
        );
        let map = load_hook_dir(tmp.path());
        assert_eq!(map.get(&HookEvent::PreToolUse).map(Vec::len), Some(1));
        let pre = &map[&HookEvent::PreToolUse][0];
        assert_eq!(pre.command, "echo pre");
        assert_eq!(pre.tool_filter.as_deref(), Some("Bash"));
        assert!(pre.blocking);
        assert!(map.contains_key(&HookEvent::Stop));
    }

    #[test]
    fn several_files_merge_per_event() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(
            tmp.path(),
            "a.json",
            r#"{ "Stop": [{ "command": "one" }] }"#,
        );
        write_file(
            tmp.path(),
            "b.json",
            r#"{ "Stop": [{ "command": "two" }] }"#,
        );
        let map = load_hook_dir(tmp.path());
        let stop = map.get(&HookEvent::Stop).expect("stop hooks");
        assert_eq!(stop.len(), 2);
        let commands: Vec<&str> = stop.iter().map(|h| h.command.as_str()).collect();
        assert!(commands.contains(&"one"));
        assert!(commands.contains(&"two"));
    }

    #[test]
    fn a_jsonc_file_with_comments_parses() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(
            tmp.path(),
            "hooks.jsonc",
            "{ // lead comment\n \"Stop\": [{ \"command\": \"x\" }] }",
        );
        assert!(load_hook_dir(tmp.path()).contains_key(&HookEvent::Stop));
    }

    #[test]
    fn a_non_json_file_is_ignored_and_bad_json_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(tmp.path(), "notes.txt", "not hooks");
        write_file(tmp.path(), "broken.json", "{ not valid");
        write_file(tmp.path(), "ok.json", r#"{ "Stop": [{ "command": "x" }] }"#);
        let map = load_hook_dir(tmp.path());
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&HookEvent::Stop));
    }

    #[test]
    fn a_missing_directory_is_empty() {
        assert!(load_hook_dir(Path::new("/nonexistent/hooks/xyz")).is_empty());
    }

    #[test]
    fn project_hooks_are_found_walking_up() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_dir = tmp.path().join(".mikmik").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        write_file(
            &hooks_dir,
            "hooks.json",
            r#"{ "Stop": [{ "command": "x" }] }"#,
        );
        // A nested cwd still finds the project hooks above it.
        let nested = tmp.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(load_project_hooks(&nested).contains_key(&HookEvent::Stop));
    }
}
