//! The [`UsageProvider`] trait: one implementation per provider that can report
//! the signed-in account's quota.

use super::types::UsageReport;
use async_trait::async_trait;

/// What a usage fetch needs to reach a provider's quota endpoint.
///
/// A provider reads whichever fields its endpoint requires: an OAuth
/// `access_token`, a plain `api_key`, and a `base_url` override for a
/// non-default host (a proxy or a regional endpoint).
#[derive(Clone)]
pub struct UsageFetchContext {
    pub client: reqwest::Client,
    pub access_token: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

impl UsageFetchContext {
    /// A context carrying only an HTTP client, for header-only providers or
    /// tests.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            access_token: None,
            api_key: None,
            base_url: None,
        }
    }
}

/// Reports the signed-in account's quota for one provider.
///
/// A provider fills a [`UsageReport`] two ways: [`parse_rate_limit_headers`]
/// reads the meters off a normal response's headers (free, no extra request),
/// and [`fetch_usage`] calls the provider's own usage endpoint (an extra
/// request, richer data). A provider may implement either or both.
///
/// [`parse_rate_limit_headers`]: UsageProvider::parse_rate_limit_headers
/// [`fetch_usage`]: UsageProvider::fetch_usage
#[async_trait]
pub trait UsageProvider: Send + Sync {
    /// The provider id this reports for, matching the `ProviderId` string.
    fn id(&self) -> &str;

    /// Fetches usage from the provider's own quota endpoint. `Ok(None)` means
    /// the provider has no endpoint or the account is not eligible; `Err` means
    /// the request failed and the caller should keep the last good report.
    async fn fetch_usage(&self, _ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        Ok(None)
    }

    /// Parses the meters off a normal response's rate-limit headers. `None` when
    /// the headers carry no usable rate-limit fields. `now_ms` is the current
    /// time, injected so tests are deterministic.
    fn parse_rate_limit_headers(
        &self,
        _headers: &reqwest::header::HeaderMap,
        _now_ms: u64,
    ) -> Option<UsageReport> {
        None
    }
}
