// Kagi provider: the Kagi V1 Search API (POST /api/v1/search).
//
// Available when `KAGI_API_KEY` is set (Bearer). V1 returns categorized result
// buckets (search/video/news/infobox) plus related questions and a direct
// answer; recency maps onto `filters.after` as a YYYY-MM-DD bound.

use crate::web::search::provider::{Recency, SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use chrono::{Duration, Months, Utc};
use serde_json::{json, Value};

pub struct KagiProvider;

const KAGI_SEARCH_URL: &str = "https://kagi.com/api/v1/search";

fn api_key() -> Option<String> {
    super::stored_or_env_key(SearchProviderId::Kagi, "KAGI_API_KEY")
}

/// A YYYY-MM-DD bound `recency` units before now, in UTC.
fn recency_to_date(recency: Recency) -> String {
    let today = Utc::now().date_naive();
    let date = match recency {
        Recency::Day => today - Duration::days(1),
        Recency::Week => today - Duration::days(7),
        Recency::Month => today.checked_sub_months(Months::new(1)).unwrap_or(today),
        Recency::Year => today.checked_sub_months(Months::new(12)).unwrap_or(today),
    };
    date.format("%Y-%m-%d").to_string()
}

fn build_body(query: &str, limit: usize, recency: Option<Recency>) -> Value {
    let mut body = json!({ "query": query, "workflow": "search", "limit": limit });
    if let Some(recency) = recency {
        body["filters"] = json!({ "after": recency_to_date(recency) });
    }
    body
}

fn first_non_empty<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| item.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Push every valid item in a bucket as a source, with an optional title tag.
fn collect_sources(sources: &mut Vec<SearchSource>, bucket: Option<&Value>, tag: Option<&str>) {
    let Some(items) = bucket.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let Some(url) = first_non_empty(item, &["url", "href", "link"]) else {
            continue;
        };
        let title = first_non_empty(item, &["title", "name"]).unwrap_or(url);
        let title = match tag {
            Some(tag) => format!("{tag} {title}"),
            None => title.to_string(),
        };
        sources.push(SearchSource {
            title,
            url: url.to_string(),
            snippet: first_non_empty(item, &["snippet", "description", "summary"])
                .map(str::to_string),
            published_date: first_non_empty(item, &["time"]).map(str::to_string),
            ..Default::default()
        });
    }
}

/// A related/adjacent question from an item's props, else its title.
fn question_of(item: &Value) -> Option<String> {
    if let Some(props) = item.get("props") {
        if let Some(q) = first_non_empty(props, &["question", "query"]) {
            return Some(q.to_string());
        }
    }
    first_non_empty(item, &["title"]).map(str::to_string)
}

fn collect_questions(out: &mut Vec<String>, bucket: Option<&Value>) {
    let Some(items) = bucket.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        if let Some(q) = question_of(item) {
            out.push(q);
        }
    }
}

/// Map the categorized `data` object onto a unified response.
fn parse_data(data: &Value) -> SearchResponse {
    let mut sources: Vec<SearchSource> = Vec::new();
    collect_sources(&mut sources, data.get("search"), None);
    collect_sources(&mut sources, data.get("video"), Some("[Video]"));
    collect_sources(&mut sources, data.get("news"), Some("[News]"));
    collect_sources(&mut sources, data.get("infobox"), Some("[Info]"));

    let mut related: Vec<String> = Vec::new();
    collect_questions(&mut related, data.get("adjacent_question"));
    collect_questions(&mut related, data.get("related_search"));

    let answer = data
        .get("direct_answer")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|d| first_non_empty(d, &["snippet", "title"]))
        .map(str::to_string);

    let mut response = SearchResponse::empty(SearchProviderId::Kagi.as_str());
    response.sources = sources;
    response.related_questions = related;
    response.answer = answer;
    response
}

#[async_trait]
impl SearchProvider for KagiProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Kagi
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key()
            .ok_or_else(|| SearchProviderError::new(self.id(), "KAGI_API_KEY is not set"))?;
        // Kagi's index parses the classic operator set; re-emit directives.
        let query = if params.parsed.has_directives {
            format_query(params.parsed, QuerySyntax::google())
        } else {
            params.query.to_string()
        };
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;

        let resp = client
            .post(KAGI_SEARCH_URL)
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&build_body(&query, params.limit, params.recency))
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("Kagi API returned status {status}"),
                status,
            ));
        }

        let payload: Value = resp.json().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Failed to parse response: {e}"))
        })?;
        let mut response = match payload.get("data") {
            Some(data) => parse_data(data),
            None => SearchResponse::empty(self.id().as_str()),
        };
        response.sources.truncate(params.limit);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn buckets_collect_with_tags_and_questions() {
        let data = json!({
            "search": [ { "url": "https://a", "title": "A", "snippet": "sa", "time": "2h ago" } ],
            "news": [ { "url": "https://n", "title": "N" } ],
            "adjacent_question": [ { "title": "Why?" } ],
            "direct_answer": [ { "snippet": "Because." } ]
        });
        let response = parse_data(&data);
        assert_eq!(response.sources.len(), 2);
        assert_eq!(response.sources[0].title, "A");
        assert_eq!(
            response.sources[0].published_date.as_deref(),
            Some("2h ago")
        );
        // News bucket carries its tag.
        assert_eq!(response.sources[1].title, "[News] N");
        assert_eq!(response.related_questions, vec!["Why?"]);
        assert_eq!(response.answer.as_deref(), Some("Because."));
    }

    #[test]
    fn a_question_prefers_props_then_title() {
        let with_props = json!({ "props": { "question": "Q?" }, "title": "T" });
        assert_eq!(question_of(&with_props).as_deref(), Some("Q?"));
        let title_only = json!({ "title": "T" });
        assert_eq!(question_of(&title_only).as_deref(), Some("T"));
    }

    #[test]
    fn the_body_carries_workflow_and_recency_filter() {
        let body = build_body("rust", 10, Some(Recency::Week));
        assert_eq!(body["workflow"], json!("search"));
        assert_eq!(body["limit"], json!(10));
        assert!(body["filters"]["after"].is_string());
        assert!(build_body("rust", 10, None).get("filters").is_none());
    }
}
