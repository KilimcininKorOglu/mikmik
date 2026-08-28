//! cursor_oauth.rs — Cursor (Cursor Pro) OAuth.
//!
//! Cursor signs in with a PKCE poll flow, not a loopback redirect: the browser
//! opens `cursor.com/loginDeepControl?challenge&uuid`, and the CLI polls
//! `api2.cursor.sh/auth/poll?uuid&verifier` until the sign-in completes and
//! returns an access/refresh token pair. The access token is refreshed by
//! POSTing the refresh token to `api2.cursor.sh/auth/exchange_user_api_key`.
//!
//! The auth-URL builder, the JWT parsers and the expiry check are pure; the
//! poll and refresh are async.

use serde::{Deserialize, Serialize};

/// The browser sign-in page.
pub const LOGIN_URL: &str = "https://cursor.com/loginDeepControl";

/// The poll endpoint the CLI waits on.
pub const POLL_URL: &str = "https://api2.cursor.sh/auth/poll";

/// The refresh endpoint.
pub const REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";

const POLL_MAX_ATTEMPTS: u32 = 150;
const POLL_BASE_DELAY_MS: u64 = 1000;
const POLL_MAX_DELAY_MS: u64 = 10_000;

/// Treat a token as expired this many seconds early.
const EXPIRY_SKEW_SECS: u64 = 300;

/// Fallback lifetime when the access token carries no `exp`.
const FALLBACK_TTL_SECS: u64 = 3600;

/// Cursor OAuth tokens, persisted in the auth store under an account.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CursorTokens {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix timestamp in seconds when the access token expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// The PKCE parameters that drive one sign-in attempt.
pub struct CursorAuthParams {
    pub verifier: String,
    pub uuid: String,
    pub login_url: String,
}

/// Whether `tokens` should be refreshed before use.
pub fn is_expired(tokens: &CursorTokens) -> bool {
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

fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload_b64 = token.split('.').nth(1)?;
    let mut padded = payload_b64.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The access token's expiry as a Unix second timestamp, or a fallback.
pub fn token_expiry(token: &str) -> u64 {
    jwt_payload(token)
        .and_then(|p| p.get("exp").and_then(|v| v.as_u64()))
        .map(|exp| exp.saturating_sub(EXPIRY_SKEW_SECS))
        .unwrap_or_else(|| now_secs() + FALLBACK_TTL_SECS)
}

/// The user id from an access token's `sub` claim (`auth0|<id>` → `<id>`).
pub fn token_user_id(token: &str) -> Option<String> {
    let sub = jwt_payload(token)?
        .get("sub")
        .and_then(|v| v.as_str())
        .map(str::to_string)?;
    let id = sub.split('|').nth(1).unwrap_or(&sub).trim().to_string();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
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

/// Generate the PKCE parameters and the browser login URL for one attempt.
pub fn generate_auth_params() -> Result<CursorAuthParams, String> {
    let verifier = crate::oauth::generate_code_verifier().map_err(|e| e.to_string())?;
    let challenge = crate::oauth::generate_code_challenge(&verifier);
    let uuid = uuid::Uuid::new_v4().to_string();
    let login_url = format!(
        "{LOGIN_URL}?challenge={}&uuid={}&mode=login&redirectTarget=cli",
        urlencode(&challenge),
        urlencode(&uuid),
    );
    Ok(CursorAuthParams {
        verifier,
        uuid,
        login_url,
    })
}

fn tokens_from_pair(access: &str, refresh: &str) -> CursorTokens {
    CursorTokens {
        access_token: access.to_string(),
        refresh_token: (!refresh.is_empty()).then(|| refresh.to_string()),
        expires_at: Some(token_expiry(access)),
        account_id: token_user_id(access),
    }
}

/// Parse a poll / refresh token payload.
pub fn parse_token_response(json: &serde_json::Value) -> Option<CursorTokens> {
    let access = json.get("accessToken").and_then(|v| v.as_str())?;
    if access.is_empty() {
        return None;
    }
    let refresh = json
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Some(tokens_from_pair(access, refresh))
}

/// Poll until the browser sign-in completes, the flow times out, or too many
/// consecutive errors occur.
pub async fn poll_for_token(uuid: &str, verifier: &str) -> Result<CursorTokens, String> {
    let client = reqwest::Client::new();
    let mut delay = POLL_BASE_DELAY_MS;
    let mut consecutive_errors = 0;

    for _ in 0..POLL_MAX_ATTEMPTS {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;

        let url = format!("{POLL_URL}?uuid={uuid}&verifier={verifier}");
        match client.get(&url).send().await {
            Ok(resp) if resp.status().as_u16() == 404 => {
                consecutive_errors = 0;
                delay = (delay * 12 / 10).min(POLL_MAX_DELAY_MS);
            }
            Ok(resp) if resp.status().is_success() => {
                let json: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| format!("Cursor poll response was not JSON: {e}"))?;
                if let Some(tokens) = parse_token_response(&json) {
                    return Ok(tokens);
                }
                consecutive_errors += 1;
            }
            Ok(resp) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    return Err(format!("Cursor poll failed: {}", resp.status().as_u16()));
                }
            }
            Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    return Err(
                        "too many consecutive errors during Cursor auth polling".to_string()
                    );
                }
            }
        }
    }

    Err("Cursor authentication polling timed out".to_string())
}

/// Exchange a refresh token for a fresh access token.
pub async fn refresh(refresh_token: &str) -> Result<CursorTokens, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(REFRESH_URL)
        .header("Authorization", format!("Bearer {refresh_token}"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|e| format!("Cursor token refresh request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Cursor token refresh failed: {} {text}",
            status.as_u16()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Cursor token refresh response was not JSON: {e}"))?;
    let mut tokens = parse_token_response(&json)
        .ok_or_else(|| "Cursor token refresh returned no access token".to_string())?;
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_string());
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Account storage
// ---------------------------------------------------------------------------

/// Save Cursor tokens under `account_id` (persists immediately).
pub fn save_cursor_tokens_for_account(
    tokens: &CursorTokens,
    account_id: &str,
) -> anyhow::Result<()> {
    let mut store = crate::AuthStore::load();
    store.set_cursor_tokens(account_id, tokens.clone());
    Ok(())
}

/// Load the Cursor tokens stored for `account_id`.
pub fn load_cursor_tokens_for_account(account_id: &str) -> Option<CursorTokens> {
    crate::AuthStore::load().cursor_tokens(account_id).cloned()
}

/// Save Cursor tokens, open the account's `providers` entry, and make it active.
/// Returns the account id used.
pub fn save_cursor_tokens_and_register(tokens: &CursorTokens) -> anyhow::Result<String> {
    use crate::provider_id::ProviderId;

    let settings = crate::config::Settings::load_sync().unwrap_or_default();
    let config = settings.effective_config();
    let store = crate::AuthStore::load();

    let existing_id = tokens.account_id.as_ref().and_then(|identity| {
        store
            .accounts_for_protocol(ProviderId::CURSOR)
            .into_iter()
            .find(|id| {
                store
                    .cursor_tokens(id)
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
            config.account_name_for_login(&base, ProviderId::CURSOR)
        }
    };

    save_cursor_tokens_for_account(tokens, &id)?;
    crate::config::register_account(&id, ProviderId::CURSOR, true)?;
    Ok(id)
}

/// The active account, when it is a Cursor account.
pub fn active_cursor_account() -> Option<String> {
    let settings = crate::config::Settings::load_sync().ok()?;
    let active = settings.provider.clone()?;
    crate::AuthStore::load()
        .cursor_tokens(&active)
        .map(|_| active)
}

/// The active account's Cursor tokens, falling back to the only stored one.
pub fn get_cursor_tokens() -> Option<CursorTokens> {
    let store = crate::AuthStore::load();
    if let Some(active) = active_cursor_account() {
        return store.cursor_tokens(&active).cloned();
    }
    let accounts = store.accounts_for_protocol(crate::provider_id::ProviderId::CURSOR);
    match accounts.as_slice() {
        [only] => store.cursor_tokens(only).cloned(),
        _ => None,
    }
}

/// Save to the active account, registering a new one when the active account is
/// not a Cursor account.
pub fn save_cursor_tokens(tokens: &CursorTokens) -> anyhow::Result<()> {
    match active_cursor_account() {
        Some(active) => save_cursor_tokens_for_account(tokens, &active),
        None => save_cursor_tokens_and_register(tokens).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: serde_json::Value) -> String {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("header.{payload}.sig")
    }

    #[test]
    fn token_user_id_strips_the_auth0_prefix() {
        let token = jwt(json!({ "sub": "auth0|user-42" }));
        assert_eq!(token_user_id(&token), Some("user-42".to_string()));
        let bare = jwt(json!({ "sub": "user-9" }));
        assert_eq!(token_user_id(&bare), Some("user-9".to_string()));
    }

    #[test]
    fn token_expiry_reads_exp_or_falls_back() {
        let token = jwt(json!({ "exp": 2_000_000_000u64 }));
        assert_eq!(token_expiry(&token), 2_000_000_000 - EXPIRY_SKEW_SECS);
        assert!(token_expiry("opaque") > now_secs());
    }

    #[test]
    fn parse_token_response_requires_an_access_token() {
        let tokens = parse_token_response(&json!({
            "accessToken": jwt(json!({ "sub": "auth0|u1", "exp": 2_000_000_000u64 })),
            "refreshToken": "r1"
        }))
        .expect("valid pair");
        assert!(!tokens.access_token.is_empty());
        assert_eq!(tokens.refresh_token.as_deref(), Some("r1"));
        assert_eq!(tokens.account_id.as_deref(), Some("u1"));

        assert!(parse_token_response(&json!({ "refreshToken": "r" })).is_none());
    }

    #[test]
    fn generate_auth_params_builds_a_login_url_with_challenge_and_uuid() {
        let params = generate_auth_params().expect("params");
        assert!(params.login_url.starts_with(LOGIN_URL));
        assert!(params.login_url.contains("redirectTarget=cli"));
        assert!(params.login_url.contains(&format!("uuid={}", params.uuid)));
        assert!(!params.verifier.is_empty());
    }
}
