// auth_store.rs — JSON-based credential store at ~/.config/mikmik/auth.json.
//
// Stores API keys and OAuth tokens for providers so users don't have to rely
// solely on environment variables.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// A stored credential for an account.
///
/// Every credential the product holds lives here, whatever its shape, so an
/// account is one entry in one file rather than a registry entry plus a token
/// file of its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StoredCredential {
    #[serde(rename = "api")]
    ApiKey { key: String },
    /// GitHub Copilot's device-flow token.
    #[serde(rename = "oauth")]
    OAuthToken {
        access: String,
        refresh: String,
        expires: u64,
    },
    /// Anthropic's OAuth tokens, from either the claude.ai or the console flow.
    ///
    /// Carries its own fields rather than collapsing into `OAuthToken`, because
    /// the scope list decides whether the credential is a Bearer token or a
    /// minted API key, and the identity fields name the account.
    #[serde(rename = "anthropic-oauth")]
    AnthropicOAuth(crate::oauth::OAuthTokens),
    /// OpenAI Codex OAuth tokens.
    #[serde(rename = "codex-oauth")]
    CodexOAuth(crate::oauth_config::CodexTokens),
    /// Kimi Code device-flow OAuth tokens.
    #[serde(rename = "kimi-oauth")]
    KimiOAuth(crate::kimi_oauth::KimiTokens),
    /// xAI Grok device-flow OAuth tokens.
    #[serde(rename = "xai-oauth")]
    XaiOAuth(crate::xai_oauth::XaiTokens),
    /// GitLab Duo OAuth tokens (PKCE loopback or a stored PAT).
    #[serde(rename = "gitlab-duo-oauth")]
    GitlabDuoOAuth(crate::gitlab_duo::GitlabTokens),
    /// Google Antigravity OAuth tokens plus the resolved Cloud Code project.
    #[serde(rename = "antigravity-oauth")]
    AntigravityOAuth(crate::antigravity_oauth::AntigravityTokens),
    /// Devin / Windsurf Cascade session token (PKCE loopback).
    #[serde(rename = "devin-oauth")]
    DevinOAuth(crate::devin_oauth::DevinTokens),
    /// Cursor (Cursor Pro) OAuth tokens (PKCE poll).
    #[serde(rename = "cursor-oauth")]
    CursorOAuth(crate::cursor_oauth::CursorTokens),
    /// A session on an organisation's configuration server.
    ///
    /// Held here rather than in `settings.json` beside `workspace.url`: this
    /// file is written `0o600`, and the token reaches every provider key the
    /// organisation has assigned to this account.
    ///
    /// The address is repeated here on purpose. A token issued by one server
    /// must never be sent to another, so the credential carries the only
    /// address it is good for instead of trusting whatever the settings file
    /// says at the moment it is used.
    #[serde(rename = "workspace-session")]
    WorkspaceSession {
        url: String,
        token: String,
        /// Seconds since the Unix epoch.
        expires: u64,
    },
}

/// Seconds since the Unix epoch.
///
/// `SystemTime` rather than `Instant`: this value is written to a file and
/// compared after a restart, which a monotonic clock cannot do.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The key `auth.json` holds the workspace session under.
///
/// A fixed name rather than one per server: an installation logs in to one
/// organisation, and two entries would leave the second one silently unused.
pub const WORKSPACE_ACCOUNT: &str = "workspace";

/// Persistent credential store backed by `~/.config/mikmik/auth.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AuthStore {
    pub credentials: HashMap<String, StoredCredential>,
}

// ---------------------------------------------------------------------------
// The old account registry, read once by the startup migration
// ---------------------------------------------------------------------------

/// `accounts.json` as it used to be written.
///
/// Declared here rather than reused from `accounts.rs` so the migration keeps
/// working after that module drops the types.
#[derive(Debug, Default, Deserialize)]
struct LegacyRegistry {
    #[serde(default)]
    providers: std::collections::BTreeMap<String, LegacyProviderAccounts>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyProviderAccounts {
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    profiles: std::collections::BTreeMap<String, serde_json::Value>,
}

/// The protocol a credential's own shape implies, for an account that has no
/// `providers` entry yet.
///
/// An API key says nothing about its vendor, so it implies nothing.
fn implied_protocol(credential: &StoredCredential) -> Option<&'static str> {
    match credential {
        StoredCredential::AnthropicOAuth(_) => Some(crate::provider_id::ProviderId::ANTHROPIC),
        StoredCredential::CodexOAuth(_) => Some(crate::provider_id::ProviderId::CODEX),
        StoredCredential::KimiOAuth(_) => Some(crate::provider_id::ProviderId::KIMI_CODE),
        StoredCredential::XaiOAuth(_) => Some(crate::provider_id::ProviderId::XAI_OAUTH),
        StoredCredential::GitlabDuoOAuth(_) => Some(crate::provider_id::ProviderId::GITLAB_DUO),
        StoredCredential::AntigravityOAuth(_) => {
            Some(crate::provider_id::ProviderId::GOOGLE_ANTIGRAVITY)
        }
        StoredCredential::DevinOAuth(_) => Some(crate::provider_id::ProviderId::DEVIN),
        StoredCredential::CursorOAuth(_) => Some(crate::provider_id::ProviderId::CURSOR),
        StoredCredential::OAuthToken { .. } => Some("github-copilot"),
        StoredCredential::ApiKey { .. } => None,
        // Not a model provider at all: it authenticates against the
        // organisation's own server, which serves no completions.
        StoredCredential::WorkspaceSession { .. } => None,
    }
}

/// The token file each provider wrote inside a profile directory.
fn token_file_name(protocol: &str) -> &'static str {
    match protocol {
        crate::provider_id::ProviderId::CODEX => "codex_tokens.json",
        _ => "oauth_tokens.json",
    }
}

/// Read one profile's token file into the credential it becomes.
fn read_legacy_credential(protocol: &str, path: &std::path::Path) -> Option<StoredCredential> {
    let text = std::fs::read_to_string(path).ok()?;
    match protocol {
        crate::provider_id::ProviderId::CODEX => serde_json::from_str(&text)
            .ok()
            .map(StoredCredential::CodexOAuth),
        _ => serde_json::from_str(&text)
            .ok()
            .map(StoredCredential::AnthropicOAuth),
    }
}

/// A name no account is using yet, keeping the profile id where it is free.
fn free_account_name(settings: &crate::config::Settings, store: &AuthStore, base: &str) -> String {
    crate::accounts::unique_account_name(base, |candidate| {
        settings.providers.contains_key(candidate) || store.credentials.contains_key(candidate)
    })
}

/// Give a migrated account the `providers` entry it needs to be built.
fn open_provider_entry(settings: &mut crate::config::Settings, account_id: &str, protocol: &str) {
    let entry = settings
        .providers
        .entry(account_id.to_string())
        .or_default();
    entry.enabled = true;
    if protocol != account_id {
        entry.protocol = Some(protocol.to_string());
    }
}

/// Move the old registry and profile directories aside.
///
/// Kept rather than deleted: the credentials have just been copied, and a
/// migration that got something wrong is only recoverable while the originals
/// still exist.
fn archive_legacy_layout(
    root: &std::path::Path,
    registry_path: &std::path::Path,
    accounts_dir: &std::path::Path,
) {
    let backup = root.join(format!(
        "accounts-backup-{}",
        chrono::Utc::now().timestamp()
    ));
    if std::fs::create_dir_all(&backup).is_err() {
        tracing::warn!(
            "could not create {}; leaving the old account files in place",
            backup.display()
        );
        return;
    }
    crate::accounts::set_user_only_dir_perms(&backup);
    for (source, name) in [(registry_path, "accounts.json"), (accounts_dir, "accounts")] {
        if source.exists() {
            if let Err(e) = std::fs::rename(source, backup.join(name)) {
                tracing::warn!("could not move {} aside: {}", source.display(), e);
            }
        }
    }
    for legacy in [
        crate::oauth::OAuthTokens::token_file_path(),
        crate::oauth_config::codex_tokens_path(),
    ] {
        if legacy.exists() {
            let name = legacy
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "legacy_tokens.json".to_string());
            if let Err(e) = std::fs::rename(&legacy, backup.join(name)) {
                tracing::warn!("could not move {} aside: {}", legacy.display(), e);
            }
        }
    }
}

impl AuthStore {
    /// Path to the auth store file.
    pub fn path() -> PathBuf {
        crate::config::Settings::config_dir().join("auth.json")
    }

    /// Load the store from disk (returns default if missing or invalid).
    pub fn load() -> Self {
        let path = Self::path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => match serde_json::from_str(&s) {
                    Ok(store) => store,
                    Err(e) => {
                        tracing::warn!(
                            "auth store at {} is corrupt ({}); starting with an empty store. \
                             The corrupt file is left in place until the next save.",
                            path.display(),
                            e
                        );
                        Self::default()
                    }
                },
                Err(e) => {
                    tracing::warn!("failed to read auth store at {}: {}", path.display(), e);
                    Self::default()
                }
            }
        } else {
            Self::default()
        }
    }

    /// Persist the store to disk (best-effort).
    ///
    /// Writes to a temp file then renames over the destination so a crash or
    /// disk-full mid-write can never truncate `auth.json` (which would
    /// silently wipe the user's stored credentials on the next load).
    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            crate::accounts::set_user_only_dir_perms(parent);
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(_) => return,
        };
        let tmp = path.with_file_name(format!(".auth.json.mikmik-tmp-{}", std::process::id()));
        if std::fs::write(&tmp, &json).is_ok() {
            // auth.json holds API keys + OAuth tokens. Lock the temp file to
            // 0o600 *before* the rename so the live credential file is never
            // even momentarily world/group readable (issue #212).
            crate::accounts::set_user_only_perms(&tmp);
            if std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// Store a credential for the given provider (persists immediately).
    pub fn set(&mut self, provider_id: &str, cred: StoredCredential) {
        self.credentials.insert(provider_id.to_string(), cred);
        self.save();
    }

    /// Get the stored credential for a provider.
    pub fn get(&self, provider_id: &str) -> Option<&StoredCredential> {
        self.credentials.get(provider_id)
    }

    /// Remove the credential for a provider (persists immediately).
    pub fn remove(&mut self, provider_id: &str) {
        self.credentials.remove(provider_id);
        self.save();
    }

    /// The Anthropic OAuth tokens stored for `account_id`, if that is what the
    /// account holds.
    pub fn anthropic_tokens(&self, account_id: &str) -> Option<&crate::oauth::OAuthTokens> {
        match self.get(account_id) {
            Some(StoredCredential::AnthropicOAuth(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// The Codex OAuth tokens stored for `account_id`, if that is what the
    /// account holds.
    pub fn codex_tokens(&self, account_id: &str) -> Option<&crate::oauth_config::CodexTokens> {
        match self.get(account_id) {
            Some(StoredCredential::CodexOAuth(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// Store Anthropic OAuth tokens for `account_id` (persists immediately).
    pub fn set_anthropic_tokens(&mut self, account_id: &str, tokens: crate::oauth::OAuthTokens) {
        self.set(account_id, StoredCredential::AnthropicOAuth(tokens));
    }

    /// Store Codex OAuth tokens for `account_id` (persists immediately).
    pub fn set_codex_tokens(&mut self, account_id: &str, tokens: crate::oauth_config::CodexTokens) {
        self.set(account_id, StoredCredential::CodexOAuth(tokens));
    }

    /// The Kimi OAuth tokens stored under `account_id`, if any.
    pub fn kimi_tokens(&self, account_id: &str) -> Option<&crate::kimi_oauth::KimiTokens> {
        match self.get(account_id) {
            Some(StoredCredential::KimiOAuth(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// Store Kimi OAuth tokens for `account_id` (persists immediately).
    pub fn set_kimi_tokens(&mut self, account_id: &str, tokens: crate::kimi_oauth::KimiTokens) {
        self.set(account_id, StoredCredential::KimiOAuth(tokens));
    }

    /// The xAI OAuth tokens stored under `account_id`, if any.
    pub fn xai_tokens(&self, account_id: &str) -> Option<&crate::xai_oauth::XaiTokens> {
        match self.get(account_id) {
            Some(StoredCredential::XaiOAuth(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// Store xAI OAuth tokens for `account_id` (persists immediately).
    pub fn set_xai_tokens(&mut self, account_id: &str, tokens: crate::xai_oauth::XaiTokens) {
        self.set(account_id, StoredCredential::XaiOAuth(tokens));
    }

    /// The GitLab Duo OAuth tokens stored under `account_id`, if any.
    pub fn gitlab_tokens(&self, account_id: &str) -> Option<&crate::gitlab_duo::GitlabTokens> {
        match self.get(account_id) {
            Some(StoredCredential::GitlabDuoOAuth(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// Store GitLab Duo OAuth tokens for `account_id` (persists immediately).
    pub fn set_gitlab_tokens(&mut self, account_id: &str, tokens: crate::gitlab_duo::GitlabTokens) {
        self.set(account_id, StoredCredential::GitlabDuoOAuth(tokens));
    }

    /// The Antigravity OAuth tokens stored under `account_id`, if any.
    pub fn antigravity_tokens(
        &self,
        account_id: &str,
    ) -> Option<&crate::antigravity_oauth::AntigravityTokens> {
        match self.get(account_id) {
            Some(StoredCredential::AntigravityOAuth(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// Store Antigravity OAuth tokens for `account_id` (persists immediately).
    pub fn set_antigravity_tokens(
        &mut self,
        account_id: &str,
        tokens: crate::antigravity_oauth::AntigravityTokens,
    ) {
        self.set(account_id, StoredCredential::AntigravityOAuth(tokens));
    }

    /// The Devin session token stored under `account_id`, if any.
    pub fn devin_tokens(&self, account_id: &str) -> Option<&crate::devin_oauth::DevinTokens> {
        match self.get(account_id) {
            Some(StoredCredential::DevinOAuth(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// Store Devin session token for `account_id` (persists immediately).
    pub fn set_devin_tokens(&mut self, account_id: &str, tokens: crate::devin_oauth::DevinTokens) {
        self.set(account_id, StoredCredential::DevinOAuth(tokens));
    }

    /// The Cursor OAuth tokens stored under `account_id`, if any.
    pub fn cursor_tokens(&self, account_id: &str) -> Option<&crate::cursor_oauth::CursorTokens> {
        match self.get(account_id) {
            Some(StoredCredential::CursorOAuth(tokens)) => Some(tokens),
            _ => None,
        }
    }

    /// Store Cursor OAuth tokens for `account_id` (persists immediately).
    pub fn set_cursor_tokens(
        &mut self,
        account_id: &str,
        tokens: crate::cursor_oauth::CursorTokens,
    ) {
        self.set(account_id, StoredCredential::CursorOAuth(tokens));
    }

    /// The live workspace session for `url`, if there is one.
    ///
    /// Answers `None` for a session that has expired or that was issued by a
    /// different address, so a caller cannot send one organisation's token to
    /// another's server by changing one line of `settings.json`.
    pub fn workspace_session(&self, url: &str) -> Option<&str> {
        let StoredCredential::WorkspaceSession {
            url: issued_by,
            token,
            expires,
        } = self.get(WORKSPACE_ACCOUNT)?
        else {
            return None;
        };
        if issued_by.trim_end_matches('/') != url.trim().trim_end_matches('/') {
            return None;
        }
        if *expires <= now_secs() {
            return None;
        }
        Some(token.as_str())
    }

    /// Store a workspace session (persists immediately).
    pub fn set_workspace_session(&mut self, url: &str, token: &str, expires_in_secs: u64) {
        self.set(
            WORKSPACE_ACCOUNT,
            StoredCredential::WorkspaceSession {
                url: url.trim().trim_end_matches('/').to_string(),
                token: token.to_string(),
                expires: now_secs().saturating_add(expires_in_secs),
            },
        );
    }

    /// Drop the workspace session (persists immediately).
    pub fn clear_workspace_session(&mut self) {
        self.remove(WORKSPACE_ACCOUNT);
    }

    /// Every account holding a credential and speaking `protocol`.
    ///
    /// An account's protocol is declared by its `providers` entry, so that is
    /// asked first. An account with a credential but no entry falls back to
    /// what its credential shape implies, which is what a login writes before
    /// the entry exists.
    pub fn accounts_for_protocol(&self, protocol: &str) -> Vec<String> {
        let settings = crate::config::Settings::load_sync().unwrap_or_default();
        let mut ids: Vec<String> = self
            .credentials
            .iter()
            .filter(|(id, cred)| match settings.providers.get(*id) {
                Some(entry) => entry.protocol_or(id) == protocol,
                None => implied_protocol(cred) == Some(protocol),
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Move every plaintext `providers.<account>.api_key` out of
    /// `settings.json` and into this store.
    ///
    /// `settings.json` is written with the default file mode and holds no
    /// other secret, while `auth.json` is written `0o600`, so a key left in
    /// settings is readable by every other account on the machine. Returns the
    /// accounts that were moved, so a caller can tell the user where the key
    /// went.
    ///
    /// Runs at startup, which also means a key written into `settings.json` by
    /// hand is relocated on the next launch rather than staying in the clear.
    pub fn migrate_plaintext_provider_keys() -> Vec<String> {
        let Ok(mut settings) = crate::config::Settings::load_sync() else {
            return Vec::new();
        };

        let mut store = Self::load();
        let mut moved = Vec::new();
        for (account_id, provider) in settings.providers.iter_mut() {
            let Some(key) = provider.api_key.take().filter(|key| !key.is_empty()) else {
                continue;
            };
            // A credential already in the store is the newer one, because
            // nothing writes to `settings.json` any more. Drop the stale copy
            // rather than restoring it over the live credential.
            if store.get(account_id).is_none() {
                store
                    .credentials
                    .insert(account_id.clone(), StoredCredential::ApiKey { key });
            }
            moved.push(account_id.clone());
        }

        if moved.is_empty() {
            return moved;
        }

        store.save();
        if let Err(e) = settings.save_sync() {
            // The key now lives in both files. Say so instead of reporting a
            // move that only half happened.
            tracing::warn!(
                "moved {} plaintext provider key(s) into the auth store, but could not \
                 rewrite settings.json ({}); the plaintext copy is still there",
                moved.len(),
                e
            );
        }
        moved
    }

    /// Fold the old account registry into the auth store and `settings.json`.
    ///
    /// Anthropic and Codex accounts used to live as a registry entry in
    /// `accounts.json` plus a token file under `accounts/<provider>/<id>/`,
    /// which meant the same concept was stored two ways and only one of them
    /// could be addressed as `"<account>/<model>"`. Each profile becomes an
    /// ordinary account: a credential here and a `providers` entry there.
    ///
    /// Returns the account names that were moved. The old files are left in a
    /// `accounts-backup-<timestamp>/` directory rather than deleted, so a
    /// migration that goes wrong can be undone by hand.
    pub fn migrate_account_registry() -> Vec<String> {
        let root = crate::config::Settings::config_dir();
        let registry_path = root.join("accounts.json");
        let accounts_dir = root.join("accounts");
        let legacy_anthropic = crate::oauth::OAuthTokens::token_file_path();
        let legacy_codex = crate::oauth_config::codex_tokens_path();

        let has_registry = registry_path.exists();
        if !has_registry && !legacy_anthropic.exists() && !legacy_codex.exists() {
            return Vec::new();
        }

        let Ok(mut settings) = crate::config::Settings::load_sync() else {
            return Vec::new();
        };
        let mut store = Self::load();
        let mut moved = Vec::new();
        // Which account each provider's active profile became, so the active
        // pointer can be rewritten once every profile has a name.
        let mut new_active: Vec<(String, String)> = Vec::new();

        if has_registry {
            let registry: LegacyRegistry = std::fs::read_to_string(&registry_path)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();

            for (protocol, section) in &registry.providers {
                for profile_id in section.profiles.keys() {
                    let token_path = accounts_dir
                        .join(protocol)
                        .join(profile_id)
                        .join(token_file_name(protocol));
                    let Some(credential) = read_legacy_credential(protocol, &token_path) else {
                        continue;
                    };
                    let account_id = free_account_name(&settings, &store, profile_id);
                    store.credentials.insert(account_id.clone(), credential);
                    open_provider_entry(&mut settings, &account_id, protocol);
                    if section.active.as_deref() == Some(profile_id.as_str()) {
                        new_active.push((protocol.clone(), account_id.clone()));
                    }
                    moved.push(account_id);
                }
            }
        }

        // The single-file layout that predates the registry. Named after the
        // vendor, because there is no profile id to take a name from.
        for (protocol, path) in [
            (crate::provider_id::ProviderId::ANTHROPIC, legacy_anthropic),
            (crate::provider_id::ProviderId::CODEX, legacy_codex),
        ] {
            let Some(credential) = read_legacy_credential(protocol, &path) else {
                continue;
            };
            let account_id = free_account_name(&settings, &store, protocol);
            store.credentials.insert(account_id.clone(), credential);
            open_provider_entry(&mut settings, &account_id, protocol);
            new_active.push((protocol.to_string(), account_id.clone()));
            moved.push(account_id);
        }

        if moved.is_empty() {
            return moved;
        }

        // The active pointer named a vendor before accounts had names of their
        // own. Point it at whichever account that vendor's active profile
        // became, and leave any other value alone: it already names an account.
        if let Some(active) = settings.provider.clone() {
            if let Some((_, account_id)) = new_active
                .iter()
                .find(|(protocol, _)| protocol.as_str() == active)
            {
                settings.provider = Some(account_id.clone());
                settings.config.provider = Some(account_id.clone());
            }
        }

        store.save();
        if let Err(e) = settings.save_sync() {
            tracing::warn!(
                "moved {} account(s) into the auth store, but could not write \
                 settings.json ({}); they have no providers entry yet",
                moved.len(),
                e
            );
            return moved;
        }
        archive_legacy_layout(&root, &registry_path, &accounts_dir);
        moved
    }

    /// Get the API key for a provider, checking stored credentials first then
    /// falling back to the relevant environment variable.
    pub fn api_key_for(&self, provider_id: &str) -> Option<String> {
        self.api_key_for_protocol(provider_id, provider_id)
    }

    /// Get the API key stored under `account_id`, reading it as a credential of
    /// `protocol`.
    ///
    /// The two differ whenever the user named the account: the credential is
    /// filed under the name they chose, while how to read it and which env var
    /// stands in for it are properties of the wire format it speaks. Passing
    /// the account name as both is what [`api_key_for`](Self::api_key_for)
    /// does, which is right for an account named after its vendor.
    pub fn api_key_for_protocol(&self, account_id: &str, protocol: &str) -> Option<String> {
        // Check stored credentials first
        if let Some(stored) = self.get(account_id) {
            match stored {
                StoredCredential::ApiKey { key } => {
                    if !key.is_empty() {
                        return Some(key.clone());
                    }
                }
                StoredCredential::OAuthToken {
                    access, refresh, ..
                } if protocol == "github-copilot" => {
                    if !refresh.is_empty() {
                        return Some(refresh.clone());
                    }
                    if !access.is_empty() {
                        return Some(access.clone());
                    }
                }
                // The claude.ai flow presents the access token as a Bearer and
                // the console flow presents the API key it minted, so the
                // credential to hand out is whichever the scopes call for.
                //
                // Expiry is not checked here: this is a synchronous read and
                // refreshing needs the network. The caller that can await goes
                // through `oauth::resolve_auth_for_account`.
                StoredCredential::AnthropicOAuth(tokens) => {
                    if let Some(credential) = tokens.effective_credential() {
                        return Some(credential.to_string());
                    }
                }
                StoredCredential::CodexOAuth(tokens) => {
                    if !tokens.access_token.is_empty() {
                        return Some(tokens.access_token.clone());
                    }
                }
                _ => {}
            }
        }
        // Fall back to environment variable.
        //
        // These mappings must match the env var each provider's adapter
        // actually reads in `crates/api/src/providers/openai_compat_providers.rs`
        // (and the bespoke adapters next to it). When they drift, keys that
        // were exported via env vars look "configured" to the dialog but
        // resolve to empty at request time. If you add a provider there,
        // mirror its env var here.
        let env_var = match protocol {
            "anthropic" => "ANTHROPIC_API_KEY",
            "openai" => "OPENAI_API_KEY",
            "google" => "GOOGLE_API_KEY",
            "groq" => "GROQ_API_KEY",
            "cerebras" => "CEREBRAS_API_KEY",
            "deepseek" => "DEEPSEEK_API_KEY",
            "mistral" => "MISTRAL_API_KEY",
            "xai" => "XAI_API_KEY",
            "openrouter" => "OPENROUTER_API_KEY",
            "togetherai" | "together-ai" => "TOGETHER_API_KEY",
            "perplexity" => "PERPLEXITY_API_KEY",
            "cohere" => "COHERE_API_KEY",
            "deepinfra" => "DEEPINFRA_API_KEY",
            "venice" => "VENICE_API_KEY",
            "github-copilot" => "GITHUB_TOKEN",
            "azure" => "AZURE_API_KEY",
            "huggingface" => "HF_TOKEN",
            "nvidia" => "NVIDIA_API_KEY",
            "zai" => "ZAI_API_KEY",
            "opencode-zen" | "opencode-go" => "OPENCODE_API_KEY",
            "crof" => "CROF_API_KEY",
            "sambanova" => "SAMBANOVA_API_KEY",
            // qwen adapter reads DASHSCOPE_API_KEY (Alibaba's DashScope is the
            // backing service), not QWEN_API_KEY.
            "qwen" | "alibaba" => "DASHSCOPE_API_KEY",
            "moonshot" | "moonshotai" => "MOONSHOT_API_KEY",
            "zhipu" | "zhipuai" => "ZHIPU_API_KEY",
            "siliconflow" => "SILICONFLOW_API_KEY",
            "nebius" => "NEBIUS_API_KEY",
            "novita" => "NOVITA_API_KEY",
            "ovhcloud" => "OVHCLOUD_API_KEY",
            "scaleway" => "SCALEWAY_API_KEY",
            "vultr" | "vultr-ai" => "VULTR_API_KEY",
            "baseten" => "BASETEN_API_KEY",
            // friendli adapter reads FRIENDLI_TOKEN (Friendli's docs use that
            // name), not FRIENDLI_API_KEY.
            "friendli" => "FRIENDLI_TOKEN",
            "upstage" => "UPSTAGE_API_KEY",
            "stepfun" => "STEPFUN_API_KEY",
            "fireworks" => "FIREWORKS_API_KEY",
            "minimax" => "MINIMAX_API_KEY",
            "synthetic" => "SYNTHETIC_API_KEY",
            "routing" => "ROUTING_API_KEY",
            "neuralwatt" => "NEURALWATT_API_KEY",
            "meta" => "META_API_KEY",
            "coreweave" => "WANDB_API_KEY",
            "sakana" => "SAKANA_API_KEY",
            "gmi-cloud" => "GMI_CLOUD_API_KEY",
            "nanogpt" => "NANOGPT_API_KEY",
            "zenmux" => "ZENMUX_API_KEY",
            "vercel-ai-gateway" => "AI_GATEWAY_API_KEY",
            "umans" => "UMANS_API_KEY",
            "qianfan" => "QIANFAN_API_KEY",
            "wafer-serverless" => "WAFER_API_KEY",
            "litellm" => "LITELLM_API_KEY",
            "ollama-cloud" => "OLLAMA_API_KEY",
            "vllm" => "", // No API key required unless the server sets one
            "cloudflare-ai-gateway" => "CLOUDFLARE_API_TOKEN",
            "bedrock-mantle" => "AWS_BEARER_TOKEN_BEDROCK",
            "minimax-code" => "MINIMAX_CODE_API_KEY",
            "minimax-code-cn" => "MINIMAX_CODE_CN_API_KEY",
            "xiaomi" => "XIAOMI_API_KEY",
            "custom-openai" => "CUSTOM_OPENAI_API_KEY",
            "custom-anthropic" => "CUSTOM_ANTHROPIC_API_KEY",
            "ollama" | "lm-studio" | "llama-cpp" => "", // No API key required
            _ => return None,
        };
        if env_var.is_empty() {
            None
        } else {
            std::env::var(env_var).ok().filter(|k| !k.is_empty())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthStore, StoredCredential};

    #[test]
    fn github_copilot_oauth_prefers_refresh_token() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "github-copilot".to_string(),
            StoredCredential::OAuthToken {
                access: "access-token".to_string(),
                refresh: "refresh-token".to_string(),
                expires: 0,
            },
        );

        assert_eq!(
            store.api_key_for("github-copilot").as_deref(),
            Some("refresh-token")
        );
    }

    fn anthropic_tokens(scopes: &[&str], api_key: Option<&str>) -> crate::oauth::OAuthTokens {
        crate::oauth::OAuthTokens {
            access_token: "access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            email: Some("work@example.com".to_string()),
            api_key: api_key.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn every_credential_shape_survives_a_round_trip() {
        // The store is now the only place a credential lives, so a field lost
        // in serialisation is a credential the account can never present.
        let mut store = AuthStore::default();
        store.credentials.insert(
            "gateway".to_string(),
            StoredCredential::ApiKey { key: "sk-1".into() },
        );
        store.credentials.insert(
            "kerem".to_string(),
            StoredCredential::OAuthToken {
                access: "gho-a".into(),
                refresh: "gho-r".into(),
                expires: 7,
            },
        );
        store.credentials.insert(
            "work".to_string(),
            StoredCredential::AnthropicOAuth(anthropic_tokens(&["user:inference"], None)),
        );
        store.credentials.insert(
            "chatgpt".to_string(),
            StoredCredential::CodexOAuth(crate::oauth_config::CodexTokens {
                access_token: "codex-a".into(),
                refresh_token: Some("codex-r".into()),
                account_id: Some("acct-1".into()),
                expires_at: Some(99),
            }),
        );

        let json = serde_json::to_string(&store).expect("serialise");
        let back: AuthStore = serde_json::from_str(&json).expect("deserialise");

        assert_eq!(back.api_key_for("gateway").as_deref(), Some("sk-1"));
        let tokens = back.anthropic_tokens("work").expect("anthropic account");
        assert_eq!(tokens.email.as_deref(), Some("work@example.com"));
        assert_eq!(tokens.scopes, vec!["user:inference".to_string()]);
        let codex = back.codex_tokens("chatgpt").expect("codex account");
        assert_eq!(codex.refresh_token.as_deref(), Some("codex-r"));
        assert_eq!(codex.expires_at, Some(99));
    }

    #[test]
    fn an_anthropic_account_presents_what_its_scopes_call_for() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "subscription".to_string(),
            StoredCredential::AnthropicOAuth(anthropic_tokens(&["user:inference"], None)),
        );
        store.credentials.insert(
            "console".to_string(),
            StoredCredential::AnthropicOAuth(anthropic_tokens(
                &["org:create_api_key"],
                Some("sk-ant-minted"),
            )),
        );

        assert_eq!(
            store
                .api_key_for_protocol("subscription", "anthropic")
                .as_deref(),
            Some("access-token"),
            "a claude.ai token is presented as the Bearer itself"
        );
        assert_eq!(
            store
                .api_key_for_protocol("console", "anthropic")
                .as_deref(),
            Some("sk-ant-minted"),
            "a console account presents the key it minted, not the access token"
        );
    }

    #[test]
    fn accounts_are_grouped_by_the_credential_they_hold() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "gateway".to_string(),
            StoredCredential::ApiKey { key: "sk-1".into() },
        );
        store.credentials.insert(
            "personal".to_string(),
            StoredCredential::AnthropicOAuth(anthropic_tokens(&["user:inference"], None)),
        );
        store.credentials.insert(
            "work".to_string(),
            StoredCredential::AnthropicOAuth(anthropic_tokens(&["user:inference"], None)),
        );

        assert_eq!(
            store.accounts_for_protocol("anthropic"),
            vec!["personal".to_string(), "work".to_string()]
        );
        assert!(store.accounts_for_protocol("codex").is_empty());
    }

    #[test]
    fn a_copilot_token_is_read_under_the_account_it_was_filed_under() {
        // A second Copilot login is stored under its GitHub name, so the OAuth
        // branch has to key off the protocol rather than the account name.
        let mut store = AuthStore::default();
        store.credentials.insert(
            "kerem".to_string(),
            StoredCredential::OAuthToken {
                access: "access-token".to_string(),
                refresh: "refresh-token".to_string(),
                expires: 0,
            },
        );

        assert_eq!(
            store
                .api_key_for_protocol("kerem", "github-copilot")
                .as_deref(),
            Some("refresh-token")
        );
        assert!(
            store.api_key_for("kerem").is_none(),
            "without the protocol there is nothing that says how to read it"
        );
    }

    #[test]
    fn api_key_for_regular_provider_uses_stored_key() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            "openrouter".to_string(),
            StoredCredential::ApiKey {
                key: "or-key".to_string(),
            },
        );

        assert_eq!(store.api_key_for("openrouter").as_deref(), Some("or-key"));
    }

    // -----------------------------------------------------------------------
    // The workspace session
    // -----------------------------------------------------------------------

    use super::{now_secs, WORKSPACE_ACCOUNT};

    fn with_session(url: &str, expires: u64) -> AuthStore {
        let mut store = AuthStore::default();
        store.credentials.insert(
            WORKSPACE_ACCOUNT.to_string(),
            StoredCredential::WorkspaceSession {
                url: url.to_string(),
                token: "session-token".to_string(),
                expires,
            },
        );
        store
    }

    #[test]
    fn a_live_session_is_answered() {
        let store = with_session("https://mikmik.firma.com", now_secs() + 3600);
        assert_eq!(
            store.workspace_session("https://mikmik.firma.com"),
            Some("session-token")
        );
    }

    #[test]
    fn a_session_is_not_handed_to_another_server() {
        // Otherwise editing one line of `settings.json` would send the
        // organisation's session token to an address of the editor's choosing.
        let store = with_session("https://mikmik.firma.com", now_secs() + 3600);
        assert_eq!(store.workspace_session("https://attacker.example"), None);
    }

    #[test]
    fn a_trailing_slash_is_not_a_different_server() {
        let store = with_session("https://mikmik.firma.com", now_secs() + 3600);
        assert_eq!(
            store.workspace_session("https://mikmik.firma.com/"),
            Some("session-token")
        );
    }

    #[test]
    fn an_expired_session_is_not_answered() {
        // The server would refuse it anyway; answering `None` here is what
        // lets the caller say "log in again" without a round trip.
        let store = with_session("https://mikmik.firma.com", now_secs() - 1);
        assert_eq!(store.workspace_session("https://mikmik.firma.com"), None);
    }

    #[test]
    fn nothing_stored_is_no_session() {
        assert_eq!(
            AuthStore::default().workspace_session("https://mikmik.firma.com"),
            None
        );
    }

    #[test]
    fn a_credential_of_another_shape_is_not_a_session() {
        let mut store = AuthStore::default();
        store.credentials.insert(
            WORKSPACE_ACCOUNT.to_string(),
            StoredCredential::ApiKey {
                key: "not-a-session".to_string(),
            },
        );
        assert_eq!(store.workspace_session("https://mikmik.firma.com"), None);
    }

    #[test]
    fn the_workspace_account_is_not_a_model_provider() {
        // It would otherwise turn up in the account list as something the
        // model could be pointed at, and it serves no completions.
        assert_eq!(
            super::implied_protocol(&StoredCredential::WorkspaceSession {
                url: "https://mikmik.firma.com".to_string(),
                token: "session-token".to_string(),
                expires: 0,
            }),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_stored_session_lands_in_an_owner_only_file() {
        // The token reaches every provider key the organisation assigned to
        // this account, so it must not be world-readable for a moment.
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;
        static HOME_LOCK: Mutex<()> = Mutex::new(());
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("temp dir");
        let prev_home = std::env::var_os("MIKMIK_HOME");
        std::env::set_var("MIKMIK_HOME", tmp.path());

        let mut store = AuthStore::load();
        store.set_workspace_session("https://mikmik.firma.com/", "session-token", 3600);

        let path = AuthStore::path();
        let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777);
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let reloaded = AuthStore::load();
        let live = reloaded
            .workspace_session("https://mikmik.firma.com")
            .map(str::to_string);

        let mut after_logout = AuthStore::load();
        after_logout.clear_workspace_session();
        let gone = AuthStore::load()
            .workspace_session("https://mikmik.firma.com")
            .is_none();
        let left_behind = std::fs::read_to_string(&path).unwrap_or_default();

        // Restore the config root before asserting, so a failure cannot leak
        // the override into the rest of the test binary.
        match prev_home {
            Some(value) => std::env::set_var("MIKMIK_HOME", value),
            None => std::env::remove_var("MIKMIK_HOME"),
        }

        assert_eq!(mode.expect("mode"), 0o600, "the session file is readable");
        assert_eq!(live.as_deref(), Some("session-token"));
        assert!(
            raw.contains("workspace-session"),
            "the credential was not written: {raw}"
        );
        assert!(gone, "logging out left the session behind");
        assert!(
            !left_behind.contains("session-token"),
            "the token survived a logout: {left_behind}"
        );
    }
}
