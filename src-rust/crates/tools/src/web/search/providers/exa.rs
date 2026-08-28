// Exa provider: high-quality neural search via the Exa Search API.
//
// Available when `EXA_API_KEY` is set. Requests per-result summaries and
// synthesizes the top few into a combined `answer`. Directives map onto Exa's
// native request parameters (includeDomains/excludeDomains/start+endPublishedDate).
//
// NOTE: oh-my-pi also ships a keyless public-MCP fallback for explicit selection;
// that path (JSON-RPC over SSE) is not ported here, so Exa is env-key gated.

use crate::web::search::provider::{SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax, StructuredQuery};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};

pub struct ExaProvider;

const EXA_API_URL: &str = "https://api.exa.ai/search";
const MAX_EXA_SNIPPET_CHARS: usize = 500;
const MAX_ANSWER_SUMMARIES: usize = 3;

fn api_key() -> Option<String> {
    std::env::var("EXA_API_KEY").ok().filter(|k| !k.is_empty())
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

/// The Exa request body derived from the parsed query.
fn build_body(params: &SearchParams<'_>) -> Value {
    let parsed = params.parsed;
    let query = if parsed.has_directives {
        format_query(
            parsed,
            QuerySyntax {
                phrases: true,
                ..QuerySyntax::default()
            },
        )
    } else {
        parsed.raw.clone()
    };
    let mut body = json!({
        "query": query,
        "numResults": params.limit,
        "type": "auto",
        "contents": { "summary": { "query": query } },
    });
    if parsed.has_directives {
        apply_directives(&mut body, parsed);
    }
    body
}

fn apply_directives(body: &mut Value, parsed: &StructuredQuery) {
    let include = hosts(&parsed.sites);
    if !include.is_empty() {
        body["includeDomains"] = json!(include);
    }
    let exclude = hosts(&parsed.excluded_sites);
    if !exclude.is_empty() {
        body["excludeDomains"] = json!(exclude);
    }
    if let Some(after) = &parsed.after {
        body["startPublishedDate"] = json!(after);
    }
    if let Some(before) = &parsed.before {
        body["endPublishedDate"] = json!(before);
    }
}

/// Age in seconds from an ISO published date, or `None`.
fn date_to_age_seconds(date: Option<&str>) -> Option<f64> {
    let date = date?;
    let ms = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date) {
        dt.timestamp_millis()
    } else {
        let d = chrono::NaiveDate::parse_from_str(date.get(..10)?, "%Y-%m-%d").ok()?;
        d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis()
    };
    let age = (Utc::now().timestamp_millis() - ms) as f64 / 1000.0;
    (age >= 0.0).then_some(age)
}

fn result_snippet(item: &Value) -> Option<String> {
    let text = item
        .get("summary")
        .and_then(Value::as_str)
        .or_else(|| item.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| {
            item.get("highlights").and_then(Value::as_array).map(|hs| {
                hs.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
        .filter(|s| !s.is_empty())?;
    Some(text.chars().take(MAX_EXA_SNIPPET_CHARS).collect())
}

fn parse_sources(results: &[Value], max: usize) -> Vec<SearchSource> {
    results
        .iter()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            if url.is_empty() {
                return None;
            }
            let published = item
                .get("publishedDate")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            Some(SearchSource {
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(url)
                    .to_string(),
                url: url.to_string(),
                snippet: result_snippet(item),
                published_date: published.map(str::to_string),
                age_seconds: date_to_age_seconds(published),
                author: item
                    .get("author")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            })
        })
        .take(max)
        .collect()
}

/// Synthesize an answer from the top per-result summaries.
fn synthesize_answer(results: &[Value]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for r in results {
        if parts.len() >= MAX_ANSWER_SUMMARIES {
            break;
        }
        if r.get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            continue;
        }
        let summary = r
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(summary) = summary else {
            continue;
        };
        let title = r
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| r.get("url").and_then(Value::as_str))
            .unwrap_or("Untitled");
        parts.push(format!("**{title}**: {summary}"));
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

#[async_trait]
impl SearchProvider for ExaProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Exa
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key()
            .ok_or_else(|| SearchProviderError::new(self.id(), "EXA_API_KEY is not set"))?;
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;

        let resp = client
            .post(EXA_API_URL)
            .header("Content-Type", "application/json")
            .header("x-api-key", key)
            .json(&build_body(params))
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Exa API returned status {status}"),
                status,
            ));
        }

        let data: Value = resp.json().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Failed to parse response: {e}"))
        })?;
        let results = data
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut response = SearchResponse::empty(self.id().as_str());
        response.answer = synthesize_answer(&results);
        response.sources = parse_sources(&results, params.limit);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hosts_dedupe_and_strip_paths() {
        assert_eq!(
            hosts(&["github.com/a".into(), "github.com/b".into(), "x.io".into()]),
            vec!["github.com", "x.io"]
        );
    }

    #[test]
    fn sources_prefer_summary_then_text_and_cap_the_snippet() {
        let results = vec![
            json!({ "url": "https://a", "title": "A", "summary": "sum", "text": "txt" }),
            json!({ "url": "https://b", "text": "only text" }),
            json!({ "title": "no url" }),
        ];
        let sources = parse_sources(&results, 20);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].snippet.as_deref(), Some("sum"));
        assert_eq!(sources[1].snippet.as_deref(), Some("only text"));

        let long = json!({ "url": "https://c", "summary": "x".repeat(600) });
        let capped = parse_sources(std::slice::from_ref(&long), 20);
        assert_eq!(
            capped[0].snippet.as_ref().unwrap().chars().count(),
            MAX_EXA_SNIPPET_CHARS
        );
    }

    #[test]
    fn the_answer_synthesizes_the_top_summaries() {
        let results = vec![
            json!({ "url": "https://a", "title": "A", "summary": "one" }),
            json!({ "url": "https://b", "title": "B", "summary": "two" }),
            json!({ "url": "https://c", "summary": "" }),
        ];
        let answer = synthesize_answer(&results).expect("answer");
        assert!(answer.contains("**A**: one"));
        assert!(answer.contains("**B**: two"));

        assert!(synthesize_answer(&[json!({ "url": "https://a" })]).is_none());
    }

    #[test]
    fn directives_map_onto_native_parameters() {
        let parsed = crate::web::search::query::parse_search_query(
            "neural nets site:arxiv.org -site:spam.com after:2024-01-01",
        );
        let mut body = json!({});
        apply_directives(&mut body, &parsed);
        assert_eq!(body["includeDomains"], json!(["arxiv.org"]));
        assert_eq!(body["excludeDomains"], json!(["spam.com"]));
        assert_eq!(body["startPublishedDate"], json!("2024-01-01"));
    }
}
