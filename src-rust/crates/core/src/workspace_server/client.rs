//! Talking to an organisation's configuration server.
//!
//! One type per answer the server gives, so a caller reads a field rather than
//! a status code. Failures are classified by variant and never by their text:
//! a caller has to tell "log in again" from "the network is down" to decide
//! whether the cached policy is still good enough to open a session with.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::config::{WorkspaceConfigError, WorkspaceSettings};

/// How long any single request may take.
///
/// A session must not hang on a server that is up but wedged; every caller
/// here has a working answer for "could not reach it".
const TIMEOUT_SECS: u64 = 15;

/// Why a call to the workspace server did not answer.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("{0}")]
    Config(#[from] WorkspaceConfigError),
    /// The session is gone, expired, or was never valid.
    #[error("the server did not accept this session; log in again")]
    Unauthorized,
    /// The server understood the request and said no.
    #[error("the server refused this ({status}): {message}")]
    Refused { status: u16, message: String },
    /// Nothing was reached: DNS, TLS, a refused connection, a timeout.
    #[error("could not reach the workspace server: {0}")]
    Transport(String),
    /// Something answered, but not in a shape this client can read.
    #[error("the workspace server answered something unreadable: {0}")]
    Malformed(String),
}

impl WorkspaceError {
    /// Whether trying again later could succeed without the user doing
    /// anything.
    ///
    /// A caller decides retries and cache fallbacks with this rather than by
    /// matching on the message, which changes whenever the wording does.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Refused { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

/// What a login answers.
#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub token: String,
    pub expires_in: u64,
    pub user: Account,
}

/// One account, as the server describes it.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub is_admin: bool,
}

/// A group the account belongs to.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
}

/// Who the session belongs to.
#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub is_admin: bool,
    #[serde(default)]
    pub groups: Vec<Group>,
}

/// One provider the organisation has assigned to this account.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct EntitledProvider {
    pub name: String,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub api_base: Option<String>,
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// `llm` (the default when a server omits it) or `web_search`. Decides
    /// whether this becomes a `settings.json` model account or a search key
    /// written only to `auth.json`.
    #[serde(default)]
    pub kind: Option<String>,
}

impl EntitledProvider {
    /// Whether this entitlement is a web-search key rather than a model account.
    ///
    /// Anything but the explicit `web_search` kind, the absent kind included,
    /// is a model account, so a server that has never heard of the field keeps
    /// handing out LLM providers exactly as before.
    pub fn is_web_search(&self) -> bool {
        self.kind.as_deref() == Some("web_search")
    }
}

/// What a policy fetch found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyFetch {
    /// The organisation has not written one.
    Unset,
    /// The checksum we sent is still current, so the cache stands.
    Unchanged,
    Fetched {
        settings: Value,
        checksum: String,
    },
}

/// A settings backup as the server holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBackup {
    pub settings: Value,
    pub version: i64,
    pub checksum: String,
}

/// What an upload did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupWrite {
    Stored {
        version: i64,
        checksum: String,
    },
    /// Another machine wrote first. Nothing was stored.
    Conflict {
        current_version: i64,
    },
}

/// A connection to one organisation's server.
pub struct WorkspaceClient {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl WorkspaceClient {
    /// Build a client for an address, refusing one that is not safe to send a
    /// password to.
    pub fn new(settings: &WorkspaceSettings) -> Result<Self, WorkspaceError> {
        settings.validate()?;
        Ok(Self {
            base: settings.base().to_string(),
            token: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(TIMEOUT_SECS))
                .build()
                .map_err(|e| WorkspaceError::Transport(e.to_string()))?,
        })
    }

    /// Carry a session on every following call.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// The address this client talks to.
    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Exchange an address and a password for a session.
    ///
    /// The password is passed in and dropped here. Nothing writes it anywhere,
    /// and only the token it bought is ever persisted.
    pub async fn login(&self, email: &str, password: &str) -> Result<Session, WorkspaceError> {
        let response = self
            .http
            .post(self.url("/api/v1/login"))
            .json(&serde_json::json!({ "email": email, "password": password }))
            .send()
            .await
            .map_err(transport)?;

        let status = response.status().as_u16();
        if status == 401 {
            return Err(WorkspaceError::Unauthorized);
        }
        let text = body_text(response).await?;
        if !(200..300).contains(&status) {
            return Err(refused(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| WorkspaceError::Malformed(e.to_string()))
    }

    /// End this session on the server.
    pub async fn logout(&self) -> Result<(), WorkspaceError> {
        let response = self
            .authorized(self.http.post(self.url("/api/v1/logout")))
            .send()
            .await
            .map_err(transport)?;
        // A session the server has already dropped is the state the caller
        // wanted, so it is not an error to ask twice.
        let status = response.status().as_u16();
        if status == 401 || (200..300).contains(&status) {
            return Ok(());
        }
        Err(refused(status, &body_text(response).await?))
    }

    /// Who this session belongs to.
    pub async fn me(&self) -> Result<Identity, WorkspaceError> {
        self.get_json("/api/v1/me").await
    }

    /// Every provider this account may use, with its key.
    pub async fn providers(&self) -> Result<Vec<EntitledProvider>, WorkspaceError> {
        self.get_json("/api/v1/providers").await
    }

    /// The organisation's settings policy.
    ///
    /// Passing the checksum already held turns most polls into a 304 with no
    /// body, which is what makes a timer cheap enough to leave running.
    pub async fn policy(&self, known: Option<&str>) -> Result<PolicyFetch, WorkspaceError> {
        let mut request = self.authorized(self.http.get(self.url("/api/v1/policy")));
        if let Some(checksum) = known {
            request = request.header(reqwest::header::IF_NONE_MATCH, checksum);
        }
        let response = request.send().await.map_err(transport)?;

        let status = response.status().as_u16();
        if status == 401 {
            return Err(WorkspaceError::Unauthorized);
        }
        if status == 304 {
            return Ok(PolicyFetch::Unchanged);
        }
        if status == 204 {
            return Ok(PolicyFetch::Unset);
        }
        let checksum = header(&response, reqwest::header::ETAG.as_str());
        let text = body_text(response).await?;
        if !(200..300).contains(&status) {
            return Err(refused(status, &text));
        }
        Ok(PolicyFetch::Fetched {
            settings: serde_json::from_str(&text)
                .map_err(|e| WorkspaceError::Malformed(e.to_string()))?,
            checksum: checksum.unwrap_or_default(),
        })
    }

    /// This account's settings backup, if it has ever uploaded one.
    pub async fn backup(&self) -> Result<Option<StoredBackup>, WorkspaceError> {
        let response = self
            .authorized(self.http.get(self.url("/api/v1/settings")))
            .send()
            .await
            .map_err(transport)?;

        let status = response.status().as_u16();
        if status == 401 {
            return Err(WorkspaceError::Unauthorized);
        }
        if status == 204 {
            return Ok(None);
        }
        let text = body_text(response).await?;
        if !(200..300).contains(&status) {
            return Err(refused(status, &text));
        }

        #[derive(Deserialize)]
        struct Wire {
            settings: Value,
            version: i64,
            #[serde(default)]
            checksum: String,
        }
        let wire: Wire =
            serde_json::from_str(&text).map_err(|e| WorkspaceError::Malformed(e.to_string()))?;
        Ok(Some(StoredBackup {
            settings: wire.settings,
            version: wire.version,
            checksum: wire.checksum,
        }))
    }

    /// Upload a backup, replacing the version this machine last read.
    ///
    /// `expected_version` is 0 for the first upload. A version that has moved
    /// on answers `Conflict` and writes nothing, so a second machine cannot
    /// delete a change it never saw.
    pub async fn put_backup(
        &self,
        settings: &Value,
        expected_version: i64,
    ) -> Result<BackupWrite, WorkspaceError> {
        let response = self
            .authorized(self.http.put(self.url("/api/v1/settings")))
            .header(reqwest::header::IF_MATCH, expected_version.to_string())
            .json(settings)
            .send()
            .await
            .map_err(transport)?;

        let status = response.status().as_u16();
        if status == 401 {
            return Err(WorkspaceError::Unauthorized);
        }
        let text = body_text(response).await?;

        if status == 409 {
            #[derive(Deserialize)]
            struct Wire {
                current_version: i64,
            }
            let wire: Wire = serde_json::from_str(&text)
                .map_err(|e| WorkspaceError::Malformed(e.to_string()))?;
            return Ok(BackupWrite::Conflict {
                current_version: wire.current_version,
            });
        }
        if !(200..300).contains(&status) {
            return Err(refused(status, &text));
        }

        #[derive(Deserialize)]
        struct Wire {
            version: i64,
            #[serde(default)]
            checksum: String,
        }
        let wire: Wire =
            serde_json::from_str(&text).map_err(|e| WorkspaceError::Malformed(e.to_string()))?;
        Ok(BackupWrite::Stored {
            version: wire.version,
            checksum: wire.checksum,
        })
    }

    /// Remove this account's backup from the server.
    pub async fn clear_backup(&self) -> Result<bool, WorkspaceError> {
        let response = self
            .authorized(self.http.delete(self.url("/api/v1/settings")))
            .send()
            .await
            .map_err(transport)?;

        let status = response.status().as_u16();
        if status == 401 {
            return Err(WorkspaceError::Unauthorized);
        }
        if status == 404 {
            return Ok(false);
        }
        if !(200..300).contains(&status) {
            return Err(refused(status, &body_text(response).await?));
        }
        Ok(true)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, WorkspaceError> {
        let response = self
            .authorized(self.http.get(self.url(path)))
            .send()
            .await
            .map_err(transport)?;

        let status = response.status().as_u16();
        if status == 401 {
            return Err(WorkspaceError::Unauthorized);
        }
        let text = body_text(response).await?;
        if !(200..300).contains(&status) {
            return Err(refused(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| WorkspaceError::Malformed(e.to_string()))
    }
}

fn transport(error: reqwest::Error) -> WorkspaceError {
    WorkspaceError::Transport(error.to_string())
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

async fn body_text(response: reqwest::Response) -> Result<String, WorkspaceError> {
    response.text().await.map_err(transport)
}

/// Turn a refusal into an error carrying whatever the server explained.
///
/// The server answers `{"error": "..."}` on a refusal, and that sentence names
/// the key or the version the caller got wrong. Dropping it would leave the
/// user with a number.
fn refused(status: u16, body: &str) -> WorkspaceError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    WorkspaceError::Refused { status, message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(url: &str) -> WorkspaceSettings {
        WorkspaceSettings {
            url: url.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_client_refuses_an_address_it_should_not_send_a_password_to() {
        assert!(WorkspaceClient::new(&settings("http://mikmik.firma.com")).is_err());
        assert!(WorkspaceClient::new(&settings("")).is_err());
        assert!(WorkspaceClient::new(&settings("ftp://firma.com")).is_err());
    }

    #[test]
    fn only_the_web_search_kind_is_a_search_provider() {
        let mut provider = EntitledProvider {
            name: "tavily".to_string(),
            protocol: None,
            api_base: None,
            api_key: "k".to_string(),
            models: Vec::new(),
            kind: Some("web_search".to_string()),
        };
        assert!(provider.is_web_search());

        // An LLM entitlement, and one from a server that never sends the field,
        // are both model accounts.
        provider.kind = Some("llm".to_string());
        assert!(!provider.is_web_search());
        provider.kind = None;
        assert!(!provider.is_web_search());
    }

    #[test]
    fn a_client_accepts_tls_and_a_local_address() {
        assert!(WorkspaceClient::new(&settings("https://mikmik.firma.com")).is_ok());
        assert!(WorkspaceClient::new(&settings("http://127.0.0.1:8420")).is_ok());
        assert!(WorkspaceClient::new(&settings("http://localhost:8420")).is_ok());
    }

    #[test]
    fn a_trailing_slash_does_not_double_up_in_a_path() {
        let client = WorkspaceClient::new(&settings("https://firma.com/")).expect("client");
        assert_eq!(client.url("/api/v1/me"), "https://firma.com/api/v1/me");
    }

    #[test]
    fn only_a_reachable_failure_is_worth_retrying() {
        // A caller decides between "use the cache" and "tell the user to log
        // in again" on this, so the split has to hold per variant.
        assert!(WorkspaceError::Transport("timed out".into()).is_retryable());
        assert!(WorkspaceError::Refused {
            status: 503,
            message: String::new()
        }
        .is_retryable());
        assert!(!WorkspaceError::Unauthorized.is_retryable());
        assert!(!WorkspaceError::Refused {
            status: 400,
            message: String::new()
        }
        .is_retryable());
        assert!(!WorkspaceError::Malformed("junk".into()).is_retryable());
    }

    #[test]
    fn a_refusal_keeps_the_sentence_the_server_wrote() {
        // The server names the key or the version the caller got wrong; a bare
        // status code would leave the user with nothing to act on.
        let error = refused(
            400,
            &json!({ "error": "a policy may not set `hooks`" }).to_string(),
        );
        assert!(error.to_string().contains("hooks"), "{error}");
    }

    #[test]
    fn a_refusal_with_no_json_still_says_something() {
        let error = refused(502, "<html>Bad Gateway</html>");
        assert!(error.to_string().contains("Bad Gateway"), "{error}");
    }

    #[test]
    fn a_refusal_body_cannot_flood_the_message() {
        let error = refused(500, &"x".repeat(10_000));
        assert!(error.to_string().len() < 400, "the whole body came through");
    }
}
