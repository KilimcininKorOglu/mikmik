//! gitlab_duo.rs — GitLab Duo OAuth (PKCE loopback) and AI-gateway access.
//!
//! GitLab Duo is reached in two steps. First the user signs in: either the
//! OAuth 2.0 authorization-code grant with PKCE against `gitlab.com` (browser
//! plus a loopback callback), or a Personal Access Token supplied in
//! `GITLAB_TOKEN`. Either way the result is a GitLab access token.
//!
//! That token is not used directly for inference. At request time it is
//! exchanged at `gitlab.com/api/v4/ai/third_party_agents/direct_access` for a
//! short-lived *direct-access* token plus a set of gateway headers, and those
//! are what authenticate the OpenAI-compatible proxy at
//! `cloud.gitlab.com/ai/v1/proxy/openai/v1`.
//!
//! Network I/O lives here; the authorize-URL builder, token-response parser,
//! direct-access parser and expiry check are pure so they can be tested
//! without a live endpoint.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// GitLab.com origin (auth + direct-access exchange).
pub const GITLAB_URL: &str = "https://gitlab.com";

/// The AI gateway that fronts the model proxies.
pub const AI_GATEWAY_URL: &str = "https://cloud.gitlab.com";

/// The OpenAI-compatible proxy base.
pub const OPENAI_PROXY_URL: &str = "https://cloud.gitlab.com/ai/v1/proxy/openai/v1";

/// The bundled OAuth client id. GitLab rejects the authorize request when this
/// client id's registered redirect URI drifts from the default below; a user in
/// that case sets `GITLAB_CLIENT_ID` + `GITLAB_REDIRECT_URI`, or supplies a
/// `GITLAB_TOKEN` and skips OAuth entirely.
pub const DEFAULT_CLIENT_ID: &str =
    "da4edff2e6ebd2bc3208611e2768bc1c1dd7be791dc5ff26ca34ca9ee44f7d4b";

/// The redirect URI the bundled client id is registered with.
pub const DEFAULT_REDIRECT_URI: &str = "http://localhost:8080/callback";

/// The default loopback port (matches `DEFAULT_REDIRECT_URI`).
pub const DEFAULT_CALLBACK_PORT: u16 = 8080;

/// The OAuth scope GitLab Duo needs.
pub const OAUTH_SCOPE: &str = "api";

/// Treat a token as expired this many seconds early.
const EXPIRY_SKEW_SECS: u64 = 300;

/// How long a direct-access token is treated as valid before re-exchange.
pub const DIRECT_ACCESS_TTL_SECS: u64 = 30 * 60;

/// The OAuth client id in effect (`GITLAB_CLIENT_ID` overrides the bundled one).
pub fn client_id() -> String {
    std::env::var("GITLAB_CLIENT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

/// The redirect URI in effect (`GITLAB_REDIRECT_URI` overrides the default).
pub fn redirect_uri() -> String {
    std::env::var("GITLAB_REDIRECT_URI")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string())
}

/// The loopback port to bind, parsed from the redirect URI's port.
pub fn callback_port() -> u16 {
    redirect_uri()
        .parse::<url::Url>()
        .ok()
        .and_then(|u| u.port())
        .unwrap_or(DEFAULT_CALLBACK_PORT)
}

/// A Personal Access Token supplied in `GITLAB_TOKEN`, if any. This is the
/// OAuth-free path: the PAT is used as the GitLab access token directly.
pub fn pat_from_env() -> Option<String> {
    std::env::var("GITLAB_TOKEN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// GitLab OAuth tokens, persisted in the auth store under an account.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitlabTokens {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix timestamp in seconds when the access token expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// A direct-access grant: the token and the gateway headers to send with it.
#[derive(Debug, Clone)]
pub struct DirectAccess {
    pub token: String,
    pub headers: BTreeMap<String, String>,
}

/// Whether `tokens` should be refreshed before use.
pub fn is_expired(tokens: &GitlabTokens) -> bool {
    let Some(expires_at) = tokens.expires_at else {
        return false;
    };
    now_secs() + EXPIRY_SKEW_SECS >= expires_at
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Build the OAuth authorization URL for the PKCE loopback flow.
pub fn build_authorize_url(code_challenge: &str, state: &str) -> String {
    let mut url = format!("{GITLAB_URL}/oauth/authorize");
    url.push_str(&format!("?client_id={}", urlencoding::encode(&client_id())));
    url.push_str(&format!(
        "&redirect_uri={}",
        urlencoding::encode(&redirect_uri())
    ));
    url.push_str("&response_type=code");
    url.push_str(&format!("&scope={}", urlencoding::encode(OAUTH_SCOPE)));
    url.push_str(&format!("&state={}", urlencoding::encode(state)));
    url.push_str(&format!(
        "&code_challenge={}",
        urlencoding::encode(code_challenge)
    ));
    url.push_str("&code_challenge_method=S256");
    url
}

/// Turn a GitLab token-endpoint payload into [`GitlabTokens`].
pub fn parse_token_response(
    json: &serde_json::Value,
    refresh_fallback: Option<&str>,
) -> Result<GitlabTokens, String> {
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "GitLab token response missing access_token".to_string())?
        .to_string();

    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| refresh_fallback.map(str::to_string));

    // GitLab returns `created_at` (issue time) and `expires_in` (lifetime).
    let created_at = json.get("created_at").and_then(|v| v.as_u64());
    let expires_at = json.get("expires_in").and_then(|v| v.as_u64()).map(|secs| {
        let base = created_at.unwrap_or_else(now_secs);
        base + secs
    });

    Ok(GitlabTokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

/// Parse a direct-access response (pure, for tests and the live call).
pub fn parse_direct_access(json: &serde_json::Value) -> Result<DirectAccess, String> {
    let token = json
        .get("token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "GitLab Duo direct-access response missing token".to_string())?
        .to_string();
    let headers_obj = json
        .get("headers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "GitLab Duo direct-access response missing headers".to_string())?;
    let mut headers = BTreeMap::new();
    for (name, value) in headers_obj {
        if let Some(value) = value.as_str() {
            headers.insert(name.clone(), value.to_string());
        }
    }
    Ok(DirectAccess { token, headers })
}

/// Exchange an authorization code for tokens (PKCE loopback).
pub async fn exchange_code(code: &str, verifier: &str) -> Result<GitlabTokens, String> {
    let client = reqwest::Client::new();
    let redirect = redirect_uri();
    let cid = client_id();
    let resp = client
        .post(format!("{GITLAB_URL}/oauth/token"))
        .header("Accept", "application/json")
        .form(&[
            ("client_id", cid.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("GitLab token exchange request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GitLab token exchange failed: {} {text}",
            status.as_u16()
        ));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitLab token response was not JSON: {e}"))?;
    parse_token_response(&json, None)
}

/// Exchange a refresh token for fresh tokens.
pub async fn refresh(refresh_token: &str) -> Result<GitlabTokens, String> {
    let client = reqwest::Client::new();
    let cid = client_id();
    let resp = client
        .post(format!("{GITLAB_URL}/oauth/token"))
        .header("Accept", "application/json")
        .form(&[
            ("client_id", cid.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|e| format!("GitLab token refresh request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GitLab token refresh failed: {} {text}",
            status.as_u16()
        ));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitLab token refresh response was not JSON: {e}"))?;
    parse_token_response(&json, Some(refresh_token))
}

/// Exchange a GitLab access token for a direct-access token and gateway headers.
pub async fn direct_access(gitlab_access_token: &str) -> Result<DirectAccess, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{GITLAB_URL}/api/v4/ai/third_party_agents/direct_access"
        ))
        .header("Authorization", format!("Bearer {gitlab_access_token}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "feature_flags": { "DuoAgentPlatformNext": true } }))
        .send()
        .await
        .map_err(|e| format!("GitLab Duo direct-access request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 403 {
            return Err(format!(
                "GitLab Duo access denied. Ensure Duo is enabled for this account. {text}"
            ));
        }
        return Err(format!(
            "GitLab Duo direct-access failed: {} {text}",
            status.as_u16()
        ));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitLab Duo direct-access response was not JSON: {e}"))?;
    parse_direct_access(&json)
}

// ---------------------------------------------------------------------------
// Account storage
// ---------------------------------------------------------------------------

/// Save GitLab tokens under `account_id` (persists immediately).
pub fn save_gitlab_tokens_for_account(
    tokens: &GitlabTokens,
    account_id: &str,
) -> anyhow::Result<()> {
    let mut store = crate::AuthStore::load();
    store.set_gitlab_tokens(account_id, tokens.clone());
    Ok(())
}

/// Load the GitLab tokens stored for `account_id`.
pub fn load_gitlab_tokens_for_account(account_id: &str) -> Option<GitlabTokens> {
    crate::AuthStore::load().gitlab_tokens(account_id).cloned()
}

/// Save GitLab tokens, open the account's `providers` entry, make it active.
/// Returns the account id used. The account is named `gitlab-duo` since the
/// GitLab token carries no readable identity locally.
pub fn save_gitlab_tokens_and_register(tokens: &GitlabTokens) -> anyhow::Result<String> {
    use crate::provider_id::ProviderId;

    let settings = crate::config::Settings::load_sync().unwrap_or_default();
    let config = settings.effective_config();
    let id = config.account_name_for_login("gitlab", ProviderId::GITLAB_DUO);
    save_gitlab_tokens_for_account(tokens, &id)?;
    crate::config::register_account(&id, ProviderId::GITLAB_DUO, true)?;
    Ok(id)
}

/// The active account, when it is a GitLab Duo account.
pub fn active_gitlab_account() -> Option<String> {
    let settings = crate::config::Settings::load_sync().ok()?;
    let active = settings.provider.clone()?;
    crate::AuthStore::load()
        .gitlab_tokens(&active)
        .map(|_| active)
}

/// The active account's GitLab tokens, falling back to the only stored account.
pub fn get_gitlab_tokens() -> Option<GitlabTokens> {
    let store = crate::AuthStore::load();
    if let Some(active) = active_gitlab_account() {
        return store.gitlab_tokens(&active).cloned();
    }
    let accounts = store.accounts_for_protocol(crate::provider_id::ProviderId::GITLAB_DUO);
    match accounts.as_slice() {
        [only] => store.gitlab_tokens(only).cloned(),
        _ => None,
    }
}

/// Save to the active account, registering a new one when the active account
/// is not a GitLab Duo account.
pub fn save_gitlab_tokens(tokens: &GitlabTokens) -> anyhow::Result<()> {
    match active_gitlab_account() {
        Some(active) => save_gitlab_tokens_for_account(tokens, &active),
        None => save_gitlab_tokens_and_register(tokens).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn authorize_url_carries_pkce_and_the_api_scope() {
        let url = build_authorize_url("challenge-xyz", "state-abc");
        assert!(url.contains("/oauth/authorize"), "{url}");
        assert!(url.contains("code_challenge=challenge-xyz"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("state=state-abc"), "{url}");
        assert!(url.contains("scope=api"), "{url}");
        assert!(url.contains("response_type=code"), "{url}");
    }

    #[test]
    fn token_response_derives_expiry_from_created_at_plus_lifetime() {
        let tokens = parse_token_response(
            &json!({
                "access_token": "gl-1",
                "refresh_token": "gl-r",
                "created_at": 1_000,
                "expires_in": 7200
            }),
            None,
        )
        .expect("valid token response");
        assert_eq!(tokens.access_token, "gl-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("gl-r"));
        assert_eq!(tokens.expires_at, Some(1_000 + 7200));

        assert!(parse_token_response(&json!({ "expires_in": 1 }), None).is_err());
    }

    #[test]
    fn direct_access_parses_the_token_and_gateway_headers() {
        let grant = parse_direct_access(&json!({
            "token": "da-token",
            "headers": { "X-Gitlab-Unit": "duo", "X-Extra": "1" }
        }))
        .expect("valid direct access");
        assert_eq!(grant.token, "da-token");
        assert_eq!(
            grant.headers.get("X-Gitlab-Unit").map(String::as_str),
            Some("duo")
        );

        // Missing headers or token is an error, not a silent empty grant.
        assert!(parse_direct_access(&json!({ "token": "x" })).is_err());
        assert!(parse_direct_access(&json!({ "headers": {} })).is_err());
    }

    #[test]
    fn an_expiry_in_the_past_reads_as_expired() {
        let expired = GitlabTokens {
            access_token: "a".into(),
            expires_at: Some(now_secs().saturating_sub(1)),
            ..Default::default()
        };
        assert!(is_expired(&expired));
        assert!(!is_expired(&GitlabTokens {
            access_token: "a".into(),
            expires_at: Some(now_secs() + 3600),
            ..Default::default()
        }));
        // A PAT has no expiry and must not be treated as expired.
        assert!(!is_expired(&GitlabTokens {
            access_token: "pat".into(),
            ..Default::default()
        }));
    }
}
