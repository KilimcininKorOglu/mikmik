// Account/auth commands: `/login`, `/logout`, `/accounts`, `/switch`, `/refresh`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct LoginCommand;
pub struct LogoutCommand;
pub struct RefreshCommand;

// ---- /login --------------------------------------------------------------

#[async_trait]
impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }
    fn description(&self) -> &str {
        "Authenticate with Anthropic or Codex (multi-account)"
    }
    fn help(&self) -> &str {
        "Usage: /login [--console] [--codex] [--label <name>]\n\n\
         Start an OAuth login. By default authenticates with Claude.ai. Pass\n\
         `--console` for an API-key (Console) login, or `--codex` to add a\n\
         ChatGPT/Codex account. `--label work` names the saved profile so you\n\
         can `switch` to it later by that name."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        let use_codex = tokens.contains(&"--codex");
        let login_with_claude_ai = !tokens.contains(&"--console");
        let label = parse_label_arg(&tokens);

        let provider = if use_codex {
            mikmik_core::ProviderId::CODEX
        } else {
            mikmik_core::ProviderId::ANTHROPIC
        };

        CommandResult::StartLoginForProvider {
            provider: provider.to_string(),
            login_with_claude_ai,
            label,
        }
    }
}

/// Drop every account speaking `protocol`: its credential and its `providers`
/// entry. Returns how many were removed.
fn forget_every_account(protocol: &str) -> usize {
    let mut store = mikmik_core::AuthStore::load();
    let ids = store.accounts_for_protocol(protocol);
    for id in &ids {
        store.credentials.remove(id);
        let _ = mikmik_core::config::forget_account(id);
    }
    store.save();
    ids.len()
}

fn parse_label_arg(tokens: &[&str]) -> Option<String> {
    let mut it = tokens.iter();
    while let Some(t) = it.next() {
        if *t == "--label" || *t == "-l" {
            return it.next().map(|s| s.to_string());
        }
        if let Some(rest) = t.strip_prefix("--label=") {
            return Some(rest.to_string());
        }
    }
    None
}

// ---- /logout -------------------------------------------------------------

#[async_trait]
impl SlashCommand for LogoutCommand {
    fn name(&self) -> &str {
        "logout"
    }
    fn description(&self) -> &str {
        "Clear credentials for the active account"
    }
    fn help(&self) -> &str {
        "Usage: /logout [--codex] [--all]\n\n\
         By default removes the active Anthropic account. `--codex` targets\n\
         Codex instead. `--all` purges every stored credential for the chosen\n\
         provider and clears any API key in settings."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        let use_codex = tokens.contains(&"--codex");
        let purge_all = tokens.contains(&"--all");

        if use_codex {
            if purge_all {
                let removed = forget_every_account(mikmik_core::ProviderId::CODEX);
                return CommandResult::Message(format!(
                    "Removed {} stored Codex account(s).",
                    removed
                ));
            }
            if let Err(e) = mikmik_core::oauth_config::clear_codex_tokens() {
                return CommandResult::Error(format!("Failed to clear Codex tokens: {}", e));
            }
            return CommandResult::Message("Logged out of the active Codex account.".to_string());
        }

        // Anthropic logout.
        if purge_all {
            let removed = forget_every_account(mikmik_core::ProviderId::ANTHROPIC);
            let mut settings = mikmik_core::config::Settings::load()
                .await
                .unwrap_or_default();
            settings.config.api_key = None;
            let _ = settings.save().await;
            ctx.config.api_key = None;
            return CommandResult::Message(format!(
                "Removed {} stored Anthropic account(s) and cleared API key.",
                removed
            ));
        }

        if let Err(e) = mikmik_core::oauth::OAuthTokens::clear().await {
            return CommandResult::Error(format!("Failed to clear OAuth tokens: {}", e));
        }
        let mut settings = mikmik_core::config::Settings::load()
            .await
            .unwrap_or_default();
        settings.config.api_key = None;
        if let Err(e) = settings.save().await {
            return CommandResult::Error(format!("Failed to update settings: {}", e));
        }
        ctx.config.api_key = None;
        CommandResult::Message("Logged out of the active Anthropic account.".to_string())
    }
}

// ---- /accounts ------------------------------------------------------------

pub struct AccountsCommand;

#[async_trait]
impl SlashCommand for AccountsCommand {
    fn name(&self) -> &str {
        "accounts"
    }
    fn description(&self) -> &str {
        "List every stored account"
    }
    fn help(&self) -> &str {
        "Usage: /accounts\n\n\
         Lists every account that holds a credential, grouped by the protocol\n\
         it speaks, with the active one marked `*`. Use /switch to change the\n\
         active account, /connect or /login to add one, /logout to remove one."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let store = mikmik_core::AuthStore::load();
        let settings = mikmik_core::config::Settings::load_sync().unwrap_or_default();
        let active = ctx
            .config
            .provider
            .clone()
            .or_else(|| settings.provider.clone());

        // One row per account, grouped by protocol. Metadata comes from the
        // credential itself, because that is the only place it is stored.
        let mut by_protocol: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for (account_id, credential) in &store.credentials {
            // The workspace session is not a model account: it authenticates
            // against the organisation's own server, which serves no
            // completions, and `/switch` could not do anything with it.
            // `/workspace status` is where it belongs.
            if matches!(
                credential,
                mikmik_core::StoredCredential::WorkspaceSession { .. }
            ) {
                continue;
            }
            let protocol = settings
                .providers
                .get(account_id)
                .map(|entry| entry.protocol_or(account_id))
                .unwrap_or_else(|| account_id.clone());
            let marker = if active.as_deref() == Some(account_id.as_str()) {
                "*"
            } else {
                " "
            };
            by_protocol.entry(protocol).or_default().push(format!(
                "  {} {}{}",
                marker,
                account_id,
                describe(credential)
            ));
        }

        if by_protocol.is_empty() {
            return CommandResult::Message(
                "No accounts stored. Use /connect to add one.".to_string(),
            );
        }

        let mut out = String::new();
        for (protocol, mut rows) in by_protocol {
            rows.sort();
            out.push_str(&format!("{}:\n", protocol));
            for row in rows {
                out.push_str(&row);
                out.push('\n');
            }
        }
        CommandResult::Message(out.trim_end().to_string())
    }
}

/// The identity a credential carries, for the `/accounts` listing.
fn describe(credential: &mikmik_core::StoredCredential) -> String {
    use mikmik_core::StoredCredential as C;
    match credential {
        C::AnthropicOAuth(tokens) => {
            let tier = tokens
                .subscription_type
                .as_deref()
                .map(|t| format!(" [{}]", t))
                .unwrap_or_default();
            let email = tokens
                .email
                .as_deref()
                .map(|e| format!("  {}", e))
                .unwrap_or_default();
            format!("{tier}{email}")
        }
        C::CodexOAuth(tokens) => tokens
            .account_id
            .as_deref()
            .map(|id| format!("  {}", id))
            .unwrap_or_default(),
        C::KimiOAuth(tokens) => tokens
            .account_id
            .as_deref()
            .map(|id| format!("  {}", id))
            .unwrap_or_default(),
        C::XaiOAuth(tokens) => tokens
            .account_id
            .as_deref()
            .map(|id| format!("  {}", id))
            .unwrap_or_default(),
        // GitLab Duo tokens carry no readable identity locally; the workspace
        // session is skipped by the listing. These arms are only what the
        // compiler needs to see the match is complete.
        C::GitlabDuoOAuth(_)
        | C::OAuthToken { .. }
        | C::ApiKey { .. }
        | C::WorkspaceSession { .. } => String::new(),
    }
}

// ---- /switch --------------------------------------------------------------

pub struct SwitchCommand;

#[async_trait]
impl SlashCommand for SwitchCommand {
    fn name(&self) -> &str {
        "switch"
    }
    fn description(&self) -> &str {
        "Make a stored account the active one"
    }
    fn help(&self) -> &str {
        "Usage: /switch <account>\n\n\
         Point the session at a stored account. Run /accounts first to see the\n\
         names. Every account is switched the same way, whatever protocol it\n\
         speaks."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        let Some(account_id) = tokens.iter().find(|t| !t.starts_with("--")) else {
            return CommandResult::Error(
                "Usage: /switch <account> (try /accounts to see the names)".to_string(),
            );
        };

        let store = mikmik_core::AuthStore::load();
        if store.get(account_id).is_none() {
            let mut known: Vec<&str> = store.credentials.keys().map(String::as_str).collect();
            known.sort();
            return CommandResult::Error(if known.is_empty() {
                format!("No account named '{account_id}'. Nothing is stored yet.")
            } else {
                format!(
                    "No account named '{account_id}'. Stored: {}.",
                    known.join(", ")
                )
            });
        }

        let protocol = mikmik_core::config::Settings::load_sync()
            .ok()
            .and_then(|settings| {
                settings
                    .providers
                    .get(*account_id)
                    .map(|entry| entry.protocol_or(account_id))
            })
            .unwrap_or_else(|| (*account_id).to_string());

        if let Err(e) = mikmik_core::config::register_account(account_id, &protocol, true) {
            return CommandResult::Error(format!("Failed to switch account: {e}"));
        }
        ctx.config.provider = Some((*account_id).to_string());
        CommandResult::ConfigChangeMessage(
            ctx.config.clone(),
            format!("Switched to '{account_id}'."),
        )
    }
}

// ---- /refresh ------------------------------------------------------------

#[async_trait]
impl SlashCommand for RefreshCommand {
    fn name(&self) -> &str {
        "refresh"
    }
    fn description(&self) -> &str {
        "Clear saved provider auth and model caches"
    }
    fn help(&self) -> &str {
        "Usage: /refresh\n\n\
         Clears saved provider credentials, provider/model selection, and model caches, then rebuilds the live runtime state.\n\
         After refreshing, run /connect to authenticate and choose a provider again."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        if !args.trim().is_empty() {
            return CommandResult::Error("Usage: /refresh".to_string());
        }
        CommandResult::RefreshProviderState
    }
}
