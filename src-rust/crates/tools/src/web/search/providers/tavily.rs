// Tavily provider: a dedicated search API returning full results.
//
// Available when `TAVILY_API_KEY` is set.

use crate::web::search::provider::{Recency, SearchParams, SearchProvider};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct TavilyProvider;

fn api_key() -> Option<String> {
    super::stored_or_env_key(SearchProviderId::Tavily, "TAVILY_API_KEY")
}

/// The Tavily request body, carrying `time_range` when a window was asked for.
fn request_body(query: &str, limit: usize, recency: Option<Recency>) -> Value {
    let mut body = json!({ "query": query, "max_results": limit });
    if let Some(recency) = recency {
        body["time_range"] = json!(recency.as_word());
    }
    body
}

fn parse_results(data: &Value, max: usize) -> Vec<SearchSource> {
    let Some(items) = data.get("results").and_then(|r| r.as_array()) else {
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
                .get("content")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            ..Default::default()
        })
        .collect()
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Tavily
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key()
            .ok_or_else(|| SearchProviderError::new(self.id(), "TAVILY_API_KEY is not set"))?;
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;
        let body = request_body(params.query, params.limit, params.recency);

        let resp = client
            .post("https://api.tavily.com/search")
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Tavily API returned status {status}"),
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

    #[test]
    fn a_body_carries_the_window_and_result_cap() {
        let windowed = request_body("rust", 7, Some(Recency::Month));
        assert_eq!(windowed["time_range"], json!("month"));
        assert_eq!(windowed["max_results"], json!(7));
        let plain = request_body("rust", 5, None);
        assert!(plain.get("time_range").is_none(), "{plain}");
    }

    #[test]
    fn results_parse_into_sources_with_the_cap() {
        let body = json!({ "results": [
            { "title": "First", "url": "https://a", "content": "one" },
            { "title": "Second", "url": "https://b", "content": "two" }
        ]});
        let sources = parse_results(&body, 1);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].title, "First");
        assert!(parse_results(&json!({ "results": [] }), 20).is_empty());
    }
}
