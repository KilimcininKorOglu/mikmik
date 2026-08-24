// `/workspace` — the organisation's configuration server, from inside a
// session.
//
// Read-only plus the two triggers a user can pull by hand. Signing in and out
// stays in `mikmik workspace`, because a password must not be typed into a
// prompt that the transcript records.

use super::*;
use async_trait::async_trait;

pub struct WorkspaceCommand;

#[async_trait]
impl SlashCommand for WorkspaceCommand {
    fn name(&self) -> &str {
        "workspace"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["ws"]
    }
    fn description(&self) -> &str {
        "Show the organisation's server: providers, policy and backup"
    }
    fn help(&self) -> &str {
        "Usage: /workspace [status|sync|pull]\n\n\
         The workspace server holds the providers your organisation assigns\n\
         you, the settings policy it enforces, and your own settings backup.\n\
         See docs/workspace-server.md.\n\n\
         Subcommands:\n\
         /workspace          Show the server, providers, policy and sync\n\
         /workspace sync     Upload this machine's settings now\n\
         /workspace pull     Take the providers and policy again now\n\n\
         Signing in and out is `mikmik workspace login` and `logout`. A\n\
         password does not belong in a prompt this transcript records."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let settings = match mikmik_core::config::Settings::load_sync() {
            Ok(settings) => settings,
            Err(error) => return CommandResult::Error(format!("Failed to load settings: {error}")),
        };

        let Some(workspace) = settings.workspace.clone() else {
            return CommandResult::Message(
                "No workspace server is configured.\n\n\
                 An organisation running one gives you an address and an account:\n  \
                 mikmik workspace login <url> --email <address>"
                    .to_string(),
            );
        };

        match args.trim() {
            "" | "status" => status(&settings, &workspace),
            "sync" => upload(&settings).await,
            "pull" => pull(&settings).await,
            other => CommandResult::Error(format!(
                "Unknown subcommand `{other}`. Try /workspace, /workspace sync or /workspace pull."
            )),
        }
    }
}

fn status(
    settings: &mikmik_core::config::Settings,
    workspace: &mikmik_core::config::WorkspaceSettings,
) -> CommandResult {
    use mikmik_core::workspace_server::{policy, providers};

    let signed_in = mikmik_core::AuthStore::load()
        .workspace_session(workspace.base())
        .is_some();

    // The company's providers listed apart from the user's own. Editing one of
    // these is wasted work: the next pull overwrites it.
    let managed = providers::managed_by(settings, workspace.base());
    let own: Vec<&String> = settings
        .providers
        .iter()
        .filter(|(_, config)| config.managed_by.is_none())
        .map(|(name, _)| name)
        .collect();

    let policy_line = match policy::load_cached(workspace.base()).settings {
        Some(policy) => {
            let keys = policy::decided_keys(&policy);
            if keys.is_empty() {
                "an empty policy".to_string()
            } else {
                keys.join(", ")
            }
        }
        None => "none".to_string(),
    };

    CommandResult::Message(format!(
        "Workspace Server\n\
         ════════════════\n\
         Server:   {server}\n\
         Session:  {session}\n\n\
         Company providers: {managed}\n\
         (these are refreshed from the server; editing one is undone by the next pull)\n\
         Your own providers: {own}\n\n\
         Policy decides: {policy_line}\n\
         (whatever the policy names, this machine cannot override)\n\n\
         Sync: on change {on_change}, at startup {at_startup}, timer {timer}\n\n\
         /workspace sync uploads now. /workspace pull takes the providers and\n\
         policy again. `mikmik workspace restore` brings a backup back.",
        server = workspace.base(),
        session = if signed_in {
            "signed in"
        } else {
            "none — run `mikmik workspace login`"
        },
        managed = list(&managed),
        own = list_refs(&own),
        on_change = on_off(workspace.sync.on_change),
        at_startup = on_off(workspace.sync.pull_at_startup),
        timer = match workspace.sync.interval_minutes {
            Some(minutes) => format!("every {minutes} minutes"),
            None => "off".to_string(),
        },
    ))
}

async fn upload(settings: &mikmik_core::config::Settings) -> CommandResult {
    use mikmik_core::workspace_server::{session, BackupWrite};

    let Some((_, client)) = session::connect(settings) else {
        return CommandResult::Error(
            "There is no live session. Run `mikmik workspace login`.".to_string(),
        );
    };
    match session::upload(&client).await {
        Ok(BackupWrite::Stored { version, .. }) => CommandResult::Message(format!(
            "Uploaded. The stored backup is now version {version}."
        )),
        // Never reported as a success: two machines hold different settings,
        // and only the person using them can say which is right.
        Ok(BackupWrite::Conflict { current_version }) => CommandResult::Error(format!(
            "Another machine wrote version {current_version} first, so nothing was uploaded. \
             Run `mikmik workspace restore` to see what it stored."
        )),
        Err(error) => CommandResult::Error(format!("The upload failed: {error}")),
    }
}

async fn pull(settings: &mikmik_core::config::Settings) -> CommandResult {
    use mikmik_core::workspace_server::{policy, session};

    let Some((_, client)) = session::connect(settings) else {
        return CommandResult::Error(
            "There is no live session. Run `mikmik workspace login`.".to_string(),
        );
    };

    let mut lines = Vec::new();
    match session::pull_providers(&client).await {
        Ok(applied) if applied.is_empty() => lines.push("Providers: nothing changed.".to_string()),
        Ok(applied) => {
            if !applied.written.is_empty() {
                lines.push(format!("Providers: {}", applied.written.join(", ")));
            }
            if !applied.withdrawn.is_empty() {
                lines.push(format!(
                    "No longer assigned: {}",
                    applied.withdrawn.join(", ")
                ));
            }
            for name in &applied.refused {
                lines.push(format!(
                    "`{name}` was offered and this machine already has one under that name. \
                     Yours was kept."
                ));
            }
        }
        Err(error) => lines.push(format!(
            "The providers could not be read ({error}); what is configured is untouched."
        )),
    }

    match policy::refresh(&client).await {
        Ok(cached) => match cached.settings.as_ref() {
            Some(stored) => lines.push(format!(
                "Policy decides: {}",
                policy::decided_keys(stored).join(", ")
            )),
            None => lines.push("The company sets no policy.".to_string()),
        },
        Err(error) => lines.push(format!("The policy could not be read ({error}).")),
    }

    // A policy fetched now reaches the next session, not this one: the layers
    // were merged when this session opened.
    lines.push("A changed policy applies from the next session.".to_string());
    CommandResult::Message(lines.join("\n"))
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

fn list_refs(names: &[&String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_help_sends_a_password_to_the_command_line() {
        // A password typed into a slash prompt lands in the transcript, which
        // is saved. The help has to point at the subcommand instead.
        let help = WorkspaceCommand.help();
        assert!(help.contains("mikmik workspace login"));
        assert!(
            !help.contains("/workspace login"),
            "the help offers to take a password in a prompt: {help}"
        );
    }

    #[test]
    fn nothing_is_listed_as_the_empty_string() {
        assert_eq!(list(&[]), "none");
        assert_eq!(list_refs(&[]), "none");
    }

    #[test]
    fn the_command_is_reachable_by_name_and_by_alias() {
        // A command written and not registered is a command nobody can run.
        let registered = crate::all_commands();
        for wanted in ["workspace", "ws"] {
            assert!(
                registered
                    .iter()
                    .any(|command| command.name() == wanted || command.aliases().contains(&wanted)),
                "`/{wanted}` resolves to nothing"
            );
        }
    }

    #[test]
    fn names_are_joined_as_written() {
        let names = vec!["b".to_string(), "a".to_string()];
        assert_eq!(list(&names), "b, a");
        assert_eq!(list_refs(&names.iter().collect::<Vec<_>>()), "b, a");
    }
}
