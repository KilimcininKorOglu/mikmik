// Jina Reader provider: the `s.jina.ai` search endpoint.
//
// Available when `JINA_API_KEY` is set. Jina's index is Bing-backed and parses
// classic operators; a single `site:` include is sent through the `X-Site`
// header instead, and the rest of the directives are re-emitted as query text.

use crate::web::search::provider::{SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::Value;

pub struct JinaProvider;

const JINA_SEARCH_URL: &str = "https://s.jina.ai/";

fn api_key() -> Option<String> {
    super::stored_or_env_key(SearchProviderId::Jina, "JINA_API_KEY")
}

/// The query to send and the optional single-site host for `X-Site`.
fn plan(params: &SearchParams<'_>) -> (String, Option<String>) {
    let parsed = params.parsed;
    if !parsed.has_directives {
        return (params.query.to_string(), None);
    }
    // A single include site rides the X-Site header; multiple stay inline.
    let site = if parsed.sites.len() == 1 {
        parsed.sites[0].split('/').next().map(str::to_string)
    } else {
        None
    };
    let syntax = QuerySyntax {
        phrases: true,
        negation: true,
        site: site.is_none(),
        in_title: true,
        in_url: true,
        filetype: true,
        ..QuerySyntax::default()
    };
    (format_query(parsed, syntax), site)
}

/// Build the request URL with the query as a path segment.
fn build_url(query: &str, limit: usize) -> Result<url::Url, SearchProviderError> {
    let mut url = url::Url::parse(JINA_SEARCH_URL)
        .map_err(|e| SearchProviderError::new(SearchProviderId::Jina, format!("URL: {e}")))?;
    url.path_segments_mut()
        .map_err(|_| SearchProviderError::new(SearchProviderId::Jina, "URL cannot be a base"))?
        .push(query);
    url.query_pairs_mut()
        .append_pair("count", &limit.to_string());
    Ok(url)
}

/// Extract the result array from either a bare array or a `{ data: [...] }` envelope.
fn results_array(payload: &Value) -> Result<&Vec<Value>, SearchProviderError> {
    if let Some(arr) = payload.as_array() {
        return Ok(arr);
    }
    if let Some(code) = payload.get("code").and_then(Value::as_u64) {
        if code != 200 {
            return Err(SearchProviderError::new(
                SearchProviderId::Jina,
                format!("Jina API response reported failure ({code})"),
            ));
        }
    }
    payload
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SearchProviderError::new(SearchProviderId::Jina, "Jina API returned no data array")
        })
}

fn parse_results(payload: &Value, max: usize) -> Result<Vec<SearchSource>, SearchProviderError> {
    let items = results_array(payload)?;
    let sources = items
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            if url.is_empty() {
                return None;
            }
            let snippet = item
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    item.get("content")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
                .map(str::to_string);
            Some(SearchSource {
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(url)
                    .to_string(),
                url: url.to_string(),
                snippet,
                ..Default::default()
            })
        })
        .take(max)
        .collect();
    Ok(sources)
}

#[async_trait]
impl SearchProvider for JinaProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Jina
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key()
            .ok_or_else(|| SearchProviderError::new(self.id(), "JINA_API_KEY is not set"))?;
        let (query, site) = plan(params);
        let url = build_url(&query, params.limit)?;
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;

        let mut req = client
            .get(url)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {key}"))
            .header("X-Respond-With", "no-content")
            .header("X-Retain-Images", "none");
        if let Some(site) = site {
            req = req.header("X-Site", site);
        }

        let resp = req.send().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Jina request failed: {e}"))
        })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Jina API returned status {status}"),
                status,
            ));
        }

        let payload: Value = resp.json().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Failed to parse response: {e}"))
        })?;

        let mut response = SearchResponse::empty(self.id().as_str());
        response.sources = parse_results(&payload, params.limit)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_bare_array_and_a_data_envelope_both_parse() {
        let bare = json!([
            { "title": "A", "url": "https://a", "description": "one" },
            { "url": "https://b", "content": "two" }
        ]);
        let sources = parse_results(&bare, 20).expect("parse");
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].title, "A");
        // Missing title falls back to the URL.
        assert_eq!(sources[1].title, "https://b");
        assert_eq!(sources[1].snippet.as_deref(), Some("two"));

        let enveloped =
            json!({ "code": 200, "data": [ { "url": "https://c", "description": "d" } ] });
        assert_eq!(parse_results(&enveloped, 20).expect("parse").len(), 1);
    }

    #[test]
    fn a_failure_code_is_surfaced() {
        let payload = json!({ "code": 402, "data": [] });
        assert!(parse_results(&payload, 20).is_err());
    }

    #[test]
    fn results_without_a_url_are_skipped_and_the_cap_applies() {
        let payload = json!([
            { "title": "no url" },
            { "url": "https://a" },
            { "url": "https://b" }
        ]);
        let sources = parse_results(&payload, 1).expect("parse");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].url, "https://a");
    }

    #[test]
    fn the_url_carries_the_query_and_count() {
        let url = build_url("rust ownership", 7).expect("url");
        assert!(url.as_str().contains("s.jina.ai"));
        assert!(url.as_str().contains("count=7"));
        assert!(url.path().contains("rust%20ownership"));
    }
}
