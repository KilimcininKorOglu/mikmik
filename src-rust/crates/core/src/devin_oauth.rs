//! devin_oauth.rs — Devin / Windsurf Cascade OAuth (PKCE loopback session token).
//!
//! Devin signs in with a PKCE authorization-code flow against a loopback
//! redirect (`http://127.0.0.1:59653/callback`): the browser lands on
//! `app.devin.ai/auth/cli/continue`, and the captured code is exchanged at
//! `api.devin.ai/auth/cli/token` for a long-lived session token (a JWT). That
//! session token is not sent to the inference endpoint directly — the provider
//! exchanges it for a short-lived user JWT via the Cascade auth RPC on each
//! session (see `providers/devin`). Only the session token is persisted here.
//!
//! The authorize-URL builder, the token/JWT parsers and the expiry check are
//! pure; the code exchange is async.

use serde::{Deserialize, Serialize};

/// The Devin web app that hosts the CLI sign-in page.
pub const WEBAPP_URL: &str = "https://app.devin.ai";

/// The Devin API that exchanges the authorization code for a session token.
pub const API_URL: &str = "https://api.devin.ai";

/// The fixed loopback port the CLI sign-in page redirects to.
pub const CALLBACK_PORT: u16 = 59653;

/// The loopback callback path.
pub const CALLBACK_PATH: &str = "/callback";

/// Treat a token as expired this many seconds early.
const EXPIRY_SKEW_SECS: u64 = 300;

/// Conservative lifetime for a session token whose JWT carries no `exp`.
const FALLBACK_TTL_SECS: u64 = 365 * 24 * 60 * 60;

/// The redirect URI the loopback flow listens on.
pub fn redirect_uri() -> String {
    format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}")
}

/// Devin session token, persisted in the auth store under an account.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DevinTokens {
    /// The long-lived session token (a JWT) exchanged for a user JWT per turn.
    pub session_token: String,
    /// Unix timestamp in seconds when the session token expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// The account identity, from the session token's JWT `sub`, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Whether `tokens` should be treated as expired.
pub fn is_expired(tokens: &DevinTokens) -> bool {
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

fn jwt_claim(token: &str, claim: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let payload_b64 = token.split('.').nth(1)?;
    let mut padded = payload_b64.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get(claim).cloned()
}

/// The session token's expiry (`exp` claim) as a Unix second timestamp, or a
/// conservative fallback when the token is not a JWT.
pub fn token_expiry(token: &str) -> u64 {
    jwt_claim(token, "exp")
        .and_then(|v| v.as_u64())
        .map(|exp| exp.saturating_sub(EXPIRY_SKEW_SECS))
        .unwrap_or_else(|| now_secs() + FALLBACK_TTL_SECS)
}

/// The account identity inside a session token (JWT `sub`), when present.
pub fn token_subject(token: &str) -> Option<String> {
    jwt_claim(token, "sub")
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
}

/// Build the authorization URL the user opens in their browser.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let params = [
        ("redirect_uri", redirect_uri()),
        ("state", state.to_string()),
        ("prompt", "select_account".to_string()),
        ("code_challenge", challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{WEBAPP_URL}/auth/cli/continue?{query}")
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

/// Build [`DevinTokens`] from a raw session token.
pub fn tokens_from_session(token: &str) -> DevinTokens {
    DevinTokens {
        session_token: token.to_string(),
        expires_at: Some(token_expiry(token)),
        account_id: token_subject(token),
    }
}

/// Exchange an authorization code and PKCE verifier for a session token.
pub async fn exchange_code(code: &str, verifier: &str) -> Result<DevinTokens, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{API_URL}/auth/cli/token"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "code": code, "code_verifier": verifier }))
        .send()
        .await
        .map_err(|e| format!("Devin token exchange request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Devin token exchange failed: {} {text}",
            status.as_u16()
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Devin token response was not JSON: {e}"))?;
    let token = json
        .get("token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Devin token exchange returned an empty token".to_string())?;
    Ok(tokens_from_session(token))
}

// ---------------------------------------------------------------------------
// Account storage
// ---------------------------------------------------------------------------

/// Save Devin tokens under `account_id` (persists immediately).
pub fn save_devin_tokens_for_account(tokens: &DevinTokens, account_id: &str) -> anyhow::Result<()> {
    let mut store = crate::AuthStore::load();
    store.set_devin_tokens(account_id, tokens.clone());
    Ok(())
}

/// Load the Devin tokens stored for `account_id`.
pub fn load_devin_tokens_for_account(account_id: &str) -> Option<DevinTokens> {
    crate::AuthStore::load().devin_tokens(account_id).cloned()
}

/// Save Devin tokens, open the account's `providers` entry, and make it active.
/// Returns the account id used.
pub fn save_devin_tokens_and_register(tokens: &DevinTokens) -> anyhow::Result<String> {
    use crate::provider_id::ProviderId;

    let settings = crate::config::Settings::load_sync().unwrap_or_default();
    let config = settings.effective_config();
    let store = crate::AuthStore::load();

    let existing_id = tokens.account_id.as_ref().and_then(|identity| {
        store
            .accounts_for_protocol(ProviderId::DEVIN)
            .into_iter()
            .find(|id| {
                store
                    .devin_tokens(id)
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
            config.account_name_for_login(&base, ProviderId::DEVIN)
        }
    };

    save_devin_tokens_for_account(tokens, &id)?;
    crate::config::register_account(&id, ProviderId::DEVIN, true)?;
    Ok(id)
}

/// The active account, when it is a Devin account.
pub fn active_devin_account() -> Option<String> {
    let settings = crate::config::Settings::load_sync().ok()?;
    let active = settings.provider.clone()?;
    crate::AuthStore::load()
        .devin_tokens(&active)
        .map(|_| active)
}

/// The active account's Devin tokens, falling back to the only stored one.
pub fn get_devin_tokens() -> Option<DevinTokens> {
    let store = crate::AuthStore::load();
    if let Some(active) = active_devin_account() {
        return store.devin_tokens(&active).cloned();
    }
    let accounts = store.accounts_for_protocol(crate::provider_id::ProviderId::DEVIN);
    match accounts.as_slice() {
        [only] => store.devin_tokens(only).cloned(),
        _ => None,
    }
}

/// Save to the active account, registering a new one when the active account is
/// not a Devin account.
pub fn save_devin_tokens(tokens: &DevinTokens) -> anyhow::Result<()> {
    match active_devin_account() {
        Some(active) => save_devin_tokens_for_account(tokens, &active),
        None => save_devin_tokens_and_register(tokens).map(|_| ()),
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
    fn token_expiry_reads_the_exp_claim() {
        let token = jwt(json!({ "exp": 2_000_000_000u64 }));
        assert_eq!(token_expiry(&token), 2_000_000_000 - EXPIRY_SKEW_SECS);
    }

    #[test]
    fn token_expiry_falls_back_for_a_non_jwt() {
        let expiry = token_expiry("opaque");
        assert!(expiry > now_secs());
    }

    #[test]
    fn token_subject_reads_the_sub_claim() {
        let token = jwt(json!({ "sub": "user-7" }));
        assert_eq!(token_subject(&token), Some("user-7".to_string()));
        assert_eq!(token_subject("opaque"), None);
    }

    #[test]
    fn authorize_url_carries_pkce_and_the_loopback_redirect() {
        let url = authorize_url("chal-1", "state-1");
        assert!(url.starts_with(WEBAPP_URL));
        assert!(url.contains("code_challenge=chal-1"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("127.0.0.1%3A59653%2Fcallback"));
    }

    #[test]
    fn tokens_from_session_captures_identity_and_expiry() {
        let token = jwt(json!({ "sub": "u1", "exp": 2_000_000_000u64 }));
        let tokens = tokens_from_session(&token);
        assert_eq!(tokens.session_token, token);
        assert_eq!(tokens.account_id.as_deref(), Some("u1"));
        assert!(tokens.expires_at.is_some());
    }
}
