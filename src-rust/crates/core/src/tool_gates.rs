//! Which tools a session offers, decided in one place.
//!
//! Two surfaces have to agree: the roster the model is handed, and the catalogue
//! `ToolSearch` searches. When they disagreed the search advertised a tool the
//! roster had withheld, the model called it, and the call answered `Unknown
//! tool` — the wasted turn the gating exists to prevent. Both now read the
//! functions here.

use std::path::Path;

use crate::config::Config;
use crate::constants::{
    TOOL_NAME_COMPUTER_USE, TOOL_NAME_REPL, TOOL_NAME_TEAM_CREATE, TOOL_NAME_TEAM_DELETE,
};

/// The tools this session has no use for, by name.
///
/// Two kinds are collected. A tool the *machine or the directory* cannot
/// support is withheld because it could only report its own absence: there are
/// no MCP resources without a manager, no worktree outside a repository, and no
/// language server for a tree none of them recognises. A tool behind a setting
/// is withheld because the user has not asked for it.
pub fn unusable_tools(has_mcp: bool, config: &Config, cwd: &Path) -> Vec<&'static str> {
    let mut withheld = Vec::new();
    if !has_mcp {
        withheld.extend(["ListMcpResources", "ReadMcpResource", "mcp__auth"]);
    }
    if crate::snapshot::shadow::find_repo_root(cwd).is_none() {
        withheld.extend(["EnterWorktree", "ExitWorktree"]);
    }
    if !any_language_server_reachable(config, cwd) {
        withheld.push("LSP");
    }
    if !config.teams_enabled {
        withheld.extend([TOOL_NAME_TEAM_CREATE, TOOL_NAME_TEAM_DELETE]);
    }
    if !config.cron_enabled {
        withheld.extend(["CronCreate", "CronDelete", "CronList"]);
    }
    if !config.repl_enabled {
        withheld.push(TOOL_NAME_REPL);
    }
    if !config.computer_use_enabled {
        withheld.push(TOOL_NAME_COMPUTER_USE);
    }
    withheld
}

/// Whether `name` survives `--allowed-tools` and `--disallowed-tools`.
///
/// Deny wins, matching `PermissionManager::evaluate`. An empty allow list means
/// "everything", so a session that named neither keeps its whole roster.
pub fn passes_roster_filter(name: &str, config: &Config) -> bool {
    if config.disallowed_tools.iter().any(|d| d == name) {
        return false;
    }
    config.allowed_tools.is_empty() || config.allowed_tools.iter().any(|a| a == name)
}

/// Whether the session offers `name` at all.
///
/// The single question both the roster and the search catalogue ask.
pub fn tool_is_offered(name: &str, has_mcp: bool, config: &Config, cwd: &Path) -> bool {
    passes_roster_filter(name, config) && !unusable_tools(has_mcp, config, cwd).contains(&name)
}

/// Whether the session carries `Memory` and `Learn`.
///
/// They are added on a condition rather than withheld on one, so they are not
/// in `unusable_tools`. `Learn` rides the same gate as `Memory` because it
/// writes into the directory `Memory` reads.
pub fn offers_memory_tools(config: &Config) -> bool {
    crate::memdir::is_auto_memory_enabled(config.auto_memory_enabled)
}

/// Whether any language server this tree would use is installed.
///
/// A configured server counts without probing: the user named it, and a missing
/// binary is their own report to read, which they cannot get if the tool that
/// would report it is withheld. `detect_servers` already requires both a root
/// marker and a resolvable binary, so no probe is repeated here.
fn any_language_server_reachable(config: &Config, cwd: &Path) -> bool {
    if !config.lsp_servers.is_empty() {
        return true;
    }
    config.effective_lsp_auto_detect() && !crate::lsp::detect_servers(cwd).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn a_gated_tool_is_unusable_by_default() {
        let withheld = unusable_tools(false, &Config::default(), &cwd());

        for name in ["CronList", "TeamCreate", "REPL", "ListMcpResources"] {
            assert!(withheld.contains(&name), "{name}");
        }
    }

    #[test]
    fn turning_the_setting_on_makes_it_usable() {
        let config = Config {
            cron_enabled: true,
            ..Default::default()
        };
        let withheld = unusable_tools(false, &config, &cwd());

        assert!(!withheld.contains(&"CronList"));
    }

    #[test]
    fn the_deny_list_wins_over_the_allow_list() {
        let config = Config {
            allowed_tools: vec!["Read".to_string()],
            disallowed_tools: vec!["Read".to_string()],
            ..Default::default()
        };

        assert!(!passes_roster_filter("Read", &config));
    }

    #[test]
    fn an_empty_filter_keeps_everything() {
        assert!(passes_roster_filter("Read", &Config::default()));
    }

    #[test]
    fn one_question_answers_for_both_surfaces() {
        // The roster and the search catalogue must not disagree: a search that
        // advertised a withheld tool would cost the turn the gate saves.
        let config = Config::default();

        assert!(tool_is_offered("Read", false, &config, &cwd()));
        assert!(!tool_is_offered("CronList", false, &config, &cwd()));
    }
}
