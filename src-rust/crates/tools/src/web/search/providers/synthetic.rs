// Synthetic provider: the zero-data-retention search API for coding agents.
//
// Available when `SYNTHETIC_API_KEY` is set. POST /v2/search with the objective
// in the body; `site:` directives re-emit as query text.

use crate::web::search::provider::{SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct SyntheticProvider;

const SYNTHETIC_SEARCH_URL: &str = "https://api.synthetic.new/v2/search";

fn api_key() -> Option<String> {
    std::env::var("SYNTHETIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
}

/// The query to send: directives re-emitted with site syntax, else verbatim.
fn plan_query(params: &SearchParams<'_>) -> String {
    if !params.parsed.has_directives {
        return params.query.to_string();
    }
    let syntax = QuerySyntax {
        phrases: true,
        negation: true,
        site: true,
        ..QuerySyntax::default()
    };
    format_query(params.parsed, syntax)
}

fn parse_results(data: &Value, max: usize) -> Vec<SearchSource> {
    let Some(items) = data.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            if url.is_empty() {
                return None;
            }
            Some(SearchSource {
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(url)
                    .to_string(),
                url: url.to_string(),
                snippet: item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                published_date: item
                    .get("published")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                ..Default::default()
            })
        })
        .take(max)
        .collect()
}

#[async_trait]
impl SearchProvider for SyntheticProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Synthetic
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key()
            .ok_or_else(|| SearchProviderError::new(self.id(), "SYNTHETIC_API_KEY is not set"))?;
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;

        let resp = client
            .post(SYNTHETIC_SEARCH_URL)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {key}"))
            .json(&json!({ "query": plan_query(params) }))
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Synthetic API returned status {status}"),
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
    fn results_parse_with_snippet_and_date() {
        let data = json!({ "results": [
            { "url": "https://a", "title": "A", "text": "one", "published": "2024-01-01" },
            { "url": "https://b", "title": "B" }
        ]});
        let sources = parse_results(&data, 20);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].snippet.as_deref(), Some("one"));
        assert_eq!(sources[0].published_date.as_deref(), Some("2024-01-01"));
        assert!(sources[1].snippet.is_none());
    }

    #[test]
    fn missing_url_is_skipped_and_the_cap_applies() {
        let data = json!({ "results": [
            { "title": "no url" },
            { "url": "https://a" },
            { "url": "https://b" }
        ]});
        assert_eq!(parse_results(&data, 1).len(), 1);
        assert!(parse_results(&json!({}), 20).is_empty());
    }
}
