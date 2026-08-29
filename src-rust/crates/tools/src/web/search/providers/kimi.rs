// Kimi provider: the Kimi Code search API.
//
// Ported from oh-my-pi `web/search/providers/kimi.ts`. A plain JSON search
// endpoint (`https://api.kimi.com/coding/v1/search`) that returns web results
// directly, so this reads like the Tavily provider. The credential is the Kimi
// Code Console key, stored under `kimi-code` or given through
// `MOONSHOT_SEARCH_API_KEY`/`KIMI_SEARCH_API_KEY`; a Moonshot Open Platform key
// is a different credential system and 401s here.

use crate::web::search::provider::{SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax, StructuredQuery};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct KimiProvider;

const DEFAULT_SEARCH_URL: &str = "https://api.kimi.com/coding/v1/search";
const KIMI_CODE_ID: &str = "kimi-code";
const CRAWL_TIMEOUT_SECONDS: u64 = 30;

/// Kimi Code search parses the Bing operator set; dates and language stay with
/// the central constraint filter.
const KIMI_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: false,
    site: true,
    in_url: true,
    in_title: true,
    in_text: false,
    filetype: true,
    date_range: false,
};

/// A non-empty environment variable, or `None`.
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The Kimi Code credential: the stored `kimi-code` key first (written by the
/// TUI key-entry flow), then the search-specific env override.
fn api_key() -> Option<String> {
    mikmik_core::AuthStore::load()
        .api_key_for(KIMI_CODE_ID)
        .filter(|k| !k.is_empty())
        .or_else(|| nonempty_env("MOONSHOT_SEARCH_API_KEY"))
        .or_else(|| nonempty_env("KIMI_SEARCH_API_KEY"))
}

/// The search endpoint: an env override, else the Kimi Code default.
fn search_url() -> String {
    nonempty_env("MOONSHOT_SEARCH_BASE_URL")
        .or_else(|| nonempty_env("KIMI_SEARCH_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_SEARCH_URL.to_string())
}

/// The grounded query. Directives re-emit as Bing operator syntax; a
/// directive-free query passes through byte-identical.
fn plan_query(raw: &str, parsed: &StructuredQuery) -> String {
    if parsed.has_directives {
        format_query(parsed, KIMI_SYNTAX)
    } else {
        raw.to_string()
    }
}

/// The request body: the query, the result cap, and page-crawling off.
fn request_body(query: &str, limit: usize) -> Value {
    json!({
        "text_query": query,
        "limit": limit,
        "enable_page_crawling": false,
        "timeout_seconds": CRAWL_TIMEOUT_SECONDS,
    })
}

/// A trimmed non-empty string field on a JSON object.
fn trimmed_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Age in seconds from an ISO date string, or `None` when it does not parse.
/// Mirrors omp's `dateToAgeSeconds`: `(now - date) / 1000`.
fn date_to_age_seconds(date: &str) -> Option<f64> {
    let millis = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date) {
        dt.timestamp_millis()
    } else {
        let day = chrono::NaiveDate::parse_from_str(date.get(..10)?, "%Y-%m-%d").ok()?;
        day.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis()
    };
    let now = chrono::Utc::now().timestamp_millis();
    Some(((now - millis) / 1000) as f64)
}

/// Map one Kimi result onto a source, or `None` when it carries no URL.
fn result_to_source(result: &Value) -> Option<SearchSource> {
    let url = trimmed_field(result, "url")?;
    let published_date = trimmed_field(result, "date");
    let age_seconds = published_date.as_deref().and_then(date_to_age_seconds);
    Some(SearchSource {
        title: trimmed_field(result, "title").unwrap_or_else(|| url.clone()),
        url,
        snippet: trimmed_field(result, "snippet").or_else(|| trimmed_field(result, "content")),
        published_date,
        age_seconds,
        author: trimmed_field(result, "site_name"),
    })
}

/// Parse the response body into capped sources.
fn parse_results(data: &Value, limit: usize) -> Vec<SearchSource> {
    let Some(items) = data.get("search_results").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(result_to_source)
        .take(limit)
        .collect()
}

#[async_trait]
impl SearchProvider for KimiProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Kimi
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key().ok_or_else(|| {
            SearchProviderError::new(
                self.id(),
                "Kimi search credentials not found. Set MOONSHOT_SEARCH_API_KEY or KIMI_SEARCH_API_KEY to a Kimi Code Console key, or configure a kimi-code account.",
            )
        })?;
        let query = plan_query(params.query, params.parsed);
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;
        let resp = client
            .post(search_url())
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("Authorization", format!("Bearer {key}"))
            .json(&request_body(&query, params.limit))
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Kimi search API returned status {status}"),
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
    use crate::web::search::query::parse_search_query;

    #[test]
    fn a_directive_free_query_passes_through() {
        let parsed = parse_search_query("rust web framework");
        assert_eq!(
            plan_query("rust web framework", &parsed),
            "rust web framework"
        );
    }

    #[test]
    fn directives_re_emit_as_bing_syntax_without_or() {
        let parsed = parse_search_query("axum site:docs.rs filetype:pdf");
        let planned = plan_query("axum site:docs.rs filetype:pdf", &parsed);
        assert!(planned.contains("site:docs.rs"));
        assert!(planned.contains("filetype:pdf"));
    }

    #[test]
    fn the_body_carries_the_query_cap_and_crawling_off() {
        let body = request_body("rust", 7);
        assert_eq!(body["text_query"], json!("rust"));
        assert_eq!(body["limit"], json!(7));
        assert_eq!(body["enable_page_crawling"], json!(false));
        assert_eq!(body["timeout_seconds"], json!(CRAWL_TIMEOUT_SECONDS));
    }

    #[test]
    fn results_parse_with_snippet_content_fallback_and_author() {
        let data = json!({ "search_results": [
            { "title": "Axum", "url": "https://a", "snippet": "web framework", "date": "2024-01-01", "site_name": "docs.rs" },
            { "title": "Tokio", "url": "https://b", "content": "async runtime" },
            { "title": "No URL", "snippet": "skipped" }
        ]});
        let sources = parse_results(&data, 20);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].title, "Axum");
        assert_eq!(sources[0].snippet.as_deref(), Some("web framework"));
        assert_eq!(sources[0].author.as_deref(), Some("docs.rs"));
        assert!(sources[0].age_seconds.is_some());
        // Snippet falls back to content when snippet is absent.
        assert_eq!(sources[1].snippet.as_deref(), Some("async runtime"));
        assert!(sources[1].author.is_none());
    }

    #[test]
    fn the_result_cap_bounds_the_source_count() {
        let data = json!({ "search_results": [
            { "title": "A", "url": "https://a" },
            { "title": "B", "url": "https://b" }
        ]});
        assert_eq!(parse_results(&data, 1).len(), 1);
        assert!(parse_results(&json!({ "search_results": [] }), 20).is_empty());
        assert!(parse_results(&json!({}), 20).is_empty());
    }

    #[test]
    fn a_missing_title_falls_back_to_the_url() {
        let data = json!({ "search_results": [ { "url": "https://only-url" } ] });
        let sources = parse_results(&data, 20);
        assert_eq!(sources[0].title, "https://only-url");
    }
}
