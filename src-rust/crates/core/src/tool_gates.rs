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
    // Two conditions, because the setting alone is not enough: the session it
    // would start is a `node` process, and telling the model to script a
    // desktop through a runtime the machine does not have costs it a turn.
    if !config.computer_script_enabled || which::which("node").is_err() {
        withheld.push("computer_script");
    }
    // Two conditions again: the setting, and a browser the tool can actually
    // reach. With the setting on but no CDP url, no configured binary and no
    // Chrome on the PATH, the tool could only report that it found nothing to
    // drive, so it is withheld instead.
    if !config.browser_enabled || !browser_is_reachable(config) {
        withheld.push("browser");
    }
    // The image tools reach a provider, so with none configured they could only
    // report that there is nothing to ask.
    if config.resolve_api_key().is_none() {
        withheld.extend(["generate_image", "inspect_image"]);
    }
    withheld
}

/// Whether the `browser` tool has a browser to drive.
///
/// A configured endpoint or binary counts without probing: the user named it,
/// and a wrong value is their own report to read. Otherwise a Chrome or
/// Chromium on the PATH is what the tool would launch.
fn browser_is_reachable(config: &Config) -> bool {
    if config.browser_cdp_url.is_some() || config.browser_executable.is_some() {
        return true;
    }
    ["google-chrome", "chromium", "chromium-browser", "chrome"]
        .iter()
        .any(|name| which::which(name).is_ok())
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
    fn the_browser_stays_out_until_it_is_both_enabled_and_reachable() {
        // Off by default.
        assert!(unusable_tools(false, &Config::default(), &cwd()).contains(&"browser"));

        // On, but with nothing to drive: still withheld, so it never has to
        // report its own absence.
        let enabled_only = Config {
            browser_enabled: true,
            ..Default::default()
        };
        let reachable = enabled_only.browser_cdp_url.is_some()
            || enabled_only.browser_executable.is_some()
            || ["google-chrome", "chromium", "chromium-browser", "chrome"]
                .iter()
                .any(|name| which::which(name).is_ok());
        assert_eq!(
            !unusable_tools(false, &enabled_only, &cwd()).contains(&"browser"),
            reachable,
            "browser offering must track whether a browser is reachable"
        );

        // On and reachable through a configured endpoint: offered.
        let ready = Config {
            browser_enabled: true,
            browser_cdp_url: Some("http://127.0.0.1:9222".to_string()),
            ..Default::default()
        };
        assert!(!unusable_tools(false, &ready, &cwd()).contains(&"browser"));
    }

    #[test]
    fn the_image_tools_stay_out_until_a_provider_has_a_key() {
        // The selected provider disabled: no key resolves, so both are withheld.
        // Disabling short-circuits before any environment or stored credential,
        // which keeps this deterministic wherever it runs.
        let selected = Config::default().selected_provider_id().to_string();
        let disabled_provider: crate::config::ProviderConfig =
            serde_json::from_value(serde_json::json!({ "enabled": false }))
                .expect("a disabled provider config");
        let mut off = Config::default();
        off.provider_configs.insert(selected, disabled_provider);
        let withheld = unusable_tools(false, &off, &cwd());
        assert!(withheld.contains(&"generate_image"), "{withheld:?}");
        assert!(withheld.contains(&"inspect_image"), "{withheld:?}");

        // A key on the selected provider: both are offered.
        let configured = Config {
            api_key: Some("sk-test".to_string()),
            ..Default::default()
        };
        let withheld = unusable_tools(false, &configured, &cwd());
        assert!(!withheld.contains(&"generate_image"), "{withheld:?}");
        assert!(!withheld.contains(&"inspect_image"), "{withheld:?}");
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
