// Firecrawl provider: a SERP-backed search API with an optional self-hosted base.
//
// Available when `FIRECRAWL_API_KEY` or a `FIRECRAWL_BASE_URL`/`FIRECRAWL_API_URL`
// is set. The query supports Google operators; recency maps to `tbs=qdr:*` and
// absolute date bounds to a `tbs=cdr` custom range. Runs keyless when no key is
// present (Authorization header omitted).

use crate::web::search::provider::{Recency, SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax, StructuredQuery};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct FirecrawlProvider;

const FIRECRAWL_DEFAULT_BASE_URL: &str = "https://api.firecrawl.dev/v2";

fn api_key() -> Option<String> {
    super::stored_or_env_key(SearchProviderId::Firecrawl, "FIRECRAWL_API_KEY")
}

fn configured_base() -> Option<String> {
    std::env::var("FIRECRAWL_BASE_URL")
        .ok()
        .or_else(|| std::env::var("FIRECRAWL_API_URL").ok())
        .filter(|v| !v.trim().is_empty())
}

fn recency_tbs(recency: Recency) -> &'static str {
    match recency {
        Recency::Day => "qdr:d",
        Recency::Week => "qdr:w",
        Recency::Month => "qdr:m",
        Recency::Year => "qdr:y",
    }
}

/// The `/search` endpoint URL, honouring a configured base.
fn resolve_search_url() -> Result<String, SearchProviderError> {
    let Some(configured) = configured_base() else {
        return Ok(format!("{FIRECRAWL_DEFAULT_BASE_URL}/search"));
    };
    let mut url = url::Url::parse(configured.trim()).map_err(|_| {
        SearchProviderError::new(SearchProviderId::Firecrawl, "Invalid Firecrawl base URL")
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SearchProviderError::new(
            SearchProviderId::Firecrawl,
            "Invalid Firecrawl base URL: expected an HTTP or HTTPS URL",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SearchProviderError::new(
            SearchProviderId::Firecrawl,
            "Invalid Firecrawl base URL: URL credentials are not allowed",
        ));
    }
    url.set_query(None);
    url.set_fragment(None);
    let mut path = url.path().trim_end_matches('/').to_string();
    let has_version = path
        .rsplit('/')
        .next()
        .is_some_and(|seg| matches!(seg.to_lowercase().as_str(), "v1" | "v2"));
    if !has_version {
        path.push_str("/v2");
    }
    path.push_str("/search");
    url.set_path(&path);
    Ok(url.to_string())
}

/// ISO `YYYY-MM-DD` to Google `MM/DD/YYYY`.
fn to_google_date(iso: &str) -> String {
    let parts: Vec<&str> = iso.split('-').collect();
    if parts.len() == 3 {
        format!("{}/{}/{}", parts[1], parts[2], parts[0])
    } else {
        iso.to_string()
    }
}

/// A `tbs` custom date range from absolute `before:`/`after:` bounds.
fn build_date_tbs(parsed: &StructuredQuery) -> Option<String> {
    if parsed.after.is_none() && parsed.before.is_none() {
        return None;
    }
    let mut parts = vec!["cdr:1".to_string()];
    if let Some(after) = &parsed.after {
        parts.push(format!("cd_min:{}", to_google_date(after)));
    }
    if let Some(before) = &parsed.before {
        parts.push(format!("cd_max:{}", to_google_date(before)));
    }
    Some(parts.join(","))
}

/// The query to send and the `tbs` value (custom range wins over recency).
fn plan(params: &SearchParams<'_>) -> (String, Option<String>) {
    let parsed = params.parsed;
    if !parsed.has_directives {
        let tbs = params.recency.map(|r| recency_tbs(r).to_string());
        return (params.query.to_string(), tbs);
    }
    let date_tbs = build_date_tbs(parsed);
    let syntax = QuerySyntax {
        date_range: date_tbs.is_none(),
        ..QuerySyntax::google()
    };
    let query = format_query(parsed, syntax);
    let tbs = date_tbs.or_else(|| params.recency.map(|r| recency_tbs(r).to_string()));
    (query, tbs)
}

fn build_body(query: &str, limit: usize, tbs: Option<&str>) -> Value {
    let mut body = json!({
        "query": query,
        "limit": limit,
        "sources": [{ "type": "web" }],
    });
    if let Some(tbs) = tbs {
        body["tbs"] = json!(tbs);
    }
    body
}

/// Pull the web-result array from the several shapes Firecrawl returns.
fn web_results(data: &Value) -> Vec<Value> {
    if let Some(arr) = data.get("data").and_then(Value::as_array) {
        return arr.clone();
    }
    if let Some(arr) = data
        .get("data")
        .and_then(|d| d.get("web"))
        .and_then(Value::as_array)
    {
        return arr.clone();
    }
    data.get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn parse_sources(data: &Value, max: usize) -> Vec<SearchSource> {
    web_results(data)
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            if url.is_empty() {
                return None;
            }
            let snippet = ["description", "snippet", "markdown"]
                .iter()
                .find_map(|k| item.get(*k).and_then(Value::as_str))
                .filter(|s| !s.is_empty())
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
        .collect()
}

#[async_trait]
impl SearchProvider for FirecrawlProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Firecrawl
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some() || configured_base().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let url = resolve_search_url()?;
        let (query, tbs) = plan(params);
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;

        let mut req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&build_body(&query, params.limit, tbs.as_deref()));
        if let Some(key) = api_key() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        let resp = req.send().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
        })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Firecrawl API returned status {status}"),
                status,
            ));
        }

        let data: Value = resp.json().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Failed to parse response: {e}"))
        })?;
        if data.get("success").and_then(Value::as_bool) == Some(false) {
            let msg = data
                .get("error")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Firecrawl request failed");
            return Err(SearchProviderError::new(self.id(), msg));
        }

        let mut response = SearchResponse::empty(self.id().as_str());
        response.sources = parse_sources(&data, params.limit);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::query::parse_search_query;
    use serde_json::json;

    #[test]
    fn recency_and_date_bounds_map_to_tbs() {
        assert_eq!(recency_tbs(Recency::Week), "qdr:w");
        assert_eq!(to_google_date("2024-06-30"), "06/30/2024");
        let parsed = parse_search_query("x after:2024-01-01 before:2024-06-30");
        assert_eq!(
            build_date_tbs(&parsed).as_deref(),
            Some("cdr:1,cd_min:01/01/2024,cd_max:06/30/2024")
        );
        assert!(build_date_tbs(&parse_search_query("x")).is_none());
    }

    #[test]
    fn results_parse_from_every_shape() {
        let flat = json!({ "data": [ { "url": "https://a", "title": "A", "description": "d" } ] });
        assert_eq!(parse_sources(&flat, 20).len(), 1);
        let nested = json!({ "data": { "web": [ { "url": "https://b", "snippet": "s" } ] } });
        let sources = parse_sources(&nested, 20);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].snippet.as_deref(), Some("s"));
        let legacy = json!({ "results": [ { "url": "https://c", "markdown": "m" } ] });
        assert_eq!(parse_sources(&legacy, 20)[0].snippet.as_deref(), Some("m"));
    }

    #[test]
    fn the_body_carries_query_limit_and_tbs() {
        let body = build_body("rust", 8, Some("qdr:w"));
        assert_eq!(body["query"], json!("rust"));
        assert_eq!(body["limit"], json!(8));
        assert_eq!(body["tbs"], json!("qdr:w"));
        assert_eq!(body["sources"], json!([{ "type": "web" }]));
        assert!(build_body("rust", 8, None).get("tbs").is_none());
    }
}
