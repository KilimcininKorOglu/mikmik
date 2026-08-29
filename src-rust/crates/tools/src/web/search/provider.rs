// Search-provider abstraction and the auto-fallback chain.
//
// Ported from oh-my-pi `web/search/providers/base.ts` and the resolution logic
// in `provider.ts`. A provider maps its native response onto `SearchResponse`;
// the chain walks `SEARCH_PROVIDER_ORDER`, admitting each provider that reports
// itself available and falling through to the next on failure.

use super::query::StructuredQuery;
use super::types::{SearchProviderError, SearchProviderId, SearchResponse, SEARCH_PROVIDER_ORDER};
use crate::ToolContext;
use async_trait::async_trait;
use std::time::Duration;

/// How far back a search may reach.
///
/// One shape the tool understands, mapped to each backend's own parameter, so
/// the model names a window once and every backend that has one honours it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    /// Parse the model's word, or say which words are allowed.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            "year" => Ok(Self::Year),
            other => Err(format!(
                "recency must be one of day, week, month or year, not {other:?}"
            )),
        }
    }

    /// Brave's `freshness` code.
    pub fn brave_freshness(self) -> &'static str {
        match self {
            Self::Day => "pd",
            Self::Week => "pw",
            Self::Month => "pm",
            Self::Year => "py",
        }
    }

    /// The plain word (SearXNG `time_range`, Tavily `time_range`, and others).
    pub fn as_word(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// Everything a provider needs to service one search.
pub struct SearchParams<'a> {
    /// The raw query, verbatim.
    pub query: &'a str,
    /// The query parsed once by the pipeline (`site:`, dates, …).
    pub parsed: &'a StructuredQuery,
    /// Requested result count (already clamped by the caller).
    pub limit: usize,
    /// Optional temporal filter.
    pub recency: Option<Recency>,
    /// Hard timeout for this provider's transport.
    pub timeout: Duration,
    /// Tool context: config, credentials routing, cancellation.
    pub ctx: &'a ToolContext,
}

/// A web-search backend.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn id(&self) -> SearchProviderId;

    /// Whether the provider has the credentials/config it needs right now.
    /// Drives auto-chain admission.
    async fn is_available(&self, ctx: &ToolContext) -> bool;

    /// Whether the provider should run when explicitly selected, even if
    /// [`Self::is_available`] would reject it for the auto chain. Defaults to
    /// mirroring `is_available`.
    async fn is_explicitly_available(&self, ctx: &ToolContext) -> bool {
        self.is_available(ctx).await
    }

    /// Execute a search.
    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError>;
}

/// Build the provider for `id`, or `None` when it is not yet implemented.
///
/// The chain skips a `None` exactly as it skips an unavailable provider, so
/// providers can land incrementally without breaking `execute_search`.
pub fn provider_for(id: SearchProviderId) -> Option<Box<dyn SearchProvider>> {
    use super::providers;
    match id {
        SearchProviderId::Gemini => Some(Box::new(providers::gemini::GeminiProvider)),
        SearchProviderId::Anthropic => Some(Box::new(providers::anthropic::AnthropicProvider)),
        SearchProviderId::Exa => Some(Box::new(providers::exa::ExaProvider)),
        SearchProviderId::Tinyfish => Some(Box::new(providers::tinyfish::TinyFishProvider)),
        SearchProviderId::Jina => Some(Box::new(providers::jina::JinaProvider)),
        SearchProviderId::Kagi => Some(Box::new(providers::kagi::KagiProvider)),
        SearchProviderId::Synthetic => Some(Box::new(providers::synthetic::SyntheticProvider)),
        SearchProviderId::Searxng => Some(Box::new(providers::searxng::SearxngProvider)),
        SearchProviderId::Tavily => Some(Box::new(providers::tavily::TavilyProvider)),
        SearchProviderId::Firecrawl => Some(Box::new(providers::firecrawl::FirecrawlProvider)),
        SearchProviderId::Brave => Some(Box::new(providers::brave::BraveProvider)),
        SearchProviderId::Parallel => Some(Box::new(providers::parallel::ParallelProvider)),
        SearchProviderId::Duckduckgo => Some(Box::new(providers::duckduckgo::DuckDuckGoProvider)),
        _ => None,
    }
}

/// A provider the chain will try, and whether the caller named it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCandidate {
    pub id: SearchProviderId,
    pub explicit: bool,
}

/// The provider order to walk for `auto`, honouring an exclusion set.
///
/// Ported from oh-my-pi `resolveProviderCandidates`. The order is the built-in
/// [`SEARCH_PROVIDER_ORDER`]; a future config surface can reprioritize it.
pub fn resolve_provider_candidates(excluded: &[SearchProviderId]) -> Vec<ProviderCandidate> {
    SEARCH_PROVIDER_ORDER
        .into_iter()
        .filter(|id| !excluded.contains(id))
        .map(|id| ProviderCandidate {
            id,
            explicit: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recency_parses_the_four_words_and_rejects_the_rest() {
        assert_eq!(Recency::parse("day"), Ok(Recency::Day));
        assert_eq!(Recency::parse("year"), Ok(Recency::Year));
        assert!(Recency::parse("hour").is_err());
    }

    #[test]
    fn each_backend_gets_its_own_recency_word() {
        assert_eq!(Recency::Day.brave_freshness(), "pd");
        assert_eq!(Recency::Month.as_word(), "month");
    }

    #[test]
    fn the_chain_walks_the_full_order_by_default() {
        let candidates = resolve_provider_candidates(&[]);
        assert_eq!(candidates.len(), 23);
        assert_eq!(candidates[0].id, SearchProviderId::Perplexity);
        assert!(candidates.iter().all(|c| !c.explicit));
    }

    #[test]
    fn an_excluded_provider_drops_out_of_the_chain() {
        let candidates = resolve_provider_candidates(&[SearchProviderId::Perplexity]);
        assert_eq!(candidates.len(), 22);
        assert_eq!(candidates[0].id, SearchProviderId::Gemini);
    }

    #[test]
    fn implemented_providers_keep_their_chain_order() {
        // A provider must resolve at its own slot; a mismatch means the wrong
        // backend runs. Update this list as providers land.
        let implemented: Vec<_> = SEARCH_PROVIDER_ORDER
            .into_iter()
            .filter(|id| provider_for(*id).is_some())
            .collect();
        assert_eq!(
            implemented,
            vec![
                SearchProviderId::Gemini,
                SearchProviderId::Anthropic,
                SearchProviderId::Exa,
                SearchProviderId::Tinyfish,
                SearchProviderId::Jina,
                SearchProviderId::Kagi,
                SearchProviderId::Tavily,
                SearchProviderId::Firecrawl,
                SearchProviderId::Brave,
                SearchProviderId::Parallel,
                SearchProviderId::Synthetic,
                SearchProviderId::Searxng,
                SearchProviderId::Duckduckgo,
            ]
        );
    }
}
