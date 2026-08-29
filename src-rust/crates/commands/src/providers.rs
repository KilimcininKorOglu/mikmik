// Provider/agent commands: `/providers`, `/connect`, `/agent`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ProvidersCommand;
pub struct ConnectCommand;
pub struct AgentCommand;

// ---- /providers -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ProvidersCommand {
    fn name(&self) -> &str {
        "providers"
    }
    fn description(&self) -> &str {
        "List available AI providers, or re-read an account's model list"
    }
    fn help(&self) -> &str {
        "Usage: /providers [sync [<account>] [--force]]\n\n\
         Without arguments, lists all providers registered in the model\n\
         registry with their model counts, context windows, and pricing.\n\n\
         `sync` asks each configured account what models it serves and\n\
         rewrites its stored list, so a model the endpoint added later\n\
         becomes usable without editing settings.json. Name an account to\n\
         sync only that one.\n\n\
         Limits you wrote yourself in `modelOverrides` are kept. Pass\n\
         `--force` to replace them with the endpoint's own numbers."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        if let Some((first, rest)) = tokens.split_first() {
            if *first != "sync" {
                return CommandResult::Error(format!(
                    "Unknown argument '{first}'. Usage: /providers [sync [<account>] [--force]]"
                ));
            }
            let force = rest.contains(&"--force");
            let accounts: Vec<String> = rest
                .iter()
                .filter(|token| !token.starts_with("--"))
                .map(|token| (*token).to_string())
                .collect();
            return CommandResult::SyncAccountModels { accounts, force };
        }

        self.list_providers()
    }
}

impl ProvidersCommand {
    fn list_providers(&self) -> CommandResult {
        let registry = mikmik_api::ModelRegistry::new();
        let all = registry.list_all();

        if all.is_empty() {
            return CommandResult::Message("No providers available.".to_string());
        }

        // Group by provider
        use std::collections::HashMap;
        let mut by_provider: HashMap<String, Vec<_>> = HashMap::new();
        for entry in &all {
            by_provider
                .entry(entry.info.provider_id.to_string())
                .or_default()
                .push(entry);
        }

        // Sort providers alphabetically for stable output
        let mut provider_keys: Vec<String> = by_provider.keys().cloned().collect();
        provider_keys.sort();

        let mut lines = vec!["Available providers:\n".to_string()];
        for provider in &provider_keys {
            let models = &by_provider[provider];
            lines.push(format!(
                "\n{} ({} model{})",
                provider.to_uppercase(),
                models.len(),
                if models.len() == 1 { "" } else { "s" }
            ));
            for m in models.iter().take(3) {
                let cost_str = match (m.cost_input, m.cost_output) {
                    (Some(i), Some(o)) => format!("${:.2}/${:.2} per 1M", i, o),
                    _ => "free/local".to_string(),
                };
                lines.push(format!(
                    "  {} — {}K ctx, {}",
                    m.info.id,
                    m.info.context_window / 1000,
                    cost_str
                ));
            }
            if models.len() > 3 {
                lines.push(format!("  ... and {} more", models.len() - 3));
            }
        }

        CommandResult::Message(lines.join("\n"))
    }
}

// ---- /connect -------------------------------------------------------------

#[async_trait]
impl SlashCommand for ConnectCommand {
    fn name(&self) -> &str {
        "connect"
    }
    fn description(&self) -> &str {
        "Connect an AI provider"
    }
    fn help(&self) -> &str {
        "Usage: /connect\n\nOpens the interactive provider picker dialog.\nSelect a provider to see setup instructions."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        // This is handled by the TUI interceptor — opening the connect dialog.
        CommandResult::Message("Use the connect dialog to set up a provider.".to_string())
    }
}

// ---- /agent ---------------------------------------------------------------

#[async_trait]
impl SlashCommand for AgentCommand {
    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        "List available agents or get info about a specific agent"
    }
    fn help(&self) -> &str {
        "Usage: /agent [name]\n\nWithout arguments, lists all available named agents.\nWith a name, shows details for that agent.\n\nTo use an agent, start MikMik with: --agent <name>"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        use std::collections::HashMap;

        // Built-in defaults, settings.json agents and folder agents, folder-most-wins.
        let all_agents: HashMap<String, mikmik_core::AgentDefinition> =
            mikmik_core::resolve_agents(&ctx.working_dir, &ctx.config.agents);

        let agent_name = args.trim();

        if agent_name.is_empty() {
            // List all visible agents.
            let mut keys: Vec<&String> = all_agents
                .iter()
                .filter(|(_, d)| d.visible)
                .map(|(k, _)| k)
                .collect();
            keys.sort();

            let mut output = "Available agents:\n\n".to_string();
            for name in keys {
                let def = &all_agents[name];
                output.push_str(&format!(
                    "  @{} — {}\n    access: {}{}\n",
                    name,
                    def.description.as_deref().unwrap_or(""),
                    def.access,
                    def.max_turns
                        .map(|t| format!(", max_turns: {}", t))
                        .unwrap_or_default(),
                ));
            }
            output.push_str("\nUse --agent <name> when starting MikMik to activate an agent.");
            CommandResult::Message(output)
        } else if let Some(def) = all_agents.get(agent_name) {
            // Show details for the named agent.
            let mut output = format!("Agent: @{}\n", agent_name);
            if let Some(ref desc) = def.description {
                output.push_str(&format!("Description: {}\n", desc));
            }
            output.push_str(&format!("Access: {}\n", def.access));
            if let Some(ref model) = def.model {
                output.push_str(&format!("Model: {}\n", model));
            }
            if let Some(t) = def.max_turns {
                output.push_str(&format!("Max turns: {}\n", t));
            }
            if let Some(ref color) = def.color {
                output.push_str(&format!("Color: {}\n", color));
            }
            if let Some(ref prompt) = def.prompt {
                output.push_str(&format!("\nSystem prompt prefix:\n  {}\n", prompt));
            }
            output.push_str(&format!("\nTo activate: mikmik --agent {}", agent_name));
            CommandResult::Message(output)
        } else {
            CommandResult::Error(format!(
                "Unknown agent '{}'. Run /agent to see available agents.",
                agent_name
            ))
        }
    }
}
