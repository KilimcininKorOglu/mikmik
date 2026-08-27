//! The set of tools a session runs with.
//!
//! One builder, shared by every front end. A session started from an editor
//! reads the same `settings.json` as one started from a terminal, so it must
//! end up with the same tools; building the roster in each front end is how
//! they drifted apart.

use std::sync::Arc;

use mikmik_tools::Tool;
use tracing::debug;

/// Built-in tools, the sub-agent tool, the advisor when a model backs it, the
/// ACP bridge when agents are configured, and every tool the connected MCP
/// servers offer.
///
/// Takes the whole config rather than the individual fields it gates on: two
/// of the tools are already conditional and threading one more derived value
/// through six call sites buys nothing.
pub fn build_tool_roster(
    mcp_manager: Option<Arc<mikmik_mcp::McpManager>>,
    config: &mikmik_core::Config,
) -> Arc<Vec<Box<dyn Tool>>> {
    let mut tools: Vec<Box<dyn Tool>> = mikmik_tools::all_tools();
    tools.push(Box::new(crate::AgentTool));

    // Offer the advisor only when a model backs it and the mode asks the model
    // to consult one, so a session without either pays neither the tool schema
    // nor the system-prompt guideline for it. `advisorMode: runtime` runs a
    // watcher instead, which the model does not call.
    if config
        .advisor_model
        .as_deref()
        .is_some_and(|model| !model.trim().is_empty())
        && config.effective_advisor_mode().offers_tool()
    {
        tools.push(Box::new(mikmik_tools::AdvisorTool));
    }

    // Same reasoning for the ACP bridge: without a configured agent the tool
    // could only ever answer "nothing is configured", so offering it would
    // spend schema tokens to advertise a dead end.
    if !config.acp_agents.is_empty() {
        tools.push(Box::new(mikmik_tools::AcpAgentTool));
    }

    // And again for memory: with the feature off there is no directory to
    // search, so the tool could only ever answer "nothing is there". `Learn`
    // rides the same gate, because it writes into the directory `Memory` reads.
    if mikmik_core::memdir::is_auto_memory_enabled(config.auto_memory_enabled) {
        tools.push(Box::new(mikmik_tools::MemoryTool));
        tools.push(Box::new(mikmik_tools::LearnTool));
    }

    if let Some(manager) = &mcp_manager {
        tools.extend(mikmik_tools::mcp_tools(manager));
        debug!(total_tools = tools.len(), "MCP tools registered");
    }

    // The manager plans and delegates, so it holds nothing that does the work
    // itself. Its system prompt and the documentation both said so already;
    // only the roster did not.
    if config
        .managed_agents
        .as_ref()
        .is_some_and(|managed| managed.enabled)
    {
        let before = tools.len();
        tools.retain(|t| !MANAGER_DENIED_TOOLS.contains(&t.name()));
        debug!(
            removed = before - tools.len(),
            "managed mode: withheld the tools that do the work"
        );
    }

    apply_roster_filter(&mut tools, config);

    Arc::new(tools)
}

/// Cut the roster down to what `--allowed-tools` and `--disallowed-tools` name.
///
/// These decide which tools exist for the session, not whether a call is
/// approved; that is `permission_rules`. Withholding a tool costs nothing at
/// run time and saves its schema on every turn.
///
/// Deny wins, matching `PermissionManager::evaluate`. Runs after managed mode
/// has taken its tools out, so naming one here cannot put it back.
fn apply_roster_filter(tools: &mut Vec<Box<dyn Tool>>, config: &mikmik_core::Config) {
    if config.allowed_tools.is_empty() && config.disallowed_tools.is_empty() {
        return;
    }

    // A name that matches nothing is reported rather than ignored. Silently
    // keeping every tool after a typo tells the user they restricted the
    // session when they did not.
    let present: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    for name in config.allowed_tools.iter().chain(&config.disallowed_tools) {
        if !present.contains(&name.as_str()) {
            debug!(tool = %name, "roster filter names a tool that is not registered");
        }
    }

    let before = tools.len();
    tools.retain(|t| {
        let name = t.name();
        if config.disallowed_tools.iter().any(|d| d == name) {
            return false;
        }
        config.allowed_tools.is_empty() || config.allowed_tools.iter().any(|a| a == name)
    });
    debug!(
        removed = before - tools.len(),
        kept = tools.len(),
        "roster filter applied"
    );
}

/// What a manager does not get while managed mode is on.
///
/// Reading, searching, delegating and tracking all stay: the manager still has
/// to understand the work before it can split it up. The user's own shell is
/// untouched, because the TUI reaches `PtyBashTool` directly rather than
/// through this roster.
const MANAGER_DENIED_TOOLS: &[&str] = &[
    mikmik_core::constants::TOOL_NAME_BASH,
    mikmik_core::constants::TOOL_NAME_POWERSHELL,
    mikmik_core::constants::TOOL_NAME_REPL,
    mikmik_core::constants::TOOL_NAME_COMPUTER_USE,
    mikmik_core::constants::TOOL_NAME_FILE_WRITE,
    mikmik_core::constants::TOOL_NAME_FILE_EDIT,
    mikmik_core::constants::TOOL_NAME_BATCH_EDIT,
    mikmik_core::constants::TOOL_NAME_NOTEBOOK_EDIT,
    mikmik_core::constants::TOOL_NAME_APPLY_PATCH,
];

#[cfg(test)]
mod tests {
    use super::*;

    use mikmik_core::Config;

    fn names(tools: &[Box<dyn Tool>]) -> Vec<&str> {
        tools.iter().map(|t| t.name()).collect()
    }

    fn with_advisor(model: Option<&str>) -> Config {
        Config {
            advisor_model: model.map(str::to_string),
            ..Default::default()
        }
    }

    fn managed(enabled: bool) -> Config {
        Config {
            managed_agents: Some(mikmik_core::ManagedAgentConfig {
                enabled,
                manager_model: "anthropic/claude-opus-4-6".to_string(),
                executor_model: "anthropic/claude-sonnet-4-6".to_string(),
                executor_max_turns: 10,
                max_concurrent_executors: 4,
                total_budget_usd: None,
                preset_name: None,
                executor_isolation: false,
            }),
            ..Default::default()
        }
    }

    /// The prompt and the documentation both claimed the manager does not
    /// execute tools. Only the roster decides that.
    #[test]
    fn a_manager_holds_nothing_that_does_the_work() {
        let tools = build_tool_roster(None, &managed(true));
        let names = names(&tools);

        for denied in MANAGER_DENIED_TOOLS {
            assert!(!names.contains(denied), "{denied} reached the manager");
        }
    }

    #[test]
    fn a_manager_still_reads_searches_and_delegates() {
        let tools = build_tool_roster(None, &managed(true));
        let names = names(&tools);

        assert!(names.contains(&"Read"), "{names:?}");
        assert!(names.contains(&"Grep"), "{names:?}");
        assert!(names.contains(&"Agent"), "{names:?}");
        assert!(names.contains(&"TodoWrite"), "{names:?}");
    }

    fn filtered(allowed: &[&str], denied: &[&str]) -> Config {
        Config {
            allowed_tools: allowed.iter().map(|s| s.to_string()).collect(),
            disallowed_tools: denied.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_denied_tool_is_not_offered() {
        let tools = build_tool_roster(None, &filtered(&[], &["Bash"]));
        let names = names(&tools);

        assert!(!names.contains(&"Bash"), "{names:?}");
        assert!(names.contains(&"Read"), "{names:?}");
    }

    #[test]
    fn an_allow_list_offers_exactly_what_it_names() {
        let tools = build_tool_roster(None, &filtered(&["Read", "Grep"], &[]));
        let mut names = names(&tools);
        names.sort_unstable();

        assert_eq!(names, vec!["Grep", "Read"]);
    }

    #[test]
    fn deny_wins_over_allow_for_the_same_tool() {
        // Matches how `PermissionManager::evaluate` resolves a contradiction.
        let tools = build_tool_roster(None, &filtered(&["Read"], &["Read"]));

        assert!(names(&tools).is_empty(), "{:?}", names(&tools));
    }

    #[test]
    fn a_name_that_matches_nothing_leaves_the_roster_alone() {
        let unfiltered = build_tool_roster(None, &Config::default()).len();
        let tools = build_tool_roster(None, &filtered(&[], &["NoSuchTool"]));

        assert_eq!(tools.len(), unfiltered);
    }

    #[test]
    fn an_allow_list_cannot_return_a_tool_managed_mode_took_away() {
        let mut config = managed(true);
        config.allowed_tools = vec!["Bash".to_string(), "Read".to_string()];
        let tools = build_tool_roster(None, &config);

        assert!(!names(&tools).contains(&"Bash"), "{:?}", names(&tools));
        assert!(names(&tools).contains(&"Read"), "{:?}", names(&tools));
    }

    #[test]
    fn a_configured_but_inactive_managed_mode_changes_nothing() {
        let tools = build_tool_roster(None, &managed(false));
        let names = names(&tools);

        assert!(names.contains(&"Bash"), "{names:?}");
        assert!(names.contains(&"Write"), "{names:?}");
    }

    #[test]
    fn a_session_always_gets_the_built_ins_and_the_sub_agent_tool() {
        let config = Config::default();
        let tools = build_tool_roster(None, &config);
        let names = names(&tools);

        assert!(names.contains(&"Bash"), "{names:?}");
        assert!(names.contains(&"Read"), "{names:?}");
        assert!(names.contains(&"Agent"), "{names:?}");
    }

    #[test]
    fn the_advisor_is_offered_only_when_a_model_backs_it() {
        assert!(!names(&build_tool_roster(None, &with_advisor(None))).contains(&"Advisor"));
        assert!(!names(&build_tool_roster(None, &with_advisor(Some("   ")))).contains(&"Advisor"));
        assert!(names(&build_tool_roster(
            None,
            &with_advisor(Some("claude-haiku-4-5"))
        ))
        .contains(&"Advisor"));
    }

    #[test]
    fn the_mode_decides_whether_the_model_may_ask() {
        let with_mode = |mode: &str| {
            let mut config = with_advisor(Some("claude-haiku-4-5"));
            config.advisor_mode = Some(mode.to_string());
            names(&build_tool_roster(None, &config)).contains(&"Advisor")
        };

        assert!(with_mode("tool"), "the default keeps today's behaviour");
        assert!(with_mode("both"));
        assert!(!with_mode("runtime"), "a watcher is not asked, it watches");
        assert!(!with_mode("off"));
    }

    #[test]
    fn no_agent_is_ever_offered_the_tool_it_would_advise_itself_with() {
        let mut config = with_advisor(Some("claude-haiku-4-5"));
        for mode in mikmik_core::advisor::AdvisorMode::ALL {
            config.advisor_mode = Some(mode.to_string());
            assert!(
                !names(&build_tool_roster(None, &config)).contains(&"Advise"),
                "Advise belongs to a watcher's own roster, never a primary's ({mode})"
            );
        }
    }

    /// Serialises the tests that clear the memory environment variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Clear the env vars that can override the setting, so the ambient
    /// environment cannot decide this test's answer.
    struct MemoryEnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl MemoryEnvGuard {
        fn cleared() -> Self {
            let keys = [
                "MIKMIK_DISABLE_AUTO_MEMORY",
                "MIKMIK_SIMPLE",
                "MIKMIK_REMOTE",
            ];
            let saved = keys
                .iter()
                .map(|key| {
                    let previous = std::env::var_os(key);
                    std::env::remove_var(key);
                    (*key, previous)
                })
                .collect();
            Self { saved }
        }
    }

    impl Drop for MemoryEnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn the_memory_tools_are_offered_only_when_the_directory_is_kept() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = MemoryEnvGuard::cleared();

        // Off by default, so a session that never asked pays no schema tokens.
        let bare = build_tool_roster(None, &Config::default());
        let off = names(&bare);
        assert!(!off.contains(&"Memory"), "{off:?}");
        assert!(!off.contains(&"Learn"), "{off:?}");

        let config = Config {
            auto_memory_enabled: Some(true),
            ..Default::default()
        };
        let kept = build_tool_roster(None, &config);
        let on = names(&kept);
        assert!(on.contains(&"Memory"), "{on:?}");
        // `Learn` writes into the directory `Memory` reads, so one gate has to
        // decide both. Offering a writer with no reader, or the other way
        // round, would leave half the feature advertised.
        assert!(on.contains(&"Learn"), "{on:?}");
    }

    #[test]
    fn the_acp_bridge_is_offered_only_when_an_agent_is_configured() {
        let bare = Config::default();
        assert!(!names(&build_tool_roster(None, &bare)).contains(&"AcpAgent"));

        let mut configured = Config::default();
        configured.acp_agents.insert(
            "cursor".to_string(),
            mikmik_core::AcpAgentConfig {
                command: "agent".to_string(),
                args: vec!["--force".to_string(), "acp".to_string()],
                env: Default::default(),
            },
        );
        assert!(names(&build_tool_roster(None, &configured)).contains(&"AcpAgent"));
    }
}
