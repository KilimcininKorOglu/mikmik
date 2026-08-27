// `/permissions` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct PermissionsCommand;

// ---- /permissions --------------------------------------------------------

#[async_trait]
impl SlashCommand for PermissionsCommand {
    fn name(&self) -> &str {
        "permissions"
    }
    fn description(&self) -> &str {
        "View or change tool permission settings"
    }
    fn help(&self) -> &str {
        "Usage: /permissions [set <mode>|allow <tool>|deny <tool>|reset]\n\n\
         Modes: default, accept-edits, bypass-permissions, plan\n\n\
         Examples:\n\
           /permissions                    — show current permissions\n\
           /permissions set accept-edits   — auto-accept file edits\n\
           /permissions allow Bash         — allow a specific tool\n\
           /permissions deny Write         — deny a specific tool\n\
           /permissions reset              — clear overrides"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        if args.is_empty() {
            // Read from disk rather than from `ctx.config`, because the rules
            // live on `Settings` and this command writes them there; a copy
            // taken at startup would not show what was just set.
            let rules = mikmik_core::Settings::load_sync()
                .map(|s| s.permission_rules)
                .unwrap_or_default();
            let named = |action: mikmik_core::permissions::PermissionAction| -> Vec<String> {
                rules
                    .iter()
                    .filter(|rule| rule.action == action)
                    .map(|rule| match (&rule.tool_name, &rule.path_pattern) {
                        (Some(tool), Some(path)) => format!("{tool} on {path}"),
                        (Some(tool), None) => tool.clone(),
                        (None, Some(path)) => format!("any tool on {path}"),
                        (None, None) => "any tool".to_string(),
                    })
                    .collect()
            };
            let allowed = named(mikmik_core::permissions::PermissionAction::Allow);
            let denied = named(mikmik_core::permissions::PermissionAction::Deny);
            let allowed_display = if allowed.is_empty() {
                "(all tools allowed)".to_string()
            } else {
                allowed.join(", ")
            };
            let denied_display = if denied.is_empty() {
                "(none)".to_string()
            } else {
                denied.join(", ")
            };
            return CommandResult::Message(format!(
                "Permission Settings\n\
                 ───────────────────\n\
                 Mode:          {:?}\n\
                 Allowed tools: {}\n\
                 Denied tools:  {}\n\n\
                 Use /permissions set <mode> to change the permission mode.\n\
                 Use /permissions allow|deny <tool> to override individual tools.\n\
                 Use /permissions reset to clear all overrides.",
                ctx.config.permission_mode, allowed_display, denied_display,
            ));
        }

        let mut parts = args.splitn(2, ' ');
        let sub = parts.next().unwrap_or("").trim();
        let arg = parts.next().unwrap_or("").trim();

        match sub {
            "set" => {
                let mode = match arg.to_lowercase().as_str() {
                    "default" => mikmik_core::config::PermissionMode::Default,
                    "accept-edits" | "accept_edits" => {
                        mikmik_core::config::PermissionMode::AcceptEdits
                    }
                    "bypass-permissions" | "bypass_permissions" => {
                        mikmik_core::config::PermissionMode::BypassPermissions
                    }
                    "plan" => mikmik_core::config::PermissionMode::Plan,
                    _ => {
                        return CommandResult::Error(
                            "Mode must be: default, accept-edits, bypass-permissions, or plan"
                                .to_string(),
                        )
                    }
                };
                let mut new_config = ctx.config.clone();
                new_config.permission_mode = mode;
                if let Err(e) = save_settings_mutation(|s| s.config.permission_mode = mode) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    format!("Permission mode set to {:?}.", mode),
                )
            }
            // Both verdicts go to `permission_rules`, which is the only list
            // `PermissionManager::evaluate` reads. They used to be written to
            // `config.allowed_tools` / `config.disallowed_tools`, which nothing
            // consulted, so a denied tool kept running.
            "allow" | "deny" => {
                if arg.is_empty() {
                    return CommandResult::Error(format!("Usage: /permissions {sub} <tool>"));
                }
                let tool = arg.to_string();
                let action = if sub == "allow" {
                    mikmik_core::permissions::PermissionAction::Allow
                } else {
                    mikmik_core::permissions::PermissionAction::Deny
                };
                if let Err(e) = save_settings_mutation(|s| s.set_tool_rule(&tool, action.clone())) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                let verb = if sub == "allow" { "Allowed" } else { "Denied" };
                CommandResult::ConfigChangeMessage(
                    ctx.config.clone(),
                    format!("{verb} tool: {tool}"),
                )
            }
            "reset" => {
                let mut new_config = ctx.config.clone();
                new_config.permission_mode = mikmik_core::config::PermissionMode::Default;
                if let Err(e) = save_settings_mutation(|s| {
                    s.permission_rules.clear();
                    s.config.permission_mode = mikmik_core::config::PermissionMode::Default;
                }) {
                    return CommandResult::Error(format!("Failed to save: {}", e));
                }
                CommandResult::ConfigChangeMessage(
                    new_config,
                    "Permissions reset to defaults.".to_string(),
                )
            }
            other => CommandResult::Error(format!(
                "Unknown subcommand '{}'. Use: /permissions [set|allow|deny|reset]",
                other
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::permissions::{PermissionDecision, PermissionManager};

    /// `MIKMIK_HOME` is process-global, so the tests that redirect it run one
    /// at a time and put it back afterwards.
    static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct HomeGuard {
        saved: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn pointing_at(dir: &std::path::Path) -> Self {
            let saved = std::env::var_os("MIKMIK_HOME");
            std::env::set_var("MIKMIK_HOME", dir);
            Self { saved }
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

    fn ctx() -> CommandContext {
        CommandContext {
            context_window: 200_000,
            context_used_tokens: 0,
            config: mikmik_core::Config::default(),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("."),
            session_id: "test-session".to_string(),
            session_title: None,
            effort_level: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            interactive: true,
            active_agent: None,
        }
    }

    /// What a session started right now would decide about `tool`.
    ///
    /// Built from the file rather than from the command's return value,
    /// because the file is what the next session reads and what
    /// `reload_persistent_rules` re-reads.
    fn verdict(tool: &str) -> PermissionDecision {
        let settings = mikmik_core::Settings::load_sync().expect("settings load");
        let manager = PermissionManager::new(settings.config.permission_mode, &settings);
        manager.evaluate(tool, "test", None, None, &[])
    }

    #[tokio::test]
    async fn a_denied_tool_is_actually_denied() {
        // This is the whole point: the command used to write a list nothing
        // read, so the tool it "denied" kept running.
        let _lock = HOME_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeGuard::pointing_at(dir.path());

        PermissionsCommand.execute("deny Write", &mut ctx()).await;

        assert!(matches!(verdict("Write"), PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn allowing_a_tool_takes_back_the_denial() {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeGuard::pointing_at(dir.path());

        PermissionsCommand.execute("deny Write", &mut ctx()).await;
        PermissionsCommand.execute("allow Write", &mut ctx()).await;

        assert!(!matches!(verdict("Write"), PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn reset_clears_the_rules() {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeGuard::pointing_at(dir.path());

        PermissionsCommand.execute("deny Write", &mut ctx()).await;
        PermissionsCommand.execute("reset", &mut ctx()).await;

        assert!(mikmik_core::Settings::load_sync()
            .expect("settings load")
            .permission_rules
            .is_empty());
        assert!(!matches!(verdict("Write"), PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn the_listing_names_the_rules_that_decide() {
        let _lock = HOME_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeGuard::pointing_at(dir.path());

        PermissionsCommand.execute("deny Write", &mut ctx()).await;
        PermissionsCommand.execute("allow Bash", &mut ctx()).await;
        let listing = match PermissionsCommand.execute("", &mut ctx()).await {
            CommandResult::Message(text) => text,
            other => panic!("expected a Message, got {other:?}"),
        };

        assert!(listing.contains("Allowed tools: Bash"), "{listing}");
        assert!(listing.contains("Denied tools:  Write"), "{listing}");
    }
}
