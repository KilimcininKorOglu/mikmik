// DuckDuckGo provider: the Instant Answer API.
//
// Keyless, so it is always available and sits at the end of the chain as a
// best-effort fallback. The Instant Answer API returns an abstract plus related
// topics rather than a full result set, and has no recency parameter, so a
// requested window is silently ignored (per the provider contract).

use super::urlencode;
use crate::web::search::provider::{SearchParams, SearchProvider};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::Value;

pub struct DuckDuckGoProvider;

fn parse_results(data: &Value, max: usize) -> Vec<SearchSource> {
    let mut sources: Vec<SearchSource> = Vec::new();

    if let Some(abstract_text) = data.get("Abstract").and_then(|a| a.as_str()) {
        if !abstract_text.is_empty() {
            sources.push(SearchSource {
                title: data
                    .get("AbstractSource")
                    .and_then(|s| s.as_str())
                    .unwrap_or("Abstract")
                    .to_string(),
                url: data
                    .get("AbstractURL")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet: Some(abstract_text.to_string()),
                ..Default::default()
            });
        }
    }

    if let Some(topics) = data.get("RelatedTopics").and_then(|t| t.as_array()) {
        for topic in topics {
            if sources.len() >= max {
                break;
            }
            let Some(text) = topic.get("Text").and_then(|t| t.as_str()) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let url = topic
                .get("FirstURL")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            // The first sentence-ish fragment reads as a title.
            let title = text.split(" - ").next().unwrap_or(text).to_string();
            sources.push(SearchSource {
                title,
                url,
                snippet: Some(text.to_string()),
                ..Default::default()
            });
        }
    }

    sources.truncate(max);
    sources
}

#[async_trait]
impl SearchProvider for DuckDuckGoProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Duckduckgo
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        true
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;
        let url = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
            urlencode(params.query)
        );

        let resp = client
            .get(&url)
            .header("User-Agent", "MikMik/1.0")
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("DuckDuckGo API returned status {status}"),
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
    fn abstract_and_related_topics_become_sources() {
        let body = json!({
            "Abstract": "Rust is a systems language.",
            "AbstractSource": "Wikipedia",
            "AbstractURL": "https://en.wikipedia.org/wiki/Rust",
            "RelatedTopics": [
                { "Text": "Ownership - a memory model", "FirstURL": "https://x/ownership" },
                { "Text": "", "FirstURL": "https://x/empty" }
            ]
        });
        let sources = parse_results(&body, 20);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].title, "Wikipedia");
        assert_eq!(sources[1].title, "Ownership");
        assert_eq!(sources[1].url, "https://x/ownership");
    }

    #[test]
    fn the_cap_bounds_the_source_count() {
        let body = json!({
            "Abstract": "a", "AbstractSource": "S", "AbstractURL": "https://s",
            "RelatedTopics": [
                { "Text": "one", "FirstURL": "https://1" },
                { "Text": "two", "FirstURL": "https://2" }
            ]
        });
        assert_eq!(parse_results(&body, 1).len(), 1);
        assert!(parse_results(&json!({}), 20).is_empty());
    }
}
