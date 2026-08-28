// providers/xai_oauth.rs — xAI Grok OAuth provider (SuperGrok / X Premium+).
//
// The authed endpoint is the same OpenAI-compatible api.x.ai/v1 that the
// API-key `xai` provider uses, so this provider delegates request building to
// `OpenAiCompatProvider`. It owns only the OAuth token: a Bearer that expires
// and is refreshed on demand (the refresh discovers the token endpoint from
// xAI's OIDC document), written back to the account it came from.
//
// The OAuth flow itself lives in `mikmik_core::xai_oauth`.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::Stream;
use mikmik_core::provider_id::ProviderId;
use mikmik_core::xai_oauth::{
    self, api_base, is_expired, load_xai_tokens_for_account, save_xai_tokens,
    save_xai_tokens_for_account, XaiTokens,
};
use tracing::{debug, warn};

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
};
use crate::providers::openai_compat::OpenAiCompatProvider;

pub struct XaiOAuthProvider {
    id: ProviderId,
    tokens: Arc<Mutex<XaiTokens>>,
    account: Option<String>,
}

impl XaiOAuthProvider {
    fn new(tokens: XaiTokens) -> Self {
        Self {
            id: ProviderId::new(ProviderId::XAI_OAUTH),
            tokens: Arc::new(Mutex::new(tokens)),
            account: None,
        }
    }

    /// Construct from the active (or only) stored xAI OAuth account.
    pub fn from_stored() -> Option<Self> {
        let tokens = xai_oauth::get_xai_tokens()?;
        if tokens.access_token.is_empty() {
            return None;
        }
        Some(Self::new(tokens))
    }

    /// Construct from one named account, bypassing the active pointer.
    pub fn from_account(account_id: &str) -> Option<Self> {
        let tokens = load_xai_tokens_for_account(account_id)?;
        if tokens.access_token.is_empty() {
            return None;
        }
        let mut provider = Self::new(tokens);
        provider.account = Some(account_id.to_string());
        Some(provider)
    }

    fn persist_tokens(&self, updated: &XaiTokens) {
        let result = match self.account.as_deref() {
            Some(account_id) => save_xai_tokens_for_account(updated, account_id),
            None => save_xai_tokens(updated),
        };
        if let Err(e) = result {
            warn!("could not persist refreshed xAI tokens: {e}");
        }
    }

    /// The current access token, refreshing first if it is expired.
    async fn access_token(&self) -> Result<String, ProviderError> {
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
            warn!("xAI access token is expired and no refresh token is available");
            return Ok(token);
        };

        debug!("xAI access token expired — refreshing");
        match xai_oauth::refresh(&refresh).await {
            Ok(fresh) => {
                let access = fresh.access_token.clone();
                self.persist_tokens(&fresh);
                *self.tokens.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
                Ok(access)
            }
            Err(e) => Err(ProviderError::Other {
                provider: self.id.clone(),
                message: format!("xAI token refresh failed: {e}"),
                status: None,
                body: None,
            }),
        }
    }

    fn delegate(&self, token: String) -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(ProviderId::XAI_OAUTH, "xAI Grok (OAuth)", api_base())
            .with_api_key(token)
    }

    async fn ready_delegate(&self) -> Result<OpenAiCompatProvider, ProviderError> {
        let token = self.access_token().await?;
        Ok(self.delegate(token))
    }
}

#[async_trait]
impl LlmProvider for XaiOAuthProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "xAI Grok (OAuth)"
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
        self.delegate(String::new()).capabilities()
    }
}
