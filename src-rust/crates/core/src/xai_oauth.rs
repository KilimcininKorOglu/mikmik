//! xai_oauth.rs — xAI Grok OAuth (device authorization grant).
//!
//! SuperGrok / X Premium+ signs in with the RFC 8628 device authorization
//! grant against `auth.x.ai`. Unlike Kimi, the token endpoint is not fixed: it
//! is read from the issuer's OIDC discovery document, so the flow and the
//! refresh both discover it first. The authed inference endpoint is the same
//! OpenAI-compatible `api.x.ai/v1` that the API-key `xai` provider uses; the
//! only difference is a Bearer OAuth token in place of an API key.
//!
//! Network I/O lives here; the JWT subject extractor, expiry check and
//! token-response parser are pure so they can be tested without a live endpoint.

use serde::{Deserialize, Serialize};

/// The xAI OIDC issuer.
pub const OAUTH_ISSUER: &str = "https://auth.x.ai";

/// The device-code endpoint (fixed).
pub const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";

/// The OIDC discovery document, which carries the token endpoint.
pub const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";

/// The public device-flow client id.
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

/// The scopes SuperGrok's CLI requests.
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";

/// Default authed API base, overridable by `XAI_OAUTH_BASE_URL`.
pub const DEFAULT_API_BASE: &str = "https://api.x.ai/v1";

/// Treat a token as expired this many seconds early.
const EXPIRY_SKEW_SECS: u64 = 300;

/// Default device-flow lifetime when the server omits `expires_in`.
const DEFAULT_DEVICE_TTL_SECS: u64 = 15 * 60;

/// Default poll interval when the server omits `interval`.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// The authed API base in effect, honouring `XAI_OAUTH_BASE_URL`.
pub fn api_base() -> String {
    std::env::var("XAI_OAUTH_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// xAI OAuth tokens, persisted in the auth store under an account.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct XaiTokens {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix timestamp in seconds when the access token expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Whether `tokens` should be refreshed before use.
pub fn is_expired(tokens: &XaiTokens) -> bool {
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

/// The account id inside an xAI access token, from the JWT `sub`.
pub fn jwt_subject(access_token: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = access_token.split('.').nth(1)?;
    let mut padded = payload_b64.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("sub")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The device-authorization response, normalised into seconds.
#[derive(Debug, Clone)]
pub struct XaiDeviceAuth {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Read the token endpoint from the OIDC discovery document.
///
/// Discovered rather than hardcoded, matching the reference client, so a change
/// on xAI's side is picked up without a code change.
pub async fn discover_token_endpoint() -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(DISCOVERY_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("xAI OIDC discovery failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "xAI OIDC discovery returned {}",
            resp.status().as_u16()
        ));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("xAI OIDC discovery response was not JSON: {e}"))?;
    let endpoint = json
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| s.starts_with("https://"))
        .ok_or_else(|| "xAI OIDC discovery response missing token_endpoint".to_string())?;
    Ok(endpoint.to_string())
}

/// Begin the device authorization grant.
pub async fn request_device_authorization() -> Result<XaiDeviceAuth, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await
        .map_err(|e| format!("xAI device-code request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "xAI device-code request failed: {} {text}",
            status.as_u16()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("xAI device-code response was not JSON: {e}"))?;
    parse_device_authorization(&json)
}

/// Parse a device-authorization payload (pure, for tests and the live call).
pub fn parse_device_authorization(json: &serde_json::Value) -> Result<XaiDeviceAuth, String> {
    let str_field = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let user_code = str_field("user_code")
        .ok_or_else(|| "xAI device-code response missing user_code".to_string())?;
    let device_code = str_field("device_code")
        .ok_or_else(|| "xAI device-code response missing device_code".to_string())?;
    let verification_uri =
        str_field("verification_uri").or_else(|| str_field("verification_uri_complete"));
    let verification_uri_complete = str_field("verification_uri_complete")
        .or_else(|| verification_uri.clone())
        .ok_or_else(|| "xAI device-code response missing verification_uri".to_string())?;
    let verification_uri = verification_uri.unwrap_or_else(|| verification_uri_complete.clone());
    let interval = json
        .get("interval")
        .and_then(|v| v.as_u64())
        .filter(|i| *i > 0)
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .filter(|e| *e > 0)
        .unwrap_or(DEFAULT_DEVICE_TTL_SECS);

    Ok(XaiDeviceAuth {
        user_code,
        device_code,
        verification_uri,
        verification_uri_complete,
        interval,
        expires_in,
    })
}

/// Turn a token-endpoint payload into [`XaiTokens`].
pub fn parse_token_response(
    json: &serde_json::Value,
    refresh_fallback: Option<&str>,
) -> Result<XaiTokens, String> {
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "xAI token response missing access_token".to_string())?
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

    let account_id = jwt_subject(&access_token);

    Ok(XaiTokens {
        access_token,
        refresh_token,
        expires_at,
        account_id,
    })
}

/// Poll the token endpoint until the user authorizes, the flow expires, or
/// `timeout_secs` elapses.
pub async fn poll_for_token(
    token_endpoint: &str,
    device_code: &str,
    interval: u64,
    timeout_secs: u64,
) -> Result<XaiTokens, String> {
    let client = reqwest::Client::new();
    let start = std::time::Instant::now();
    let mut wait = interval.max(1);

    loop {
        if start.elapsed().as_secs() > timeout_secs {
            return Err("xAI device flow timed out".to_string());
        }
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;

        let resp = client
            .post(token_endpoint)
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
            ])
            .send()
            .await
            .map_err(|e| format!("xAI token poll failed: {e}"))?;

        let ok = resp.status().is_success();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("xAI token response was not JSON: {e}"))?;

        if ok && json.get("access_token").is_some() {
            return parse_token_response(&json, None);
        }

        match json.get("error").and_then(|v| v.as_str()) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                wait += 5;
                continue;
            }
            Some("expired_token") => return Err("xAI device authorization expired".to_string()),
            Some("access_denied") => return Err("xAI device authorization denied".to_string()),
            Some(other) => return Err(format!("xAI device flow failed: {other}")),
            None => return Err("xAI token response carried neither a token nor an error".into()),
        }
    }
}

/// Exchange a refresh token for a fresh access token, discovering the endpoint.
pub async fn refresh(refresh_token: &str) -> Result<XaiTokens, String> {
    let token_endpoint = discover_token_endpoint().await?;
    let client = reqwest::Client::new();
    let resp = client
        .post(&token_endpoint)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|e| format!("xAI token refresh request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "xAI token refresh failed: {} {text}",
            status.as_u16()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("xAI token refresh response was not JSON: {e}"))?;
    parse_token_response(&json, Some(refresh_token))
}

// ---------------------------------------------------------------------------
// Account storage
// ---------------------------------------------------------------------------

/// Save xAI tokens under `account_id` (persists immediately).
pub fn save_xai_tokens_for_account(tokens: &XaiTokens, account_id: &str) -> anyhow::Result<()> {
    let mut store = crate::AuthStore::load();
    store.set_xai_tokens(account_id, tokens.clone());
    Ok(())
}

/// Load the xAI tokens stored for `account_id`.
pub fn load_xai_tokens_for_account(account_id: &str) -> Option<XaiTokens> {
    crate::AuthStore::load().xai_tokens(account_id).cloned()
}

/// Save xAI tokens, open the account's `providers` entry, and make it active.
/// Returns the account id used.
pub fn save_xai_tokens_and_register(tokens: &XaiTokens) -> anyhow::Result<String> {
    use crate::provider_id::ProviderId;

    let settings = crate::config::Settings::load_sync().unwrap_or_default();
    let config = settings.effective_config();
    let store = crate::AuthStore::load();

    let existing_id = tokens.account_id.as_ref().and_then(|identity| {
        store
            .accounts_for_protocol(ProviderId::XAI_OAUTH)
            .into_iter()
            .find(|id| {
                store
                    .xai_tokens(id)
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
            config.account_name_for_login(&base, ProviderId::XAI_OAUTH)
        }
    };

    save_xai_tokens_for_account(tokens, &id)?;
    crate::config::register_account(&id, ProviderId::XAI_OAUTH, true)?;
    Ok(id)
}

/// The active account, when it is an xAI OAuth account.
pub fn active_xai_account() -> Option<String> {
    let settings = crate::config::Settings::load_sync().ok()?;
    let active = settings.provider.clone()?;
    crate::AuthStore::load().xai_tokens(&active).map(|_| active)
}

/// The active account's xAI tokens, falling back to the only stored account.
pub fn get_xai_tokens() -> Option<XaiTokens> {
    let store = crate::AuthStore::load();
    if let Some(active) = active_xai_account() {
        return store.xai_tokens(&active).cloned();
    }
    let accounts = store.accounts_for_protocol(crate::provider_id::ProviderId::XAI_OAUTH);
    match accounts.as_slice() {
        [only] => store.xai_tokens(only).cloned(),
        _ => None,
    }
}

/// Save to the active account, registering a new one when the active account
/// is not an xAI OAuth account.
pub fn save_xai_tokens(tokens: &XaiTokens) -> anyhow::Result<()> {
    match active_xai_account() {
        Some(active) => save_xai_tokens_for_account(tokens, &active),
        None => save_xai_tokens_and_register(tokens).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jwt_subject_reads_the_sub_claim() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({ "sub": "user-42" })).unwrap());
        let token = format!("header.{payload}.sig");
        assert_eq!(jwt_subject(&token), Some("user-42".to_string()));
        assert_eq!(jwt_subject("opaque"), None);
    }

    #[test]
    fn parse_token_response_requires_an_access_token_and_keeps_the_refresh_fallback() {
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
    fn device_authorization_falls_back_to_the_complete_uri() {
        let auth = parse_device_authorization(&json!({
            "user_code": "WXYZ",
            "device_code": "dev-9",
            "verification_uri_complete": "https://x.ai/device?code=WXYZ"
        }))
        .expect("valid device auth");
        assert_eq!(auth.verification_uri, "https://x.ai/device?code=WXYZ");
        assert_eq!(auth.interval, DEFAULT_POLL_INTERVAL_SECS);

        assert!(parse_device_authorization(&json!({ "user_code": "x" })).is_err());
    }

    #[test]
    fn an_expiry_in_the_past_reads_as_expired() {
        let expired = XaiTokens {
            access_token: "a".into(),
            expires_at: Some(now_secs().saturating_sub(1)),
            ..Default::default()
        };
        assert!(is_expired(&expired));
        assert!(!is_expired(&XaiTokens {
            access_token: "a".into(),
            expires_at: Some(now_secs() + 3600),
            ..Default::default()
        }));
    }
}
