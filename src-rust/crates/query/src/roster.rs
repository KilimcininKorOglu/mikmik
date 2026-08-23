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
    // search, so the tool could only ever answer "nothing is there".
    if mikmik_core::memdir::is_auto_memory_enabled(config.auto_memory_enabled) {
        tools.push(Box::new(mikmik_tools::MemoryTool));
    }

    if let Some(manager) = &mcp_manager {
        tools.extend(mikmik_tools::mcp_tools(manager));
        debug!(total_tools = tools.len(), "MCP tools registered");
    }

    Arc::new(tools)
}

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
    fn the_memory_tool_is_offered_only_when_the_directory_is_kept() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = MemoryEnvGuard::cleared();

        // Off by default, so a session that never asked pays no schema tokens.
        assert!(!names(&build_tool_roster(None, &Config::default())).contains(&"Memory"));

        let on = Config {
            auto_memory_enabled: Some(true),
            ..Default::default()
        };
        assert!(names(&build_tool_roster(None, &on)).contains(&"Memory"));
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
