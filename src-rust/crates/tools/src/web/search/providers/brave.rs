// Brave Search API provider.
//
// Available when `BRAVE_SEARCH_API_KEY` is set.

use super::urlencode;
use crate::web::search::provider::{Recency, SearchParams, SearchProvider};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::Value;

pub struct BraveProvider;

fn api_key() -> Option<String> {
    super::stored_or_env_key(SearchProviderId::Brave, "BRAVE_SEARCH_API_KEY")
}

/// The Brave search URL, carrying `freshness` when a window was asked for.
fn brave_url(query: &str, limit: usize, recency: Option<Recency>) -> String {
    let freshness = recency
        .map(|r| format!("&freshness={}", r.brave_freshness()))
        .unwrap_or_default();
    format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={limit}{freshness}",
        urlencode(query)
    )
}

fn parse_results(data: &Value, max: usize) -> Vec<SearchSource> {
    let Some(items) = data
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
    else {
        return Vec::new();
    };
    items
        .iter()
        .take(max)
        .map(|item| SearchSource {
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
            snippet: item
                .get("description")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            ..Default::default()
        })
        .collect()
}

#[async_trait]
impl SearchProvider for BraveProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Brave
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key().ok_or_else(|| {
            SearchProviderError::new(self.id(), "BRAVE_SEARCH_API_KEY is not set")
        })?;
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;
        let url = brave_url(params.query, params.limit, params.recency);

        let resp = client
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", key)
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Brave Search API returned status {status}"),
                status,
            ));
        }

        let data: Value = resp.json().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Failed to parse response: {e}"))
        })?;

        let mut response = SearchResponse::empty(self.id().as_str());
        response.sources = parse_results(&data, params.limit);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_recency_reaches_the_url() {
        let url = brave_url("rust", 5, Some(Recency::Day));
        assert!(url.contains("&freshness=pd"), "{url}");
        let plain = brave_url("rust", 5, None);
        assert!(!plain.contains("freshness"), "{plain}");
    }

    #[test]
    fn results_parse_from_the_web_bucket() {
        let body = json!({ "web": { "results": [
            { "title": "Ownership", "url": "https://x", "description": "rules" }
        ]}});
        let sources = parse_results(&body, 20);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].title, "Ownership");
        assert_eq!(sources[0].snippet.as_deref(), Some("rules"));
        assert!(parse_results(&json!({}), 20).is_empty());
    }
}
