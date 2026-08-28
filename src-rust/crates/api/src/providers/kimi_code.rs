// providers/kimi_code.rs — Kimi Code provider (device-flow OAuth).
//
// Kimi Code serves an OpenAI-compatible API at api.kimi.com/coding/v1 behind a
// device-flow OAuth token. The wire is plain OpenAI-compatible, so this
// provider delegates request building to `OpenAiCompatProvider`; it only owns
// what Kimi adds on top:
//
//   - a Bearer access token that expires and is refreshed on demand, written
//     back to the account it came from;
//   - the fixed `X-Msh-*` device headers Kimi expects on every request.
//
// The OAuth flow itself (device authorization, polling, refresh) lives in
// `mikmik_core::kimi_oauth`.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::Stream;
use mikmik_core::kimi_oauth::{
    self, api_base, common_headers, is_expired, load_kimi_tokens_for_account, save_kimi_tokens,
    save_kimi_tokens_for_account, KimiTokens,
};
use mikmik_core::provider_id::ProviderId;
use tracing::{debug, warn};

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
};
use crate::providers::openai_compat::OpenAiCompatProvider;

pub struct KimiCodeProvider {
    id: ProviderId,
    /// Mutable token cache, updated in place when a refresh succeeds.
    tokens: Arc<Mutex<KimiTokens>>,
    /// Account these tokens came from, when one was named. A refresh must be
    /// written back to the same account; `None` writes through the active one.
    account: Option<String>,
}

impl KimiCodeProvider {
    fn new(tokens: KimiTokens) -> Self {
        Self {
            id: ProviderId::new(ProviderId::KIMI_CODE),
            tokens: Arc::new(Mutex::new(tokens)),
            account: None,
        }
    }

    /// Construct from the active (or only) stored Kimi account.
    pub fn from_stored() -> Option<Self> {
        let tokens = kimi_oauth::get_kimi_tokens()?;
        if tokens.access_token.is_empty() {
            return None;
        }
        Some(Self::new(tokens))
    }

    /// Construct from one named account, bypassing the active pointer.
    pub fn from_account(account_id: &str) -> Option<Self> {
        let tokens = load_kimi_tokens_for_account(account_id)?;
        if tokens.access_token.is_empty() {
            return None;
        }
        let mut provider = Self::new(tokens);
        provider.account = Some(account_id.to_string());
        Some(provider)
    }

    /// Write refreshed tokens back to the account they were read from.
    fn persist_tokens(&self, updated: &KimiTokens) {
        let result = match self.account.as_deref() {
            Some(account_id) => save_kimi_tokens_for_account(updated, account_id),
            None => save_kimi_tokens(updated),
        };
        if let Err(e) = result {
            warn!("could not persist refreshed Kimi tokens: {e}");
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
            warn!("Kimi access token is expired and no refresh token is available");
            return Ok(token);
        };

        debug!("Kimi access token expired — refreshing");
        match kimi_oauth::refresh(&refresh).await {
            Ok(fresh) => {
                let access = fresh.access_token.clone();
                self.persist_tokens(&fresh);
                *self.tokens.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
                Ok(access)
            }
            Err(e) => Err(ProviderError::Other {
                provider: self.id.clone(),
                message: format!("Kimi token refresh failed: {e}"),
                status: None,
                body: None,
            }),
        }
    }

    /// Build the OpenAI-compatible delegate carrying a fresh Bearer token and
    /// the Kimi device headers.
    fn delegate(&self, token: String) -> OpenAiCompatProvider {
        let mut provider =
            OpenAiCompatProvider::new(ProviderId::KIMI_CODE, "Kimi Code", api_base())
                .with_api_key(token);
        for (name, value) in common_headers() {
            provider = provider.with_header(name, value);
        }
        provider
    }

    /// A delegate with a freshly resolved token, for the request paths.
    async fn ready_delegate(&self) -> Result<OpenAiCompatProvider, ProviderError> {
        let token = self.access_token().await?;
        Ok(self.delegate(token))
    }
}

#[async_trait]
impl LlmProvider for KimiCodeProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "Kimi Code"
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
        // Static: the token is only needed for live calls, so a placeholder is
        // enough to describe the OpenAI-compatible feature set.
        self.delegate(String::new()).capabilities()
    }
}
