//! kimi_oauth.rs — Kimi Code OAuth (device authorization grant).
//!
//! Kimi Code signs in with the OAuth 2.0 device authorization grant against
//! `auth.kimi.com`, then serves an OpenAI-compatible API at
//! `api.kimi.com/coding/v1`. The wire differs from the generic
//! [`crate::device_code`] helper in two ways, so this module carries its own
//! request functions:
//!
//! - the device-authorization and token requests are `application/x-www-form-
//!   urlencoded`, not JSON;
//! - every request advertises a fixed set of `X-Msh-*` device headers and a
//!   persisted device id, which the server expects from its own CLI.
//!
//! All network I/O lives here; the header builder, JWT account-id extraction,
//! expiry check and token-response parser are pure so they can be tested
//! without a live endpoint.

use serde::{Deserialize, Serialize};

/// The Kimi CLI OAuth client id (public, from the device flow).
pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

/// Default OAuth host, overridable by `KIMI_CODE_OAUTH_HOST` / `KIMI_OAUTH_HOST`.
pub const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";

/// Default authed API base, overridable by `KIMI_CODE_BASE_URL`.
pub const DEFAULT_API_BASE: &str = "https://api.kimi.com/coding/v1";

/// File under the config dir that pins this install's device id.
const DEVICE_ID_FILENAME: &str = "kimi-device-id";

/// Treat a token as expired this many seconds early, to avoid a mid-request
/// expiry racing the refresh.
const EXPIRY_SKEW_SECS: u64 = 300;

/// Default device-flow lifetime when the server omits `expires_in`.
const DEFAULT_DEVICE_TTL_SECS: u64 = 15 * 60;

/// Default poll interval when the server omits `interval`.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// The OAuth host in effect, honouring the two env overrides.
pub fn oauth_host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("KIMI_OAUTH_HOST")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_string())
}

/// The authed API base in effect, honouring `KIMI_CODE_BASE_URL`.
pub fn api_base() -> String {
    std::env::var("KIMI_CODE_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// Kimi OAuth tokens, persisted in the auth store under an account.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KimiTokens {
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
pub fn is_expired(tokens: &KimiTokens) -> bool {
    let Some(expires_at) = tokens.expires_at else {
        return false; // No expiry info — assume still valid.
    };
    now_secs() + EXPIRY_SKEW_SECS >= expires_at
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Keep only printable ASCII, so a device name never breaks header encoding.
fn sanitize_header_value(value: &str, fallback: &str) -> String {
    let sanitized: String = value
        .chars()
        .filter(|c| ('\x20'..='\x7E').contains(c))
        .collect();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The stable device id for this install.
///
/// Persistence is best-effort: an unreadable or unwritable config dir falls
/// back to a fresh id so header construction, and with it every request, never
/// fails on a filesystem error.
pub fn device_id() -> String {
    let path = crate::config::Settings::config_dir().join(DEVICE_ID_FILENAME);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{id}\n"));
    id
}

/// Best-effort host name for the device headers.
fn host_name() -> String {
    hostname::get()
        .ok()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// A human label for this OS, matching the shape Kimi's own CLI sends.
fn device_model() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    format!("{os} {}", std::env::consts::ARCH)
}

/// The `X-Msh-*` device headers Kimi expects on every request, plus the
/// `User-Agent`. Pure so a test can assert the set without a network call.
pub fn common_headers() -> Vec<(String, String)> {
    let version = env!("CARGO_PKG_VERSION");
    vec![
        ("User-Agent".to_string(), format!("KimiCLI/{version}")),
        ("X-Msh-Platform".to_string(), "kimi_cli".to_string()),
        ("X-Msh-Version".to_string(), version.to_string()),
        (
            "X-Msh-Device-Name".to_string(),
            sanitize_header_value(&host_name(), "unknown"),
        ),
        (
            "X-Msh-Device-Model".to_string(),
            sanitize_header_value(&device_model(), "unknown"),
        ),
        (
            "X-Msh-Os-Version".to_string(),
            sanitize_header_value(std::env::consts::OS, "unknown"),
        ),
        (
            "X-Msh-Device-Id".to_string(),
            sanitize_header_value(&device_id(), "unknown"),
        ),
    ]
}

/// The account id inside a Kimi access token, from the JWT `user_id` or `sub`.
///
/// Returns `None` for an opaque (non-JWT) token, which is still a valid
/// credential — it just carries no identity to name the account by.
pub fn jwt_account_id(access_token: &str) -> Option<String> {
    use base64::Engine;

    let payload_b64 = access_token.splitn(3, '.').nth(1)?;
    let mut padded = payload_b64.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let pick = |key: &str| {
        json.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    pick("user_id").or_else(|| pick("sub"))
}

/// The device-authorization response, normalised into seconds.
#[derive(Debug, Clone)]
pub struct KimiDeviceAuth {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub interval: u64,
    pub expires_in: u64,
}

fn apply_headers(mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    for (name, value) in common_headers() {
        builder = builder.header(name, value);
    }
    builder
}

/// Begin the device authorization grant.
pub async fn request_device_authorization() -> Result<KimiDeviceAuth, String> {
    let url = format!("{}/api/oauth/device_authorization", oauth_host());
    let client = reqwest::Client::new();
    let resp = apply_headers(client.post(&url))
        .form(&[("client_id", CLIENT_ID)])
        .send()
        .await
        .map_err(|e| format!("Kimi device authorization request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Kimi device authorization failed: {} {text}",
            status.as_u16()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Kimi device authorization response was not JSON: {e}"))?;

    parse_device_authorization(&json)
}

/// Parse a device-authorization payload (pure, for tests and the live call).
pub fn parse_device_authorization(json: &serde_json::Value) -> Result<KimiDeviceAuth, String> {
    let str_field = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let user_code = str_field("user_code")
        .ok_or_else(|| "Kimi device authorization response missing user_code".to_string())?;
    let device_code = str_field("device_code")
        .ok_or_else(|| "Kimi device authorization response missing device_code".to_string())?;
    let verification_uri = str_field("verification_uri")
        .ok_or_else(|| "Kimi device authorization response missing verification_uri".to_string())?;
    let verification_uri_complete =
        str_field("verification_uri_complete").unwrap_or_else(|| verification_uri.clone());
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

    Ok(KimiDeviceAuth {
        user_code,
        device_code,
        verification_uri,
        verification_uri_complete,
        interval,
        expires_in,
    })
}

/// Turn a token-endpoint payload into [`KimiTokens`], carrying the JWT account
/// id and computing an absolute expiry. `refresh_fallback` supplies the refresh
/// token on a refresh response that omits a new one.
pub fn parse_token_response(
    json: &serde_json::Value,
    refresh_fallback: Option<&str>,
) -> Result<KimiTokens, String> {
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Kimi token response missing access_token".to_string())?
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
        .map(|secs| now_secs() + secs.saturating_sub(EXPIRY_SKEW_SECS / 60));

    let account_id = jwt_account_id(&access_token);

    Ok(KimiTokens {
        access_token,
        refresh_token,
        expires_at,
        account_id,
    })
}

/// Poll the token endpoint until the user authorizes, the flow expires, or
/// `timeout_secs` elapses.
pub async fn poll_for_token(
    device_code: &str,
    interval: u64,
    timeout_secs: u64,
) -> Result<KimiTokens, String> {
    let url = format!("{}/api/oauth/token", oauth_host());
    let client = reqwest::Client::new();
    let start = std::time::Instant::now();
    let mut wait = interval.max(1);

    loop {
        if start.elapsed().as_secs() > timeout_secs {
            return Err("Kimi device flow timed out".to_string());
        }
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;

        let resp = apply_headers(client.post(&url))
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|e| format!("Kimi token poll failed: {e}"))?;

        let ok = resp.status().is_success();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Kimi token response was not JSON: {e}"))?;

        if ok && json.get("access_token").is_some() {
            return parse_token_response(&json, None);
        }

        match json.get("error").and_then(|v| v.as_str()) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                wait += 5;
                continue;
            }
            Some("expired_token") => return Err("Kimi device authorization expired".to_string()),
            Some("access_denied") => return Err("Kimi device authorization denied".to_string()),
            Some(other) => return Err(format!("Kimi device flow failed: {other}")),
            None => return Err("Kimi token response carried neither a token nor an error".into()),
        }
    }
}

/// Exchange a refresh token for a fresh access token.
pub async fn refresh(refresh_token: &str) -> Result<KimiTokens, String> {
    let url = format!("{}/api/oauth/token", oauth_host());
    let client = reqwest::Client::new();
    let resp = apply_headers(client.post(&url))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|e| format!("Kimi token refresh request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Kimi token refresh failed: {} {text}",
            status.as_u16()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Kimi token refresh response was not JSON: {e}"))?;
    parse_token_response(&json, Some(refresh_token))
}

// ---------------------------------------------------------------------------
// Account storage
// ---------------------------------------------------------------------------

/// Save Kimi tokens under `account_id` (persists immediately).
pub fn save_kimi_tokens_for_account(tokens: &KimiTokens, account_id: &str) -> anyhow::Result<()> {
    let mut store = crate::AuthStore::load();
    store.set_kimi_tokens(account_id, tokens.clone());
    Ok(())
}

/// Load the Kimi tokens stored for `account_id`.
pub fn load_kimi_tokens_for_account(account_id: &str) -> Option<KimiTokens> {
    crate::AuthStore::load().kimi_tokens(account_id).cloned()
}

/// Save Kimi tokens, open the account's `providers` entry, and make it active.
///
/// The account is named after the JWT identity, so logging in again with the
/// same identity refreshes that account in place rather than duplicating it.
/// Returns the account id used.
pub fn save_kimi_tokens_and_register(tokens: &KimiTokens) -> anyhow::Result<String> {
    use crate::provider_id::ProviderId;

    let settings = crate::config::Settings::load_sync().unwrap_or_default();
    let config = settings.effective_config();
    let store = crate::AuthStore::load();

    let existing_id = tokens.account_id.as_ref().and_then(|identity| {
        store
            .accounts_for_protocol(ProviderId::KIMI_CODE)
            .into_iter()
            .find(|id| {
                store
                    .kimi_tokens(id)
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
            config.account_name_for_login(&base, ProviderId::KIMI_CODE)
        }
    };

    save_kimi_tokens_for_account(tokens, &id)?;
    crate::config::register_account(&id, ProviderId::KIMI_CODE, true)?;
    Ok(id)
}

/// The active account, when it is a Kimi account.
pub fn active_kimi_account() -> Option<String> {
    let settings = crate::config::Settings::load_sync().ok()?;
    let active = settings.provider.clone()?;
    crate::AuthStore::load()
        .kimi_tokens(&active)
        .map(|_| active)
}

/// The active account's Kimi tokens, falling back to the only stored Kimi
/// account when the session points elsewhere.
pub fn get_kimi_tokens() -> Option<KimiTokens> {
    let store = crate::AuthStore::load();
    if let Some(active) = active_kimi_account() {
        return store.kimi_tokens(&active).cloned();
    }
    let accounts = store.accounts_for_protocol(crate::provider_id::ProviderId::KIMI_CODE);
    match accounts.as_slice() {
        [only] => store.kimi_tokens(only).cloned(),
        _ => None,
    }
}

/// Save to the active account, registering a new one when the active account
/// is not a Kimi account.
pub fn save_kimi_tokens(tokens: &KimiTokens) -> anyhow::Result<()> {
    match active_kimi_account() {
        Some(active) => save_kimi_tokens_for_account(tokens, &active),
        None => save_kimi_tokens_and_register(tokens).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn common_headers_carry_the_kimi_platform_marker() {
        let headers = common_headers();
        let platform = headers
            .iter()
            .find(|(k, _)| k == "X-Msh-Platform")
            .map(|(_, v)| v.as_str());
        assert_eq!(platform, Some("kimi_cli"));
        // Every value is non-empty printable ASCII, or a device name with a
        // stray byte would corrupt the header block.
        for (name, value) in &headers {
            assert!(!value.is_empty(), "{name} must not be empty");
            assert!(
                value.chars().all(|c| ('\x20'..='\x7E').contains(&c)),
                "{name} must be printable ASCII, got {value:?}"
            );
        }
    }

    #[test]
    fn jwt_account_id_reads_user_id_then_sub() {
        use base64::Engine;
        let make = |claims: serde_json::Value| {
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&claims).unwrap());
            format!("header.{payload}.sig")
        };
        assert_eq!(
            jwt_account_id(&make(json!({ "user_id": "u-1", "sub": "s-1" }))),
            Some("u-1".to_string())
        );
        assert_eq!(
            jwt_account_id(&make(json!({ "sub": "s-1" }))),
            Some("s-1".to_string())
        );
        // An opaque token names no account but is still usable.
        assert_eq!(jwt_account_id("opaque-token"), None);
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
    fn device_authorization_falls_back_to_the_plain_verification_uri() {
        let auth = parse_device_authorization(&json!({
            "user_code": "ABCD-EFGH",
            "device_code": "dev-123",
            "verification_uri": "https://kimi.com/device"
        }))
        .expect("valid device auth");
        assert_eq!(auth.verification_uri_complete, "https://kimi.com/device");
        assert_eq!(auth.interval, DEFAULT_POLL_INTERVAL_SECS);

        assert!(parse_device_authorization(&json!({ "user_code": "x" })).is_err());
    }

    #[test]
    fn an_expiry_in_the_past_reads_as_expired() {
        let expired = KimiTokens {
            access_token: "a".into(),
            expires_at: Some(now_secs().saturating_sub(1)),
            ..Default::default()
        };
        assert!(is_expired(&expired));

        let fresh = KimiTokens {
            access_token: "a".into(),
            expires_at: Some(now_secs() + 3600),
            ..Default::default()
        };
        assert!(!is_expired(&fresh));

        // No expiry info means the token is assumed valid.
        assert!(!is_expired(&KimiTokens {
            access_token: "a".into(),
            ..Default::default()
        }));
    }
}
