// Unified web-search types shared across providers.
//
// Ported from oh-my-pi `web/search/types.ts`. Every provider maps its native
// response onto `SearchResponse`, so the auto chain and the LLM-facing
// formatter never learn a provider's wire shape.

use serde::{Deserialize, Serialize};
use std::fmt;

/// One result returned by a search provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchSource {
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// ISO date string or a relative label such as `2d ago`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    /// Age in seconds, when the provider reports one, for consistent formatting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// A citation with an optional quoted span, from LLM-mediated providers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchCitation {
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cited_text: Option<String>,
}

/// Token/usage metrics, when a provider reports them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Anthropic: number of web-search requests made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_requests: Option<u64>,
    /// Perplexity: combined token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

/// Unified response across every provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResponse {
    /// Provider id, or `none` when no provider ran.
    pub provider: String,
    /// Synthesized answer text, from LLM-mediated providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    pub sources: Vec<SearchSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<SearchCitation>,
    /// Intermediate search queries the model issued (Anthropic).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_queries: Vec<String>,
    /// Follow-up question suggestions, provider-dependent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_questions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SearchUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl SearchResponse {
    /// An empty response attributed to `provider`.
    pub fn empty(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            sources: Vec::new(),
            ..Default::default()
        }
    }

    /// Whether the response carries anything worth rendering to the model.
    pub fn has_renderable_content(&self) -> bool {
        self.answer.as_ref().is_some_and(|a| !a.trim().is_empty())
            || !self.sources.is_empty()
            || !self.citations.is_empty()
            || self.related_questions.iter().any(|q| !q.trim().is_empty())
            || self.search_queries.iter().any(|q| !q.trim().is_empty())
    }
}

/// A provider failure, carrying the provider id and an optional HTTP status.
#[derive(Debug, Clone)]
pub struct SearchProviderError {
    pub provider: SearchProviderId,
    pub message: String,
    pub status: Option<u16>,
}

impl SearchProviderError {
    pub fn new(provider: SearchProviderId, message: impl Into<String>) -> Self {
        Self {
            provider,
            message: message.into(),
            status: None,
        }
    }

    pub fn with_status(
        provider: SearchProviderId,
        message: impl Into<String>,
        status: u16,
    ) -> Self {
        Self {
            provider,
            message: message.into(),
            status: Some(status),
        }
    }
}

impl fmt::Display for SearchProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SearchProviderError {}

/// A supported web-search provider.
///
/// Ordered exactly as oh-my-pi's `SEARCH_PROVIDER_OPTIONS` (minus `auto`), so
/// [`SEARCH_PROVIDER_ORDER`] and the settings/setup surface share one source
/// of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchProviderId {
    Perplexity,
    Gemini,
    Anthropic,
    Codex,
    Xai,
    Zai,
    Exa,
    Tinyfish,
    Jina,
    Kagi,
    Tavily,
    Firecrawl,
    Brave,
    Kimi,
    Parallel,
    Synthetic,
    Searxng,
    Startpage,
    Duckduckgo,
    Ecosia,
    Google,
    Mojeek,
    Public,
}

/// Auto-resolution priority order (every provider except the `auto` sentinel).
pub const SEARCH_PROVIDER_ORDER: [SearchProviderId; 23] = [
    SearchProviderId::Perplexity,
    SearchProviderId::Gemini,
    SearchProviderId::Anthropic,
    SearchProviderId::Codex,
    SearchProviderId::Xai,
    SearchProviderId::Zai,
    SearchProviderId::Exa,
    SearchProviderId::Tinyfish,
    SearchProviderId::Jina,
    SearchProviderId::Kagi,
    SearchProviderId::Tavily,
    SearchProviderId::Firecrawl,
    SearchProviderId::Brave,
    SearchProviderId::Kimi,
    SearchProviderId::Parallel,
    SearchProviderId::Synthetic,
    SearchProviderId::Searxng,
    SearchProviderId::Startpage,
    SearchProviderId::Duckduckgo,
    SearchProviderId::Ecosia,
    SearchProviderId::Google,
    SearchProviderId::Mojeek,
    SearchProviderId::Public,
];

impl SearchProviderId {
    /// The wire id used in settings and the `provider` field of a response.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Perplexity => "perplexity",
            Self::Gemini => "gemini",
            Self::Anthropic => "anthropic",
            Self::Codex => "codex",
            Self::Xai => "xai",
            Self::Zai => "zai",
            Self::Exa => "exa",
            Self::Tinyfish => "tinyfish",
            Self::Jina => "jina",
            Self::Kagi => "kagi",
            Self::Tavily => "tavily",
            Self::Firecrawl => "firecrawl",
            Self::Brave => "brave",
            Self::Kimi => "kimi",
            Self::Parallel => "parallel",
            Self::Synthetic => "synthetic",
            Self::Searxng => "searxng",
            Self::Startpage => "startpage",
            Self::Duckduckgo => "duckduckgo",
            Self::Ecosia => "ecosia",
            Self::Google => "google",
            Self::Mojeek => "mojeek",
            Self::Public => "public",
        }
    }

    /// The human-readable label shown in pickers and fallback notices.
    pub fn label(self) -> &'static str {
        match self {
            Self::Perplexity => "Perplexity",
            Self::Gemini => "Gemini",
            Self::Anthropic => "Anthropic",
            Self::Codex => "OpenAI",
            Self::Xai => "xAI",
            Self::Zai => "Z.AI",
            Self::Exa => "Exa",
            Self::Tinyfish => "TinyFish",
            Self::Jina => "Jina",
            Self::Kagi => "Kagi",
            Self::Tavily => "Tavily",
            Self::Firecrawl => "Firecrawl",
            Self::Brave => "Brave",
            Self::Kimi => "Kimi",
            Self::Parallel => "Parallel",
            Self::Synthetic => "Synthetic",
            Self::Searxng => "SearXNG",
            Self::Startpage => "Startpage",
            Self::Duckduckgo => "DuckDuckGo",
            Self::Ecosia => "Ecosia",
            Self::Google => "Google",
            Self::Mojeek => "Mojeek",
            Self::Public => "Public Web",
        }
    }

    /// Parse a wire id, or `None` when it names no provider.
    pub fn parse(value: &str) -> Option<Self> {
        SEARCH_PROVIDER_ORDER
            .into_iter()
            .find(|id| id.as_str() == value)
    }
}

impl fmt::Display for SearchProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default hard timeout for each provider transport.
pub const DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS: u64 = 60;

/// Maximum configurable hard timeout for each provider transport.
pub const MAX_WEB_SEARCH_TIMEOUT_SECONDS: u64 = 300;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_round_trips_through_its_wire_id() {
        for id in SEARCH_PROVIDER_ORDER {
            assert_eq!(SearchProviderId::parse(id.as_str()), Some(id));
        }
        assert_eq!(SearchProviderId::parse("nope"), None);
    }

    #[test]
    fn the_order_matches_omp_and_has_no_duplicates() {
        // A drift here desyncs the auto chain from the settings dropdown.
        assert_eq!(SEARCH_PROVIDER_ORDER.len(), 23);
        assert_eq!(SEARCH_PROVIDER_ORDER[0], SearchProviderId::Perplexity);
        assert_eq!(SEARCH_PROVIDER_ORDER[22], SearchProviderId::Public);
        let mut seen = std::collections::HashSet::new();
        for id in SEARCH_PROVIDER_ORDER {
            assert!(seen.insert(id.as_str()), "duplicate {id}");
        }
    }

    #[test]
    fn a_response_reports_whether_it_has_renderable_content() {
        let empty = SearchResponse::empty("none");
        assert!(!empty.has_renderable_content());

        let mut with_source = SearchResponse::empty("brave");
        with_source.sources.push(SearchSource {
            title: "t".into(),
            url: "https://x".into(),
            ..Default::default()
        });
        assert!(with_source.has_renderable_content());
    }
}
