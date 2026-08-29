//! zai_oauth.rs — Z.AI (GLM Coding Plan) browser login.
//!
//! Z.AI signs in with a browser OAuth authorization-code flow (no PKCE) against
//! chat.z.ai on a fixed loopback port. The flow does not leave an OAuth token to
//! store: it mints a durable Z.AI API key through ZCode's business API
//! (business login → resolve the default org/project → find or create a named
//! key → copy its secret) and returns `apiKey.secretKey`. That key is stored as
//! an ordinary `zai` API-key account, so no `StoredCredential` OAuth variant is
//! needed.
//!
//! The URL builder and the envelope helper are pure so they can be tested
//! without a live endpoint. The network steps (code exchange, key mint) are
//! async.

use serde_json::{json, Value};

use crate::provider_id::ProviderId;

/// The fixed loopback port Z.AI's OAuth allowlist accepts. The redirect URI has
/// to match exactly, so a port conflict fails rather than falling back to a
/// random port that Z.AI would reject.
pub const CALLBACK_PORT: u16 = 54548;

/// The loopback callback path the redirect URI carries.
pub const CALLBACK_PATH: &str = "/callback";

/// The name this client gives the API key it provisions on the user's account.
const KEY_NAME: &str = "mikmik";

/// A completed Z.AI login: the durable API key and the account it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZaiLogin {
    pub api_key: String,
    pub email: Option<String>,
}

fn env_or(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn client_id() -> String {
    env_or("ZAI_OAUTH_CLIENT_ID", "client_P8X5CMWmlaRO9gyO-KSqtg")
}
fn authorize_base() -> String {
    env_or(
        "ZAI_OAUTH_AUTHORIZE_URL",
        "https://chat.z.ai/api/oauth/authorize",
    )
}
fn token_url() -> String {
    env_or(
        "ZAI_OAUTH_TOKEN_URL",
        "https://zcode.z.ai/api/v1/oauth/token",
    )
}
fn biz_base() -> String {
    env_or("ZAI_BIZ_BASE", "https://api.z.ai")
}
fn business_login_url() -> String {
    env_or(
        "ZAI_BUSINESS_LOGIN_URL",
        "https://api.z.ai/api/auth/z/login",
    )
}

/// The redirect URI the loopback flow listens on.
pub fn redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

/// Build the authorization URL the user opens in their browser. No PKCE, to
/// match ZCode's authorize request verbatim.
pub fn authorize_url(state: &str) -> String {
    let params = [
        ("redirect_uri", redirect_uri()),
        ("response_type", "code".to_string()),
        ("client_id", client_id()),
        ("state", state.to_string()),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{query}", authorize_base())
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

/// Whether an envelope's `code` names success. The OAuth token endpoint answers
/// `0`; the biz endpoints answer `200`. An absent code is neutral.
fn is_success_code(code: Option<i64>) -> bool {
    matches!(code, None | Some(0) | Some(200))
}

/// Unwrap Z.ai's `{ code, msg, data, success }` envelope, returning the `data`
/// payload on success. A body with neither field is returned as-is.
fn unwrap_envelope(body: Value, operation: &str) -> Result<Value, String> {
    let has_envelope = body.get("code").is_some() || body.get("success").is_some();
    if !has_envelope {
        return Ok(body);
    }
    let success = body.get("success").and_then(|v| v.as_bool());
    let code = body.get("code").and_then(|v| v.as_i64());
    if success == Some(false) || !is_success_code(code) {
        let msg = body
            .get("msg")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("code {code:?}"));
        return Err(format!("Z.ai {operation} failed: {msg}"));
    }
    Ok(body.get("data").cloned().unwrap_or(body))
}

fn trimmed(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

async fn post_json(url: &str, body: Value, bearer: Option<&str>) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let mut req = client
        .post(url)
        .header("Accept", "application/json")
        .json(&body);
    if let Some(token) = bearer {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("Z.ai request failed: {url}: {e}"))?;
    read_json(resp, url).await
}

async fn get_json(url: &str, bearer: &str) -> Result<Value, String> {
    let resp = reqwest::Client::new()
        .get(url)
        .header("Accept", "application/json")
        .bearer_auth(bearer)
        .send()
        .await
        .map_err(|e| format!("Z.ai request failed: {url}: {e}"))?;
    read_json(resp, url).await
}

async fn read_json(resp: reqwest::Response, url: &str) -> Result<Value, String> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Z.ai request failed: {} {url}: {text}",
            status.as_u16()
        ));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("Z.ai response was not JSON: {url}: {e}"))
}

/// Exchange the callback code for a short-lived OAuth access token, then mint a
/// durable API key from it.
pub async fn login(code: &str, state: &str) -> Result<ZaiLogin, String> {
    let body = post_json(
        &token_url(),
        json!({ "provider": "zai", "code": code, "redirect_uri": redirect_uri(), "state": state }),
        None,
    )
    .await?;
    let data = unwrap_envelope(body, "token exchange")?;
    let access_token = trimmed(data.pointer("/zai/access_token"))
        .ok_or_else(|| "Z.ai token response missing access token".to_string())?;
    let email = trimmed(data.pointer("/user/email"));

    let api_key = mint_api_key(&access_token).await?;
    Ok(ZaiLogin { api_key, email })
}

/// Exchange the short-lived OAuth token for a durable biz token.
async fn business_login(access_token: &str) -> Result<String, String> {
    let data = unwrap_envelope(
        post_json(
            &business_login_url(),
            json!({ "token": access_token }),
            None,
        )
        .await?,
        "business login",
    )?;
    trimmed(data.get("access_token"))
        .or_else(|| trimmed(data.get("accessToken")))
        .ok_or_else(|| "Z.ai business login returned no access token".to_string())
}

/// The default (or first) `id` under `field` in a list of objects.
fn default_id(items: &Value, list_key: &str, id_key: &str) -> Option<String> {
    let list = items.get(list_key).and_then(|v| v.as_array())?;
    let chosen = list
        .iter()
        .find(|item| item.get("isDefault").and_then(|v| v.as_bool()) == Some(true))
        .or_else(|| list.first())?;
    trimmed(chosen.get(id_key))
}

/// Provision the durable Z.ai API key: business login → resolve org/project →
/// find or create the named key → copy its secret → `apiKey.secretKey`.
async fn mint_api_key(access_token: &str) -> Result<String, String> {
    let biz = business_login(access_token).await?;

    let customer = unwrap_envelope(
        get_json(
            &format!("{}/api/biz/customer/getCustomerInfo", biz_base()),
            &biz,
        )
        .await?,
        "customer lookup",
    )?;
    let org = customer
        .get("organizations")
        .and_then(|v| v.as_array())
        .and_then(|orgs| {
            orgs.iter()
                .find(|o| o.get("isDefault").and_then(|v| v.as_bool()) == Some(true))
                .or_else(|| orgs.first())
        })
        .cloned()
        .unwrap_or(Value::Null);
    let organization_id = trimmed(org.get("organizationId"));
    let project_id = default_id(&org, "projects", "projectId");
    let (organization_id, project_id) = match (organization_id, project_id) {
        (Some(o), Some(p)) => (o, p),
        _ => return Err("Z.ai key provisioning failed: no organization/project on account".into()),
    };

    let keys_url = format!(
        "{}/api/biz/v1/organization/{organization_id}/projects/{project_id}/api_keys",
        biz_base()
    );
    let api_key = provision_key(&keys_url, &biz).await?;
    let copied = unwrap_envelope(
        get_json(&format!("{keys_url}/copy/{}", urlencode(&api_key)), &biz).await?,
        "api key copy",
    )?;
    let secret = trimmed(copied.get("secretKey"))
        .ok_or_else(|| "Z.ai key provisioning returned no secretKey".to_string())?;
    Ok(format!("{api_key}.{secret}"))
}

/// The `apiKey` of the named key, found in the list or created.
async fn provision_key(keys_url: &str, biz: &str) -> Result<String, String> {
    let list = unwrap_envelope(get_json(keys_url, biz).await?, "api key list")?;
    let existing = key_array(&list)
        .into_iter()
        .find(|k| k.get("name").and_then(|v| v.as_str()) == Some(KEY_NAME));
    let record = match existing {
        Some(k) => k,
        None => unwrap_envelope(
            post_json(keys_url, json!({ "name": KEY_NAME }), Some(biz)).await?,
            "api key create",
        )?,
    };
    trimmed(record.get("apiKey"))
        .ok_or_else(|| "Z.ai key provisioning returned no apiKey".to_string())
}

/// Coerce an api-keys list response (bare array or a common wrapper) to a vec.
fn key_array(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    for field in ["list", "keys", "apiKeys", "records"] {
        if let Some(arr) = value.get(field).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Store the minted key as a `zai` API-key account and make it active.
///
/// The flow yields a plain API key, so it is filed like any other `zai` key
/// rather than as an OAuth credential. Returns the account id.
pub fn save_zai_key_and_register(login: &ZaiLogin) -> anyhow::Result<String> {
    let settings = crate::config::Settings::load_sync().unwrap_or_default();
    let base = login
        .email
        .clone()
        .unwrap_or_else(|| ProviderId::ZAI.to_string());
    let account_id = settings
        .effective_config()
        .account_name_for_login(&base, ProviderId::ZAI);

    let mut store = crate::AuthStore::load();
    store.set(
        &account_id,
        crate::StoredCredential::api_key(login.api_key.clone()),
    );
    store.save();
    crate::config::register_account(&account_id, ProviderId::ZAI, true)?;
    Ok(account_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_carries_client_id_redirect_and_state() {
        let url = authorize_url("st-123");
        assert!(url.starts_with("https://chat.z.ai/api/oauth/authorize?"));
        assert!(url.contains("client_id=client_P8X5CMWmlaRO9gyO-KSqtg"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("state=st-123"));
        // The redirect is the fixed loopback port, percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A54548%2Fcallback"));
    }

    #[test]
    fn an_envelope_unwraps_its_data_on_success() {
        let ok_code = json!({ "code": 0, "msg": "ok", "data": { "x": 1 } });
        assert_eq!(unwrap_envelope(ok_code, "t").unwrap(), json!({ "x": 1 }));

        let ok_success = json!({ "success": true, "data": { "y": 2 } });
        assert_eq!(unwrap_envelope(ok_success, "t").unwrap(), json!({ "y": 2 }));

        // A biz endpoint answers 200.
        let ok_200 = json!({ "code": 200, "data": { "z": 3 } });
        assert_eq!(unwrap_envelope(ok_200, "t").unwrap(), json!({ "z": 3 }));
    }

    #[test]
    fn an_envelope_error_carries_its_message() {
        let failed = json!({ "code": 401, "msg": "unauthorized" });
        let err = unwrap_envelope(failed, "token exchange").unwrap_err();
        assert!(err.contains("token exchange"), "{err}");
        assert!(err.contains("unauthorized"), "{err}");

        let explicit = json!({ "success": false, "msg": "nope", "data": { "x": 1 } });
        assert!(unwrap_envelope(explicit, "t").is_err());
    }

    #[test]
    fn a_body_without_an_envelope_passes_through() {
        let raw = json!({ "apiKey": "ak", "secretKey": "sk" });
        assert_eq!(unwrap_envelope(raw.clone(), "t").unwrap(), raw);
    }

    #[test]
    fn the_default_id_prefers_is_default_then_first() {
        let holder = json!({
            "projects": [
                { "projectId": "p1" },
                { "projectId": "p2", "isDefault": true },
            ]
        });
        assert_eq!(
            default_id(&holder, "projects", "projectId").as_deref(),
            Some("p2")
        );

        let first = json!({ "projects": [ { "projectId": "p1" }, { "projectId": "p2" } ] });
        assert_eq!(
            default_id(&first, "projects", "projectId").as_deref(),
            Some("p1")
        );
    }

    #[test]
    fn a_key_list_is_read_from_a_bare_array_or_a_wrapper() {
        let bare = json!([{ "name": "mikmik" }]);
        assert_eq!(key_array(&bare).len(), 1);
        let wrapped = json!({ "list": [{ "name": "a" }, { "name": "b" }] });
        assert_eq!(key_array(&wrapped).len(), 2);
        assert!(key_array(&json!({})).is_empty());
    }
}
