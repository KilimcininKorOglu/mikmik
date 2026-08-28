// SearXNG provider: queries a self-hosted SearXNG instance's JSON API.
//
// Only available when the user named an instance (settings or `SEARXNG_URL`);
// no address is ever guessed, because whatever answers that port would receive
// the user's query.

use super::urlencode;
use crate::web::search::provider::{Recency, SearchParams, SearchProvider};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::Value;

pub struct SearxngProvider;

/// The SearXNG instance to query, or `None` when the user named none.
///
/// `settings.json` wins over the environment, matching how `config.api_key`
/// outranks `ANTHROPIC_API_KEY`.
pub fn searxng_base_url(configured: Option<&str>) -> Option<String> {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("SEARXNG_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

/// The SearXNG search URL, carrying `time_range` when a window was asked for.
fn searxng_url(base: &str, query: &str, recency: Option<Recency>) -> String {
    let time_range = recency
        .map(|r| format!("&time_range={}", r.as_word()))
        .unwrap_or_default();
    format!(
        "{}/search?q={}&format=json&safesearch=0{time_range}",
        base.trim_end_matches('/'),
        urlencode(query)
    )
}

/// Parse a SearXNG JSON body into sources.
fn parse_results(data: &Value, max: usize) -> Vec<SearchSource> {
    let Some(items) = data.get("results").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .take(max)
        .map(|item| {
            let snippet = item
                .get("content")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            SearchSource {
                title: item
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("(No title)")
                    .to_string(),
                url: item
                    .get("url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet,
                ..Default::default()
            }
        })
        .collect()
}

#[async_trait]
impl SearchProvider for SearxngProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Searxng
    }

    async fn is_available(&self, ctx: &ToolContext) -> bool {
        searxng_base_url(ctx.config.searxng_url.as_deref()).is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let base = searxng_base_url(params.ctx.config.searxng_url.as_deref())
            .ok_or_else(|| SearchProviderError::new(self.id(), "No SearXNG instance configured"))?;
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;
        let url = searxng_url(&base, params.query, params.recency);

        let resp = client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("SearXNG request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!(
                    "SearXNG returned status {status} (is JSON format enabled in settings.yml?)"
                ),
                status,
            ));
        }

        let data: Value = resp.json().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Failed to parse SearXNG response: {e}"))
        })?;

        let mut response = SearchResponse::empty(self.id().as_str());
        response.sources = parse_results(&data, params.limit);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        serde_json::json!({
            "query": "rust ownership",
            "results": [
                { "title": "Ownership", "url": "https://doc.rust-lang.org/book/ch04-01.html", "content": "Ownership is a set of rules." },
                { "title": "Borrowing", "url": "https://doc.rust-lang.org/book/ch04-02.html", "content": "References." }
            ]
        })
    }

    #[test]
    fn results_parse_into_sources() {
        let sources = parse_results(&body(), 20);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].title, "Ownership");
        assert_eq!(
            sources[0].url,
            "https://doc.rust-lang.org/book/ch04-01.html"
        );
        assert_eq!(
            sources[0].snippet.as_deref(),
            Some("Ownership is a set of rules.")
        );
    }

    #[test]
    fn parsing_honours_the_result_cap_and_empty_case() {
        assert_eq!(parse_results(&body(), 1).len(), 1);
        let empty = serde_json::json!({ "results": [] });
        assert!(parse_results(&empty, 20).is_empty());
        let absent = serde_json::json!({ "query": "x" });
        assert!(parse_results(&absent, 20).is_empty());
    }

    #[test]
    fn a_recency_reaches_the_url() {
        let url = searxng_url("http://searx.example", "rust", Some(Recency::Week));
        assert!(url.contains("&time_range=week"), "{url}");
        let plain = searxng_url("http://searx.example", "rust", None);
        assert!(!plain.contains("time_range"), "{plain}");
    }
}
