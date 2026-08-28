// providers/gitlab_duo.rs — GitLab Duo provider.
//
// Two credentials sit behind every request. The stored GitLab token (an OAuth
// access token or a `GITLAB_TOKEN` PAT) is exchanged for a short-lived
// direct-access token plus gateway headers; those authenticate the
// OpenAI-compatible proxy at cloud.gitlab.com/ai/v1/proxy/openai/v1, to which
// request building is delegated via `OpenAiCompatProvider`.
//
// The direct-access grant is cached until it nears expiry, so a run of requests
// does not re-exchange on every turn. An expired OAuth token is refreshed first
// (a PAT, which has no refresh token, is used as-is).

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::Stream;
use mikmik_core::gitlab_duo::{
    self, is_expired, load_gitlab_tokens_for_account, save_gitlab_tokens,
    save_gitlab_tokens_for_account, DirectAccess, GitlabTokens, DIRECT_ACCESS_TTL_SECS,
    OPENAI_PROXY_URL,
};
use mikmik_core::provider_id::ProviderId;
use tracing::{debug, warn};

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
};
use crate::providers::openai_compat::OpenAiCompatProvider;

pub struct GitlabDuoProvider {
    id: ProviderId,
    tokens: Arc<Mutex<GitlabTokens>>,
    account: Option<String>,
    /// Cached direct-access grant and the unix time it should be re-exchanged.
    direct_access: Arc<Mutex<Option<(DirectAccess, u64)>>>,
}

impl GitlabDuoProvider {
    fn new(tokens: GitlabTokens) -> Self {
        Self {
            id: ProviderId::new(ProviderId::GITLAB_DUO),
            tokens: Arc::new(Mutex::new(tokens)),
            account: None,
            direct_access: Arc::new(Mutex::new(None)),
        }
    }

    /// Construct from the active (or only) stored account, or a `GITLAB_TOKEN`
    /// PAT when nothing is stored.
    pub fn from_stored() -> Option<Self> {
        if let Some(tokens) = gitlab_duo::get_gitlab_tokens() {
            if !tokens.access_token.is_empty() {
                return Some(Self::new(tokens));
            }
        }
        let pat = gitlab_duo::pat_from_env()?;
        Some(Self::new(GitlabTokens {
            access_token: pat,
            ..Default::default()
        }))
    }

    /// Construct from one named account, bypassing the active pointer.
    pub fn from_account(account_id: &str) -> Option<Self> {
        let tokens = load_gitlab_tokens_for_account(account_id)?;
        if tokens.access_token.is_empty() {
            return None;
        }
        let mut provider = Self::new(tokens);
        provider.account = Some(account_id.to_string());
        Some(provider)
    }

    fn persist_tokens(&self, updated: &GitlabTokens) {
        let result = match self.account.as_deref() {
            Some(account_id) => save_gitlab_tokens_for_account(updated, account_id),
            None => save_gitlab_tokens(updated),
        };
        if let Err(e) = result {
            warn!("could not persist refreshed GitLab tokens: {e}");
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// The current GitLab access token, refreshing an expired OAuth token first.
    async fn gitlab_access_token(&self) -> Result<String, ProviderError> {
        let (token, expired, refresh_token) = {
            let guard = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
            (
                guard.access_token.clone(),
                is_expired(&guard),
                guard.refresh_token.clone(),
            )
        };

        if !expired {
            return Ok(token);
        }

        let Some(refresh) = refresh_token else {
            // A PAT (no refresh token) is used as-is.
            return Ok(token);
        };

        debug!("GitLab access token expired — refreshing");
        match gitlab_duo::refresh(&refresh).await {
            Ok(fresh) => {
                let access = fresh.access_token.clone();
                self.persist_tokens(&fresh);
                *self.tokens.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
                // A new GitLab token invalidates any cached direct-access grant.
                *self.direct_access.lock().unwrap_or_else(|p| p.into_inner()) = None;
                Ok(access)
            }
            Err(e) => Err(ProviderError::Other {
                provider: self.id.clone(),
                message: format!("GitLab token refresh failed: {e}"),
                status: None,
                body: None,
            }),
        }
    }

    /// A valid direct-access grant, exchanging (and caching) when needed.
    async fn ensure_direct_access(&self) -> Result<DirectAccess, ProviderError> {
        if let Some((grant, expires_at)) = self
            .direct_access
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            if Self::now_secs() < *expires_at {
                return Ok(grant.clone());
            }
        }

        let gitlab_token = self.gitlab_access_token().await?;
        let grant = gitlab_duo::direct_access(&gitlab_token)
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: e,
                status: None,
                body: None,
            })?;

        let expires_at = Self::now_secs() + DIRECT_ACCESS_TTL_SECS;
        *self.direct_access.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((grant.clone(), expires_at));
        Ok(grant)
    }

    fn delegate_from(grant: &DirectAccess) -> OpenAiCompatProvider {
        let mut provider =
            OpenAiCompatProvider::new(ProviderId::GITLAB_DUO, "GitLab Duo", OPENAI_PROXY_URL)
                .with_api_key(grant.token.clone());
        for (name, value) in &grant.headers {
            provider = provider.with_header(name.clone(), value.clone());
        }
        provider
    }

    async fn ready_delegate(&self) -> Result<OpenAiCompatProvider, ProviderError> {
        let grant = self.ensure_direct_access().await?;
        Ok(Self::delegate_from(&grant))
    }
}

#[async_trait]
impl LlmProvider for GitlabDuoProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "GitLab Duo"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.ready_delegate().await?.create_message(request).await
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.ready_delegate()
            .await?
            .create_message_stream(request)
            .await
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.ready_delegate().await?.discover_models().await
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        self.ready_delegate().await?.health_check().await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // Static: describe the OpenAI-compatible feature set without a token.
        OpenAiCompatProvider::new(ProviderId::GITLAB_DUO, "GitLab Duo", OPENAI_PROXY_URL)
            .capabilities()
    }
}
