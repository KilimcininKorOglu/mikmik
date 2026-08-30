//! OAuth configuration for multiple environments.
//!
//! This module mirrors the TypeScript `src/constants/oauth.ts` and
//! `src/services/oauth/crypto.ts` constants.  It is intentionally
//! *configuration-only* — no live network I/O except for the optional
//! `fetch_oauth_profile` helper at the bottom.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Scope constants (mirrors constants/oauth.ts)
// ---------------------------------------------------------------------------

/// The Claude.ai inference scope — required for Bearer-auth API calls.
pub const CLAUDE_AI_INFERENCE_SCOPE: &str = "user:inference";

/// The profile scope — required to read account / subscription data.
pub const CLAUDE_AI_PROFILE_SCOPE: &str = "user:profile";

/// Console scope — used when creating an API key via the Console flow.
pub const CONSOLE_SCOPE: &str = "org:create_api_key";

/// All Claude.ai OAuth scopes (mirrors `CLAUDE_AI_OAUTH_SCOPES`).
pub const CLAUDE_AI_OAUTH_SCOPES: &[&str] = &[
    CLAUDE_AI_PROFILE_SCOPE,
    CLAUDE_AI_INFERENCE_SCOPE,
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// Console OAuth scopes (mirrors `CONSOLE_OAUTH_SCOPES`).
pub const CONSOLE_OAUTH_SCOPES: &[&str] = &[CONSOLE_SCOPE, CLAUDE_AI_PROFILE_SCOPE];

/// Union of all scopes used during login (mirrors `ALL_OAUTH_SCOPES`).
/// Requesting all at once lets a single login satisfy both Console and
/// claude.ai auth paths.
pub const ALL_OAUTH_SCOPES: &[&str] = &[
    CONSOLE_SCOPE,
    CLAUDE_AI_PROFILE_SCOPE,
    CLAUDE_AI_INFERENCE_SCOPE,
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// Minimum scopes required for basic operation.
pub const MINIMUM_SCOPES: &[&str] = &[CLAUDE_AI_INFERENCE_SCOPE, CLAUDE_AI_PROFILE_SCOPE];

// ---------------------------------------------------------------------------
// Claude Code stealth-impersonation constants
// ---------------------------------------------------------------------------

/// User-Agent advertised to Anthropic's API on OAuth-authenticated requests.
/// Must match a Claude Code version the server still accepts; bump when
/// Anthropic invalidates the current value.
pub const CLAUDE_CODE_VERSION_FOR_OAUTH: &str = "2.1.246";

/// Anthropic SDK version Claude Code bundles, sent as `x-stainless-package-version`
/// on OAuth-authenticated requests. Paired with `CLAUDE_CODE_VERSION_FOR_OAUTH`:
/// an outdated pair is rejected with `403 Request not allowed`, so bump both
/// together to the values the current Claude Code release sends.
pub const CLAUDE_CODE_SDK_VERSION: &str = "0.112.1";

/// `anthropic-beta` flags for OAuth requests, in the exact order the official
/// `claude` sends on a Pro account. The first two are required for the server to
/// accept subscription tokens; the rest are additive capability flags. Max /
/// extra-usage accounts get two more (`OAUTH_BETA_FLAGS_MAX_EXTRA`); sending
/// `context-1m` on Pro-without-credits triggers a 429.
pub const OAUTH_BETA_FLAGS: &[&str] = &[
    "claude-code-20250219",
    "oauth-2025-04-20",
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "advisor-tool-2026-03-01",
    "advanced-tool-use-2025-11-20",
    "effort-2025-11-24",
    "afk-mode-2026-01-31",
    "extended-cache-ttl-2025-04-11",
    "cache-diagnosis-2026-04-07",
];

/// Betas added on Max / extra-usage accounts, inserted after
/// `prompt-caching-scope-2026-01-05` to keep the official ordering.
pub const OAUTH_BETA_FLAGS_MAX_EXTRA: &[&str] = &[
    "context-1m-2025-08-07",
    "mid-conversation-system-2026-04-07",
];

/// Build the `anthropic-beta` flag list matching the official client for the
/// account's tier. `has_premium` = account has 1M-context / extra-usage
/// entitlement (Max or extra-usage enabled).
pub fn oauth_beta_flags(has_premium: bool) -> Vec<&'static str> {
    if !has_premium {
        return OAUTH_BETA_FLAGS.to_vec();
    }
    let mut v = Vec::with_capacity(OAUTH_BETA_FLAGS.len() + 2);
    for &f in OAUTH_BETA_FLAGS {
        v.push(f);
        if f == "prompt-caching-scope-2026-01-05" {
            v.extend_from_slice(OAUTH_BETA_FLAGS_MAX_EXTRA);
        }
    }
    v
}

/// User-Agent for OAuth requests: `claude-cli/<version> (external, cli)`.
pub fn claude_code_user_agent() -> String {
    format!("claude-cli/{CLAUDE_CODE_VERSION_FOR_OAUTH} (external, cli)")
}

/// User-Agent the official client sends on the OAuth **token refresh** call
/// (distinct from the inference `claude_code_user_agent`): the stainless SDK
/// identity `anthropic-sdk-typescript/<sdk-version> userOAuthProvider`. The
/// upstream rejects a refresh whose SDK version drifts from the current
/// release, so it is pinned to the same `CLAUDE_CODE_SDK_VERSION`.
pub fn claude_code_refresh_user_agent() -> String {
    format!("anthropic-sdk-typescript/{CLAUDE_CODE_SDK_VERSION} userOAuthProvider")
}

/// Salt baked into the official bundle (a minified JS var), used in the
/// `cc_version` client hash. Re-extracted and bumped per release by
/// `claude-re/scripts/update-claude-code.sh`. See `findings/CCH-NATIVE.md`.
pub const CLAUDE_CODE_BILLING_SALT: &str = "59cf53e54c78";

/// The official CLI's real client hash: the `cc_version` suffix, i.e.
/// `sha256(SALT + text[4] + text[7] + text[20] + VERSION)[..3]`, where `text`
/// is the first user (non-meta) message. (`cch` itself is always literal
/// `00000`.) See `findings/CCH-NATIVE.md`.
pub fn claude_code_cc_version_suffix(first_user_text: &str) -> String {
    use sha2::{Digest, Sha256};
    let chars: Vec<char> = first_user_text.chars().collect();
    // Out-of-range index -> "0", matching JS `H[z] || "0"`.
    let pick = |z: usize| {
        chars
            .get(z)
            .map_or_else(|| "0".to_string(), |c| c.to_string())
    };
    let k = format!("{}{}{}", pick(4), pick(7), pick(20));
    let input = format!("{CLAUDE_CODE_BILLING_SALT}{k}{CLAUDE_CODE_VERSION_FOR_OAUTH}");
    hex::encode(Sha256::digest(input.as_bytes()))[..3].to_string()
}

/// The `x-anthropic-billing-header` block (`system[0]`): `cc_version` + client
/// hash suffix, `cc_entrypoint=cli`, and the literal `cch=00000` (the CLI never
/// computes a cch). See `findings/CCH-NATIVE.md`.
pub fn claude_code_billing_header(first_user_text: &str) -> String {
    let suffix = claude_code_cc_version_suffix(first_user_text);
    format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CODE_VERSION_FOR_OAUTH}.{suffix}; cc_entrypoint=cli; cch=00000;"
    )
}

/// System-prompt prefix that must appear as the first system block on every
/// OAuth-authenticated request. Anthropic's gate refuses requests whose system
/// prompt does not start with this identity string.
pub const CLAUDE_CODE_SYSTEM_PROMPT_PREFIX: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// Stable per-install device id for the OAuth `metadata.user_id`:
/// `sha256(user:home)` in hex. Deterministic across a machine so a request's
/// attribution matches the streaming client for the same account.
pub fn anthropic_device_id() -> String {
    use sha2::{Digest, Sha256};
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let mut h = Sha256::new();
    h.update(user.as_bytes());
    h.update(b":");
    h.update(home.as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// OAuthConfig struct
// ---------------------------------------------------------------------------

/// Full OAuth configuration for a deployment environment.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub base_api_url: &'static str,
    pub console_authorize_url: &'static str,
    pub claude_ai_authorize_url: &'static str,
    /// The raw claude.ai web origin (separate from the authorize URL which
    /// may bounce through claude.com for attribution).
    pub claude_ai_origin: &'static str,
    pub token_url: &'static str,
    pub api_key_url: &'static str,
    pub roles_url: &'static str,
    pub console_success_url: &'static str,
    pub claudeai_success_url: &'static str,
    pub manual_redirect_url: &'static str,
    pub client_id: &'static str,
    pub oauth_file_suffix: &'static str,
    pub mcp_proxy_url: &'static str,
    pub mcp_proxy_path: &'static str,
}

// ---------------------------------------------------------------------------
// Production config (mirrors PROD_OAUTH_CONFIG in oauth.ts)
// ---------------------------------------------------------------------------

// Claude Code OAuth client ID, used in stealth-impersonation mode so that
// Anthropic's auth server accepts Claude Pro/Max tokens through MikMik.
// The matching request-time impersonation (user-agent, x-app, anthropic-beta,
// and the Claude Code system-prompt prefix) is wired up in
// `mikmik_api::client::AnthropicClient` and is required for these tokens to
// be honoured by the API.
//
// Billing note (verified live 2026-06-03, Claude Pro account, extra-usage
// disabled): a Pro/Max subscription token used through MikMik's impersonation
// IS served and DRAWS FROM THE INTERACTIVE SUBSCRIPTION QUOTA — `oauth/usage`
// `five_hour`/`seven_day` utilisation climbed (percentage, 0-100 scale) with
// extra-usage off and `seven_day_oauth_apps` staying null. A side-by-side run of
// the official `claude -p` (same token via CLAUDE_CODE_OAUTH_TOKEN) hit the same
// buckets, i.e. MikMik is billed exactly like the official interactive client.
// This CONTRADICTS the earlier assumption that third-party usage falls back to
// the "extra usage" pool. The CCH was not even required (requests succeeded
// without it). Caveats: (1) tested before Anthropic's 2026-06-15 dual-bucket
// change, which routes `claude -p`/Agent-SDK/third-party usage to a separate
// monthly API-rate credit — re-verify after that date; (2) advertising the
// `context-1m` beta forces long-context requests onto extra-usage credits (429
// "Usage credits are required for long context requests"), so it is omitted from
// OAUTH_BETA_FLAGS.
pub const PROD_OAUTH: OAuthConfig = OAuthConfig {
    base_api_url: "https://api.anthropic.com",
    // Routes through claude.com/cai/* for attribution, 307s to claude.ai in
    // two hops — same behaviour as the TypeScript client.
    console_authorize_url: "https://platform.claude.com/oauth/authorize",
    claude_ai_authorize_url: "https://claude.com/cai/oauth/authorize",
    claude_ai_origin: "https://claude.ai",
    token_url: "https://platform.claude.com/v1/oauth/token",
    api_key_url: "https://api.anthropic.com/api/oauth/claude_cli/create_api_key",
    roles_url: "https://api.anthropic.com/api/oauth/claude_cli/roles",
    console_success_url:
        "https://platform.claude.com/buy_credits?returnUrl=/oauth/code/success%3Fapp%3Dclaude-code",
    claudeai_success_url: "https://platform.claude.com/oauth/code/success?app=claude-code",
    manual_redirect_url: "https://platform.claude.com/oauth/code/callback",
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e", // Claude Code client ID (stealth)
    oauth_file_suffix: "",
    mcp_proxy_url: "https://mcp-proxy.anthropic.com",
    mcp_proxy_path: "/v1/mcp/{server_id}",
};

// ---------------------------------------------------------------------------
// Staging config (mirrors STAGING_OAUTH_CONFIG — ant builds only)
// ---------------------------------------------------------------------------

pub const STAGING_OAUTH: OAuthConfig = OAuthConfig {
    base_api_url: "https://api-staging.anthropic.com",
    console_authorize_url: "https://platform.staging.ant.dev/oauth/authorize",
    claude_ai_authorize_url: "https://claude-ai.staging.ant.dev/oauth/authorize",
    claude_ai_origin: "https://claude-ai.staging.ant.dev",
    token_url: "https://platform.staging.ant.dev/v1/oauth/token",
    api_key_url: "https://api-staging.anthropic.com/api/oauth/claude_cli/create_api_key",
    roles_url: "https://api-staging.anthropic.com/api/oauth/claude_cli/roles",
    console_success_url: "https://platform.staging.ant.dev/buy_credits?returnUrl=/oauth/code/success%3Fapp%3Dclaude-code",
    claudeai_success_url: "https://platform.staging.ant.dev/oauth/code/success?app=claude-code",
    manual_redirect_url: "https://platform.staging.ant.dev/oauth/code/callback",
    client_id: "22422756-60c9-4084-8eb7-27705fd5cf9a", // Claude Code staging client ID (stealth)
    oauth_file_suffix: "-staging-oauth",
    mcp_proxy_url: "https://mcp-proxy-staging.anthropic.com",
    mcp_proxy_path: "/v1/mcp/{server_id}",
};

/// Client-ID Metadata Document URL for MCP OAuth (CIMD / SEP-991).
pub const MCP_CLIENT_METADATA_URL: &str = "https://claude.ai/oauth/claude-code-client-metadata";

// ---------------------------------------------------------------------------
// Config selection
// ---------------------------------------------------------------------------

/// Return the OAuth config appropriate for the current environment.
///
/// Free-code always uses production OAuth. The `USER_TYPE=ant` gate and
/// staging variant have been removed for the OSS/free build.
pub fn get_oauth_config() -> &'static OAuthConfig {
    &PROD_OAUTH
}

// ---------------------------------------------------------------------------
// PKCE helpers (mirrors src/services/oauth/crypto.ts)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Token and profile types
// ---------------------------------------------------------------------------

/// Raw OAuth token response from the token endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Slim profile fetched after token exchange.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuthProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_tier: Option<String>,
}

/// Fetch the OAuth profile using an access token.
///
/// Returns a default (all-`None`) profile on any non-success response so
/// callers can treat a profile fetch failure as non-fatal.
pub async fn fetch_oauth_profile(
    access_token: &str,
    api_base: &str,
) -> anyhow::Result<OAuthProfile> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/auth/oauth/profile", api_base.trim_end_matches('/'));

    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;

    if resp.status().is_success() {
        let profile: OAuthProfile = resp.json().await.unwrap_or_default();
        Ok(profile)
    } else {
        // Non-fatal: return an empty profile so the caller can continue.
        Ok(OAuthProfile::default())
    }
}

// ---------------------------------------------------------------------------
// Auth URL builder
// ---------------------------------------------------------------------------

/// Build the OAuth authorization URL (mirrors `buildAuthUrl` in client.ts).
pub fn build_auth_url(
    code_challenge: &str,
    state: &str,
    port: u16,
    is_manual: bool,
    login_with_claude_ai: bool,
    inference_only: bool,
) -> String {
    let cfg = get_oauth_config();

    let base = if login_with_claude_ai {
        cfg.claude_ai_authorize_url
    } else {
        cfg.console_authorize_url
    };

    let redirect_uri = if is_manual {
        cfg.manual_redirect_url.to_string()
    } else {
        format!("http://localhost:{}/callback", port)
    };

    let scopes: Vec<&str> = if inference_only {
        vec![CLAUDE_AI_INFERENCE_SCOPE]
    } else {
        ALL_OAUTH_SCOPES.to_vec()
    };

    let scope_str = scopes.join(" ");

    format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        base,
        urlencoding::encode(cfg.client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(&scope_str),
        urlencoding::encode(code_challenge),
        urlencoding::encode(state),
    )
}

// ---------------------------------------------------------------------------
// Codex (OpenAI) OAuth Token Storage
// ---------------------------------------------------------------------------

/// OpenAI Codex OAuth tokens, persisted to ~/.config/mikmik/codex_tokens.json
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodexTokens {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Unix timestamp in seconds when the access token expires
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// Legacy single-file path: `~/.config/mikmik/codex_tokens.json`, read once by the
/// startup migration and never written.
pub fn codex_tokens_path() -> std::path::PathBuf {
    crate::config::Settings::config_dir().join("codex_tokens.json")
}

/// Save Codex OAuth tokens under `account_id` in the auth store.
pub fn save_codex_tokens_for_account(tokens: &CodexTokens, account_id: &str) -> anyhow::Result<()> {
    let mut store = crate::AuthStore::load();
    store.set_codex_tokens(account_id, tokens.clone());
    Ok(())
}

/// Load the Codex tokens stored for `account_id`, or `None` when that account
/// holds a credential of another kind.
pub fn load_codex_tokens_for_account(account_id: &str) -> Option<CodexTokens> {
    crate::AuthStore::load().codex_tokens(account_id).cloned()
}

/// Save Codex OAuth tokens under an account, open its `providers` entry, and
/// make it the active account. Returns the account name used.
///
/// The name comes from `label` when given, otherwise from the JWT identity.
/// Logging in again with the same identity refreshes that account in place.
pub fn save_codex_tokens_and_register(
    tokens: &CodexTokens,
    label: Option<&str>,
) -> anyhow::Result<String> {
    use crate::accounts::jwt_identity;
    use crate::provider_id::ProviderId;

    let identity = jwt_identity(&tokens.access_token);
    let settings = crate::config::Settings::load_sync().unwrap_or_default();
    let config = settings.effective_config();
    let store = crate::AuthStore::load();

    // Same email or account id means the same account, whatever it was named
    // when it was first stored.
    let existing_id = store
        .accounts_for_protocol(ProviderId::CODEX)
        .into_iter()
        .find(|id| {
            store.codex_tokens(id).is_some_and(|stored| {
                let stored_identity = jwt_identity(&stored.access_token);
                (identity.email.is_some() && stored_identity.email == identity.email)
                    || (tokens.account_id.is_some() && stored.account_id == tokens.account_id)
                    || (identity.account_id.is_some()
                        && stored_identity.account_id == identity.account_id)
            })
        });

    let id = match existing_id {
        Some(id) => id,
        None => {
            let base = label.map(str::to_string).unwrap_or_else(|| {
                identity
                    .email
                    .as_deref()
                    .map(|e| e.split('@').next().unwrap_or(e).to_string())
                    .or_else(|| tokens.account_id.clone())
                    .or_else(|| identity.account_id.clone())
                    .unwrap_or_else(|| "account".to_string())
            });
            config.account_name_for_login(&base, ProviderId::CODEX)
        }
    };

    save_codex_tokens_for_account(tokens, &id)?;
    crate::config::register_account(&id, ProviderId::CODEX, true)?;
    Ok(id)
}

/// Save to the active account, registering a new one when the active account
/// is not a Codex account.
pub fn save_codex_tokens(tokens: &CodexTokens) -> anyhow::Result<()> {
    match active_codex_account() {
        Some(active) => save_codex_tokens_for_account(tokens, &active),
        None => save_codex_tokens_and_register(tokens, None).map(|_| ()),
    }
}

/// Load the active account's Codex tokens.
///
/// Falls back to the only stored Codex account when the session is pointed
/// elsewhere, because a Codex request reaches this path through
/// `CodexProvider::from_stored` even while another account is active.
pub fn get_codex_tokens() -> Option<CodexTokens> {
    let store = crate::AuthStore::load();
    if let Some(active) = active_codex_account() {
        return store.codex_tokens(&active).cloned();
    }
    let accounts = store.accounts_for_protocol(crate::provider_id::ProviderId::CODEX);
    match accounts.as_slice() {
        [only] => store.codex_tokens(only).cloned(),
        _ => None,
    }
}

/// The active account, when it is a Codex account.
fn active_codex_account() -> Option<String> {
    let settings = crate::config::Settings::load_sync().ok()?;
    let active = settings.provider.clone()?;
    crate::AuthStore::load()
        .codex_tokens(&active)
        .map(|_| active)
}

/// Drop the active Codex account: its credential and its `providers` entry.
pub fn clear_codex_tokens() -> anyhow::Result<()> {
    if let Some(active) = active_codex_account() {
        crate::AuthStore::load().remove(&active);
        crate::config::forget_account(&active)?;
    }
    Ok(())
}

/// Returns true if the user has a valid Codex access token.
/// Tokens are obtained via `/connect → OpenAI Codex` (browser OAuth flow)
/// or by setting `MIKMIK_USE_OPENAI=1` with a manually stored token.
pub fn is_codex_subscriber() -> bool {
    get_codex_tokens()
        .map(|t| !t.access_token.is_empty())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert the OAuth fingerprint as literal bytes, never against the
    /// constants production emits, so a wrong bump moves the constant and the
    /// test apart instead of together. This exact pair is what Anthropic
    /// accepts for subscription OAuth; a stale pair is rejected with 403.
    #[test]
    fn the_oauth_fingerprint_pins_the_current_claude_code_release() {
        assert_eq!(CLAUDE_CODE_VERSION_FOR_OAUTH, "2.1.246");
        assert_eq!(CLAUDE_CODE_SDK_VERSION, "0.112.1");
        assert_eq!(
            claude_code_user_agent(),
            "claude-cli/2.1.246 (external, cli)"
        );
        // The token-refresh call carries a different User-Agent than inference:
        // the stainless SDK identity, pinned to the same SDK version.
        assert_eq!(
            claude_code_refresh_user_agent(),
            "anthropic-sdk-typescript/0.112.1 userOAuthProvider"
        );
        assert!(OAUTH_BETA_FLAGS.contains(&"oauth-2025-04-20"));
    }

    #[test]
    fn test_prod_config_urls_are_https() {
        assert!(PROD_OAUTH.token_url.starts_with("https://"));
        assert!(PROD_OAUTH.api_key_url.starts_with("https://"));
        assert!(PROD_OAUTH.claude_ai_authorize_url.starts_with("https://"));
    }

    #[test]
    fn test_staging_config_urls_are_https() {
        assert!(STAGING_OAUTH.token_url.starts_with("https://"));
        assert!(STAGING_OAUTH.api_key_url.starts_with("https://"));
    }

    #[test]
    fn test_all_oauth_scopes_contains_inference() {
        assert!(ALL_OAUTH_SCOPES.contains(&CLAUDE_AI_INFERENCE_SCOPE));
    }

    #[test]
    fn test_build_auth_url_contains_required_params() {
        let url = build_auth_url("challenge123", "state456", 8080, false, true, false);
        assert!(url.contains("challenge123"));
        assert!(url.contains("state456"));
        assert!(url.contains("S256"));
        assert!(url.contains("localhost"));
    }

    /// Golden vector cross-checking the `b1q` port. `EXPECTED_SUFFIX` is
    /// auto-maintained per version by `claude-re/scripts/update-claude-code.sh`
    /// (independent shell `sha256`); first verified live for 2.1.161 (== `9f1`).
    #[test]
    fn test_cc_version_suffix_golden() {
        // Chars at indices 4/7/20 are 'n', ' ', 'a'.
        const GOLDEN_INPUT: &str = "Réponds uniquement par le mot: PROXYTEST";
        const EXPECTED_SUFFIX: &str = "a81"; // AUTO-MAINTAINED: cc_version suffix for 2.1.246
        assert_eq!(claude_code_cc_version_suffix(GOLDEN_INPUT), EXPECTED_SUFFIX);

        let h = claude_code_billing_header(GOLDEN_INPUT);
        assert_eq!(
            h,
            format!("x-anthropic-billing-header: cc_version={CLAUDE_CODE_VERSION_FOR_OAUTH}.{EXPECTED_SUFFIX}; cc_entrypoint=cli; cch=00000;")
        );
        assert!(h.contains("cch=00000;"));
    }

    #[test]
    fn test_cc_version_suffix_short_text_uses_zero_padding() {
        // Indices 7 and 20 are out of range -> "0"; must not panic.
        let s = claude_code_cc_version_suffix("abcd");
        assert_eq!(s.len(), 3);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
