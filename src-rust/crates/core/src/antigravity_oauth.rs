//! antigravity_oauth.rs — Google Antigravity OAuth (Google login + Cloud Code
//! Assist provisioning).
//!
//! Antigravity signs in with a standard Google OAuth 2.0 authorization-code
//! flow against a loopback redirect (`http://localhost:51121/oauth-callback`),
//! using the desktop client's embedded credentials. The access token alone is
//! not enough to infer: the internal Cloud Code Assist control plane
//! (`daily-cloudcode-pa.googleapis.com`) must first resolve a
//! `cloudaicompanionProject` for the account, provisioning the free tier when
//! the account has not been onboarded. That project id is then carried on every
//! inference request.
//!
//! The authorize-URL builder, the token-response parser, the id-token email
//! extractor and the expiry check are pure so they can be tested without a live
//! endpoint. Network I/O (code exchange, refresh, project discovery) is async.

use serde::{Deserialize, Serialize};

/// The Google OAuth authorization endpoint.
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// The Google OAuth token endpoint (code exchange and refresh).
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The internal Cloud Code Assist control-plane host used by Antigravity.
pub const CLOUD_CODE_ASSIST_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";

/// The fixed loopback port the desktop client's OAuth credentials are
/// registered with. The redirect URI must match exactly.
pub const CALLBACK_PORT: u16 = 51121;

/// The loopback callback path the redirect URI carries.
pub const CALLBACK_PATH: &str = "/oauth-callback";

/// The free tier id the control plane onboards accounts into.
pub const FREE_TIER_ID: &str = "free-tier";

/// The client version the Antigravity user-agent advertises. The backend gates
/// model access on this value.
pub const ANTIGRAVITY_VERSION: &str = "2.8.0";

/// The OAuth scopes the desktop client requests.
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Treat a token as expired this many seconds early.
const EXPIRY_SKEW_SECS: u64 = 300;

/// Overall budget for the onboarding handshake.
const ONBOARD_TIMEOUT_SECS: u64 = 30;

/// Poll interval while waiting for the onboard operation to finish.
const ONBOARD_POLL_INTERVAL_SECS: u64 = 1;

// The desktop client's embedded OAuth credentials, base64-encoded exactly as
// the reference client stores them. These identify the Antigravity desktop
// application; they are a public client identifier, not an account secret.
const CLIENT_ID_B64: &str =
    "MTA3MTAwNjA2MDU5MS10bWhzc2luMmgyMWxjcmUyMzV2dG9sb2poNGc0MDNlcC5hcHBzLmdvb2dsZXVzZXJjb250ZW50LmNvbQ==";
const CLIENT_SECRET_B64: &str = "R09DU1BYLUs1OEZXUjQ4NkxkTEoxbUxCOHNYQzR6NnFEQWY=";

/// The redirect URI the loopback flow listens on.
pub fn redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

/// The OAuth client id (decoded on demand).
pub fn client_id() -> String {
    decode_b64(CLIENT_ID_B64)
}

/// The OAuth client secret (decoded on demand).
pub fn client_secret() -> String {
    decode_b64(CLIENT_SECRET_B64)
}

fn decode_b64(input: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.as_bytes())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

/// The user-agent native Antigravity requests carry. `os`/`arch`/`cl` are not
/// validated by the backend (only the version gates), but are overridable for
/// parity with a captured client.
pub fn user_agent() -> String {
    let cl = env_or("PI_AI_ANTIGRAVITY_CL", "963137146");
    let os = env_or("PI_AI_ANTIGRAVITY_OS", "darwin");
    let arch = env_or("PI_AI_ANTIGRAVITY_ARCH", "arm64");
    format!(
        "antigravity/hub/{ANTIGRAVITY_VERSION} (aidev_client; os_type={os}; arch={arch}; cl={cl})"
    )
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The `Client-Metadata` value native Antigravity requests carry.
pub const CLIENT_METADATA: &str =
    "ideType=ANTIGRAVITY,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";

/// Antigravity OAuth tokens plus the resolved Cloud Code Assist project,
/// persisted in the auth store under an account.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AntigravityTokens {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix timestamp in seconds when the access token expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// The account identity (email), from the id-token, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// The resolved `cloudaicompanionProject` used on every inference request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// Whether `tokens` should be refreshed before use.
pub fn is_expired(tokens: &AntigravityTokens) -> bool {
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

/// The account email inside a Google id-token, from the JWT `email` claim.
pub fn id_token_email(id_token: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = id_token.split('.').nth(1)?;
    let mut padded = payload_b64.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("email")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build the authorization URL the user opens in their browser.
pub fn authorize_url(state: &str) -> String {
    let scope = SCOPES.join(" ");
    let params = [
        ("client_id", client_id()),
        ("redirect_uri", redirect_uri()),
        ("response_type", "code".to_string()),
        ("scope", scope),
        ("access_type", "offline".to_string()),
        ("prompt", "consent".to_string()),
        ("state", state.to_string()),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTH_URL}?{query}")
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Parse a Google token-endpoint payload into [`AntigravityTokens`].
///
/// `refresh_fallback` keeps the prior refresh token when the response omits one
/// (Google returns a refresh token only on the first consent).
pub fn parse_token_response(
    json: &serde_json::Value,
    refresh_fallback: Option<&str>,
) -> Result<AntigravityTokens, String> {
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Antigravity token response missing access_token".to_string())?
        .to_string();

    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| refresh_fallback.map(str::to_string));

    let expires_at = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .map(|secs| now_secs() + secs);

    let account_id = json
        .get("id_token")
        .and_then(|v| v.as_str())
        .and_then(id_token_email);

    Ok(AntigravityTokens {
        access_token,
        refresh_token,
        expires_at,
        account_id,
        project_id: None,
    })
}

/// Exchange an authorization code for tokens.
pub async fn exchange_code(code: &str) -> Result<AntigravityTokens, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id()),
            ("client_secret", client_secret()),
            ("code", code.to_string()),
            ("grant_type", "authorization_code".to_string()),
            ("redirect_uri", redirect_uri()),
        ])
        .send()
        .await
        .map_err(|e| format!("Antigravity code exchange failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Antigravity code exchange failed: {} {text}",
            status.as_u16()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Antigravity token response was not JSON: {e}"))?;
    parse_token_response(&json, None)
}

/// Exchange a refresh token for a fresh access token, preserving `project_id`.
pub async fn refresh(
    refresh_token: &str,
    project_id: Option<&str>,
) -> Result<AntigravityTokens, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id()),
            ("client_secret", client_secret()),
            ("refresh_token", refresh_token.to_string()),
            ("grant_type", "refresh_token".to_string()),
        ])
        .send()
        .await
        .map_err(|e| format!("Antigravity token refresh request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Antigravity token refresh failed: {} {text}",
            status.as_u16()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Antigravity token refresh response was not JSON: {e}"))?;
    let mut tokens = parse_token_response(&json, Some(refresh_token))?;
    tokens.project_id = project_id.map(str::to_string);
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Cloud Code Assist project discovery
// ---------------------------------------------------------------------------

/// Resolve the `cloudaicompanionProject` for `access_token`, provisioning the
/// free tier when the account has not yet been onboarded.
pub async fn discover_project(access_token: &str) -> Result<String, String> {
    let client = reqwest::Client::new();

    let initial = load_code_assist(&client, access_token, None).await?;
    let project = extract_project_id(&initial);

    if !has_field(&initial, "currentTier") {
        onboard_user(&client, access_token).await?;
    }

    let refreshed = load_code_assist(&client, access_token, project.as_deref()).await?;
    extract_project_id(&refreshed)
        .ok_or_else(|| "loadCodeAssist did not return a cloudaicompanionProject".to_string())
}

fn load_code_assist_body(project: Option<&str>) -> serde_json::Value {
    let metadata = serde_json::json!({ "ideType": "ANTIGRAVITY" });
    match project {
        Some(project) => {
            serde_json::json!({ "cloudaicompanionProject": project, "metadata": metadata })
        }
        None => serde_json::json!({ "metadata": metadata }),
    }
}

async fn load_code_assist(
    client: &reqwest::Client,
    access_token: &str,
    project: Option<&str>,
) -> Result<serde_json::Value, String> {
    let url = format!("{CLOUD_CODE_ASSIST_ENDPOINT}/v1internal:loadCodeAssist");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent())
        .json(&load_code_assist_body(project))
        .send()
        .await
        .map_err(|e| format!("loadCodeAssist request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("loadCodeAssist failed: {status} {text}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("loadCodeAssist response was not JSON: {e}"))
}

async fn onboard_user(client: &reqwest::Client, access_token: &str) -> Result<(), String> {
    let onboard_url = format!("{CLOUD_CODE_ASSIST_ENDPOINT}/v1internal:onboardUser");
    let body = serde_json::json!({
        "tierId": FREE_TIER_ID,
        "metadata": { "ideType": "ANTIGRAVITY" },
    });
    let mut operation = post_json(client, access_token, &onboard_url, &body).await?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(ONBOARD_TIMEOUT_SECS);
    loop {
        if operation.get("done").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(error) = operation.get("error").filter(|v| !v.is_null()) {
                return Err(format!("onboardUser failed: {error}"));
            }
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "onboardUser timed out after {ONBOARD_TIMEOUT_SECS}s"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_secs(ONBOARD_POLL_INTERVAL_SECS)).await;

        let name = operation
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "onboardUser returned an operation without a name".to_string())?;
        let poll_url = format!("{CLOUD_CODE_ASSIST_ENDPOINT}/v1internal/{name}");
        operation = get_json(client, access_token, &poll_url).await?;
    }
}

async fn post_json(
    client: &reqwest::Client,
    access_token: &str,
    url: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent())
        .json(body)
        .send()
        .await
        .map_err(|e| format!("onboardUser request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("onboardUser failed: {status} {text}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("onboardUser response was not JSON: {e}"))
}

async fn get_json(
    client: &reqwest::Client,
    access_token: &str,
    url: &str,
) -> Result<serde_json::Value, String> {
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", user_agent())
        .send()
        .await
        .map_err(|e| format!("onboardUser poll failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("onboardUser poll failed: {status} {text}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("onboardUser poll response was not JSON: {e}"))
}

fn extract_project_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("cloudaicompanionProject")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn has_field(payload: &serde_json::Value, field: &str) -> bool {
    payload.get(field).is_some_and(|v| !v.is_null())
}

// ---------------------------------------------------------------------------
// Account storage
// ---------------------------------------------------------------------------

/// Save Antigravity tokens under `account_id` (persists immediately).
pub fn save_antigravity_tokens_for_account(
    tokens: &AntigravityTokens,
    account_id: &str,
) -> anyhow::Result<()> {
    let mut store = crate::AuthStore::load();
    store.set_antigravity_tokens(account_id, tokens.clone());
    Ok(())
}

/// Load the Antigravity tokens stored for `account_id`.
pub fn load_antigravity_tokens_for_account(account_id: &str) -> Option<AntigravityTokens> {
    crate::AuthStore::load()
        .antigravity_tokens(account_id)
        .cloned()
}

/// Save Antigravity tokens, open the account's `providers` entry, and make it
/// active. Returns the account id used.
pub fn save_antigravity_tokens_and_register(tokens: &AntigravityTokens) -> anyhow::Result<String> {
    use crate::provider_id::ProviderId;

    let settings = crate::config::Settings::load_sync().unwrap_or_default();
    let config = settings.effective_config();
    let store = crate::AuthStore::load();

    let existing_id = tokens.account_id.as_ref().and_then(|identity| {
        store
            .accounts_for_protocol(ProviderId::GOOGLE_ANTIGRAVITY)
            .into_iter()
            .find(|id| {
                store
                    .antigravity_tokens(id)
                    .and_then(|stored| stored.account_id.clone())
                    .as_ref()
                    == Some(identity)
            })
    });

    let id = match existing_id {
        Some(id) => id,
        None => {
            let base = tokens
                .account_id
                .clone()
                .unwrap_or_else(|| "account".to_string());
            config.account_name_for_login(&base, ProviderId::GOOGLE_ANTIGRAVITY)
        }
    };

    save_antigravity_tokens_for_account(tokens, &id)?;
    crate::config::register_account(&id, ProviderId::GOOGLE_ANTIGRAVITY, true)?;
    Ok(id)
}

/// The active account, when it is an Antigravity account.
pub fn active_antigravity_account() -> Option<String> {
    let settings = crate::config::Settings::load_sync().ok()?;
    let active = settings.provider.clone()?;
    crate::AuthStore::load()
        .antigravity_tokens(&active)
        .map(|_| active)
}

/// The active account's Antigravity tokens, falling back to the only stored one.
pub fn get_antigravity_tokens() -> Option<AntigravityTokens> {
    let store = crate::AuthStore::load();
    if let Some(active) = active_antigravity_account() {
        return store.antigravity_tokens(&active).cloned();
    }
    let accounts = store.accounts_for_protocol(crate::provider_id::ProviderId::GOOGLE_ANTIGRAVITY);
    match accounts.as_slice() {
        [only] => store.antigravity_tokens(only).cloned(),
        _ => None,
    }
}

/// Save to the active account, registering a new one when the active account is
/// not an Antigravity account.
pub fn save_antigravity_tokens(tokens: &AntigravityTokens) -> anyhow::Result<()> {
    match active_antigravity_account() {
        Some(active) => save_antigravity_tokens_for_account(tokens, &active),
        None => save_antigravity_tokens_and_register(tokens).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_credentials_decode_from_base64() {
        assert!(client_id().ends_with(".apps.googleusercontent.com"));
        assert!(client_secret().starts_with("GOCSPX-"));
    }

    #[test]
    fn id_token_email_reads_the_email_claim() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({ "email": "dev@example.com" })).unwrap());
        let token = format!("header.{payload}.sig");
        assert_eq!(id_token_email(&token), Some("dev@example.com".to_string()));
        assert_eq!(id_token_email("opaque"), None);
    }

    #[test]
    fn parse_token_response_requires_access_token_and_keeps_refresh_fallback() {
        let refreshed = parse_token_response(
            &json!({ "access_token": "a1", "expires_in": 3600 }),
            Some("r-fallback"),
        )
        .expect("valid token response");
        assert_eq!(refreshed.access_token, "a1");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("r-fallback"));
        assert!(refreshed.expires_at.is_some());

        assert!(parse_token_response(&json!({ "expires_in": 1 }), None).is_err());
    }

    #[test]
    fn authorize_url_carries_the_loopback_redirect_and_scopes() {
        let url = authorize_url("state-123");
        assert!(url.starts_with(AUTH_URL));
        assert!(url.contains("state=state-123"));
        assert!(url.contains("localhost%3A51121%2Foauth-callback"));
        assert!(url.contains("cloud-platform"));
    }

    #[test]
    fn an_expiry_in_the_past_reads_as_expired() {
        let expired = AntigravityTokens {
            access_token: "a".into(),
            expires_at: Some(now_secs().saturating_sub(1)),
            ..Default::default()
        };
        assert!(is_expired(&expired));
        assert!(!is_expired(&AntigravityTokens {
            access_token: "a".into(),
            expires_at: Some(now_secs() + 3600),
            ..Default::default()
        }));
    }

    #[test]
    fn extract_project_id_reads_the_companion_project() {
        assert_eq!(
            extract_project_id(&json!({ "cloudaicompanionProject": "proj-9" })),
            Some("proj-9".to_string())
        );
        assert_eq!(extract_project_id(&json!({})), None);
    }
}
