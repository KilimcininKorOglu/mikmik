// Parallel provider: the Parallel Search API (POST /v1beta/search).
//
// Available when `PARALLEL_API_KEY` is set. The query is a natural-language
// objective; `site:`/`-site:`/`after:` and the recency window map onto Parallel's
// `source_policy` (include/exclude domains + freshness floor). Per-result
// excerpts join into the snippet.

use crate::web::search::provider::{Recency, SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax, StructuredQuery};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::{json, Value};

pub struct ParallelProvider;

const PARALLEL_SEARCH_URL: &str = "https://api.parallel.ai/v1beta/search";
const PARALLEL_BETA_HEADER: &str = "search-extract-2025-10-10";

fn api_key() -> Option<String> {
    std::env::var("PARALLEL_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
}

fn recency_days(recency: Recency) -> i64 {
    match recency {
        Recency::Day => 1,
        Recency::Week => 7,
        Recency::Month => 30,
        Recency::Year => 365,
    }
}

/// Bare hosts from `site:` values, deduped.
fn hosts(sites: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    sites
        .iter()
        .filter_map(|s| s.split('/').next())
        .filter(|h| !h.is_empty())
        .filter(|h| seen.insert(h.to_string()))
        .map(str::to_string)
        .collect()
}

/// Map directives and recency onto Parallel's `source_policy`, or `None`.
///
/// An explicit `after:` wins over the relative recency window. Parallel ignores
/// `exclude_domains` when `include_domains` is set, so exclusions are only sent
/// without an allow list (the central filter enforces them regardless).
fn source_policy(parsed: &StructuredQuery, recency: Option<Recency>) -> Option<Value> {
    let include = hosts(&parsed.sites);
    let exclude = hosts(&parsed.excluded_sites);
    let after_date = parsed.after.clone().or_else(|| {
        recency.map(|r| {
            (Utc::now().date_naive() - Duration::days(recency_days(r)))
                .format("%Y-%m-%d")
                .to_string()
        })
    });

    let mut policy = json!({});
    if !include.is_empty() {
        policy["include_domains"] = json!(include);
    } else if !exclude.is_empty() {
        policy["exclude_domains"] = json!(exclude);
    }
    if let Some(after) = after_date {
        policy["after_date"] = json!(after);
    }
    if policy.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return None;
    }
    Some(policy)
}

fn build_body(query: &str, policy: Option<Value>) -> Value {
    let mut body = json!({
        "objective": query,
        "search_queries": [query],
        "mode": "fast",
        "excerpts": { "max_chars_per_result": 10_000 },
    });
    if let Some(policy) = policy {
        body["source_policy"] = policy;
    }
    body
}

fn parse_sources(payload: &Value, max: usize) -> Vec<SearchSource> {
    let Some(items) = payload.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            if url.is_empty() {
                return None;
            }
            let excerpts: Vec<&str> = item
                .get("excerpts")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let snippet = (!excerpts.is_empty()).then(|| excerpts.join("\n\n"));
            Some(SearchSource {
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(url)
                    .to_string(),
                url: url.to_string(),
                snippet,
                published_date: item
                    .get("publish_date")
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
impl SearchProvider for ParallelProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Parallel
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key()
            .ok_or_else(|| SearchProviderError::new(self.id(), "PARALLEL_API_KEY is not set"))?;
        let query = if params.parsed.has_directives {
            format_query(
                params.parsed,
                QuerySyntax {
                    phrases: true,
                    negation: true,
                    or: true,
                    ..QuerySyntax::default()
                },
            )
        } else {
            params.query.to_string()
        };
        let policy = source_policy(params.parsed, params.recency);
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;

        let resp = client
            .post(PARALLEL_SEARCH_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("x-api-key", key)
            .header("parallel-beta", PARALLEL_BETA_HEADER)
            .json(&build_body(&query, policy))
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Parallel API returned status {status}"),
                status,
            ));
        }

        let payload: Value = resp.json().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Failed to parse response: {e}"))
        })?;

        let mut response = SearchResponse::empty(self.id().as_str());
        response.sources = parse_sources(&payload, params.limit);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::query::parse_search_query;
    use serde_json::json;

    #[test]
    fn include_domains_win_over_exclude_and_after_carries() {
        let parsed = parse_search_query("x site:arxiv.org -site:spam.com after:2024-01-01");
        let policy = source_policy(&parsed, None).expect("policy");
        assert_eq!(policy["include_domains"], json!(["arxiv.org"]));
        assert!(policy.get("exclude_domains").is_none());
        assert_eq!(policy["after_date"], json!("2024-01-01"));
    }

    #[test]
    fn exclude_only_when_no_include_and_recency_fills_after() {
        let parsed = parse_search_query("x -site:spam.com");
        let policy = source_policy(&parsed, Some(Recency::Week)).expect("policy");
        assert_eq!(policy["exclude_domains"], json!(["spam.com"]));
        assert!(policy["after_date"].is_string());

        assert!(source_policy(&parse_search_query("plain"), None).is_none());
    }

    #[test]
    fn excerpts_join_into_the_snippet() {
        let payload = json!({ "results": [
            { "url": "https://a", "title": "A", "excerpts": ["one", "two"], "publish_date": "2024-05-01" },
            { "title": "no url" }
        ]});
        let sources = parse_sources(&payload, 20);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].snippet.as_deref(), Some("one\n\ntwo"));
        assert_eq!(sources[0].published_date.as_deref(), Some("2024-05-01"));
    }

    #[test]
    fn the_body_carries_objective_mode_and_policy() {
        let body = build_body("rust", Some(json!({ "after_date": "2024-01-01" })));
        assert_eq!(body["objective"], json!("rust"));
        assert_eq!(body["mode"], json!("fast"));
        assert_eq!(body["search_queries"], json!(["rust"]));
        assert_eq!(body["source_policy"]["after_date"], json!("2024-01-01"));
        assert!(build_body("rust", None).get("source_policy").is_none());
    }
}
