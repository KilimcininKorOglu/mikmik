// xAI provider: Grok's `web_search` tool over the Responses API.
//
// Ported from oh-my-pi `web/search/providers/xai.ts`. A non-streaming
// `POST /v1/responses` (no SSE) carrying the `web_search` tool; the JSON
// response's annotations, `web_search_call` sources and top-level citations map
// onto sources + citations. `site:`/`-site:` map onto the tool's native
// `allowed_domains`/`excluded_domains`; the rest re-emit as query syntax.

use crate::web::search::provider::{SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax, StructuredQuery};
use crate::web::search::types::{
    SearchCitation, SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
    SearchUsage,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;

pub struct XaiProvider;

const XAI_DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
const XAI_WEB_SEARCH_MODEL: &str = "grok-4.5";
const XAI_REASONING_EFFORT: &str = "low";
const MAX_DOMAIN_FILTERS: usize = 5;

/// `site:` is stripped (hosts map onto the tool's domain filters); the rest of
/// the classic operators re-emit as query text.
const XAI_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: true,
    site: false,
    in_url: true,
    in_title: true,
    in_text: false,
    filetype: true,
    date_range: true,
};

/// A non-empty environment variable, or `None`.
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The xAI bearer credential: a stored key first (the TUI key-entry rule), then
/// the env fallbacks. A dedicated `xai-oauth` subscription token is preferred
/// over the plain `xai` API key, matching omp.
fn api_key() -> Option<String> {
    let store = mikmik_core::AuthStore::load();
    store
        .api_key_for(mikmik_core::provider_id::ProviderId::XAI_OAUTH)
        .filter(|k| !k.is_empty())
        .or_else(|| store.api_key_for(mikmik_core::provider_id::ProviderId::XAI))
        .filter(|k| !k.is_empty())
        .or_else(|| nonempty_env("XAI_OAUTH_TOKEN"))
        .or_else(|| nonempty_env("XAI_API_KEY"))
}

/// The Responses endpoint: an env override, else the xAI default.
fn responses_url() -> String {
    let base = nonempty_env("XAI_BASE_URL").unwrap_or_else(|| XAI_DEFAULT_BASE_URL.to_string());
    format!("{}/responses", base.trim_end_matches('/'))
}

/// Bare hosts of `site:` values, deduped and capped at five.
fn domain_filters(sites: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for site in sites {
        let host = match site.find('/') {
            Some(slash) => &site[..slash],
            None => site.as_str(),
        };
        if !host.is_empty() && !hosts.iter().any(|h| h == host) {
            hosts.push(host.to_string());
            if hosts.len() == MAX_DOMAIN_FILTERS {
                break;
            }
        }
    }
    hosts
}

/// The query text and the `web_search` tool (with domain filters when directives
/// name `site:` includes or excludes).
fn plan(raw: &str, parsed: &StructuredQuery) -> (String, Value) {
    let mut tool = json!({ "type": "web_search" });
    if !parsed.has_directives {
        return (raw.to_string(), tool);
    }
    let allowed = domain_filters(&parsed.sites);
    if !allowed.is_empty() {
        tool["filters"] = json!({ "allowed_domains": allowed });
    } else {
        let excluded = domain_filters(&parsed.excluded_sites);
        if !excluded.is_empty() {
            tool["filters"] = json!({ "excluded_domains": excluded });
        }
    }
    (format_query(parsed, XAI_SYNTAX), tool)
}

/// The Responses request body.
fn build_body(query: &str, tool: Value) -> Value {
    json!({
        "model": XAI_WEB_SEARCH_MODEL,
        "input": [{ "role": "user", "content": query }],
        "tools": [tool],
        "reasoning": { "effort": XAI_REASONING_EFFORT },
    })
}

/// Sources and citations accumulated from a response, deduped by URL.
#[derive(Default)]
struct Collected {
    sources: Vec<SearchSource>,
    citations: Vec<SearchCitation>,
    seen: HashSet<String>,
}

/// A trimmed non-empty string field on a JSON object.
fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Record one cited URL as both a source and a citation, once.
fn add_citation_source(acc: &mut Collected, url: &str, title: Option<&str>, cited: Option<String>) {
    let url = url.trim();
    if url.is_empty() || !acc.seen.insert(url.to_string()) {
        return;
    }
    let source_title = trimmed(title).unwrap_or_else(|| url.to_string());
    let snippet = cited
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    acc.sources.push(SearchSource {
        title: source_title.clone(),
        url: url.to_string(),
        snippet: snippet.clone(),
        ..Default::default()
    });
    acc.citations.push(SearchCitation {
        title: source_title,
        url: url.to_string(),
        cited_text: snippet,
    });
}

/// A ±100-char window around an annotation span, stripping markdown links.
fn extract_snippet_around(
    text: Option<&str>,
    start: Option<i64>,
    end: Option<i64>,
) -> Option<String> {
    let (text, start, end) = (text?, start?, end?);
    let chars: Vec<char> = text.chars().collect();
    let before = (start - 100).max(0) as usize;
    let after = ((end + 100) as usize).min(chars.len());
    if before >= after {
        return None;
    }
    let window: String = chars[before..after].iter().collect();
    let stripped = MARKDOWN_LINK.replace_all(&window, "$1").trim().to_string();
    if stripped.is_empty() {
        return None;
    }
    Some(if stripped.chars().count() > 300 {
        let head: String = stripped.chars().take(297).collect();
        format!("{head}...")
    } else {
        stripped
    })
}

static MARKDOWN_LINK: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
    regex::Regex::new(r"\[([^\]]*)\]\([^)]*\)").expect("static link regex")
});

/// Fold `url_citation` annotations into the accumulator.
fn collect_annotations(
    annotations: Option<&Value>,
    content_text: Option<&str>,
    acc: &mut Collected,
) {
    let Some(items) = annotations.and_then(Value::as_array) else {
        return;
    };
    for annotation in items {
        if str_field(annotation, "type") != Some("url_citation") {
            continue;
        }
        let Some(url) = str_field(annotation, "url") else {
            continue;
        };
        let cited = trimmed(str_field(annotation, "cited_text"))
            .or_else(|| trimmed(str_field(annotation, "text")))
            .or_else(|| {
                extract_snippet_around(
                    content_text,
                    annotation.get("start_index").and_then(Value::as_i64),
                    annotation.get("end_index").and_then(Value::as_i64),
                )
            });
        add_citation_source(acc, url, str_field(annotation, "title"), cited);
    }
}

/// Fold a `web_search_call` item's source groups into the accumulator.
fn collect_web_search_sources(item: &Value, acc: &mut Collected) {
    if str_field(item, "type") != Some("web_search_call") {
        return;
    }
    let groups = [
        item.get("action").and_then(|a| a.get("sources")),
        item.get("sources"),
        item.get("results"),
    ];
    for group in groups.into_iter().flatten() {
        let Some(list) = group.as_array() else {
            continue;
        };
        for source in list {
            let Some(url) =
                str_field(source, "url").or_else(|| str_field(source, "source_website_url"))
            else {
                continue;
            };
            let title = str_field(source, "title").or_else(|| str_field(source, "caption"));
            add_citation_source(acc, url, title, None);
        }
    }
}

/// The synthesized answer: the top-level `output_text`, else the joined text of
/// every output content part.
fn parse_answer(response: &Value) -> Option<String> {
    if let Some(top) = trimmed(str_field(response, "output_text")) {
        return Some(top);
    }
    let mut parts: Vec<String> = Vec::new();
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(text) =
                trimmed(str_field(part, "output_text").or_else(|| str_field(part, "text")))
            {
                parts.push(text);
            }
        }
    }
    let answer = parts.join("\n");
    let answer = answer.trim();
    (!answer.is_empty()).then(|| answer.to_string())
}

/// Usage metrics, accepting both snake_case and camelCase token counts.
fn parse_usage(usage: &Value) -> Option<SearchUsage> {
    let count = |snake: &str, camel: &str| {
        usage
            .get(snake)
            .and_then(Value::as_u64)
            .or_else(|| usage.get(camel).and_then(Value::as_u64))
    };
    let parsed = SearchUsage {
        input_tokens: count("input_tokens", "inputTokens"),
        output_tokens: count("output_tokens", "outputTokens"),
        total_tokens: count("total_tokens", "totalTokens"),
        ..Default::default()
    };
    let empty = parsed.input_tokens.is_none()
        && parsed.output_tokens.is_none()
        && parsed.total_tokens.is_none();
    (!empty).then_some(parsed)
}

/// Map the raw Responses JSON onto a `SearchResponse`, capping the results.
fn parse_response(response: &Value, cap: usize) -> SearchResponse {
    let mut acc = Collected::default();
    collect_annotations(response.get("annotations"), None, &mut acc);
    let output = response.get("output").and_then(Value::as_array);
    for item in output.into_iter().flatten() {
        collect_annotations(item.get("annotations"), None, &mut acc);
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let text = str_field(part, "output_text").or_else(|| str_field(part, "text"));
            collect_annotations(part.get("annotations"), text, &mut acc);
        }
    }
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        collect_web_search_sources(item, &mut acc);
    }
    for url in response
        .get("citations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(url) = url.as_str() {
            add_citation_source(&mut acc, url, None, None);
        }
    }
    acc.sources.truncate(cap);
    acc.citations.truncate(cap);

    let mut resp = SearchResponse::empty(SearchProviderId::Xai.as_str());
    resp.answer = parse_answer(response);
    resp.sources = acc.sources;
    resp.citations = acc.citations;
    resp.usage = response.get("usage").and_then(parse_usage);
    resp.model = trimmed(str_field(response, "model"));
    resp
}

#[async_trait]
impl SearchProvider for XaiProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Xai
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
                "xAI credentials not found. Set XAI_API_KEY or configure an xai account.",
            )
        })?;
        let (query, tool) = plan(params.query, params.parsed);
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;
        let resp = client
            .post(responses_url())
            .header("content-type", "application/json")
            .header("Authorization", format!("Bearer {key}"))
            .json(&build_body(&query, tool))
            .send()
            .await
            .map_err(|e| {
                SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
            })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(SearchProviderError::with_status(
                self.id(),
                format!("xAI Responses API returned status {status}"),
                status,
            ));
        }
        let data: Value = resp.json().await.map_err(|e| {
            SearchProviderError::new(self.id(), format!("Failed to parse response: {e}"))
        })?;
        let response = parse_response(&data, params.limit);
        if response.answer.is_none() && response.sources.is_empty() {
            return Err(SearchProviderError::with_status(
                self.id(),
                "xAI web_search returned no answer or sources",
                502,
            ));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::query::parse_search_query;

    #[test]
    fn site_includes_become_allowed_domains_capped_at_five() {
        let parsed = parse_search_query(
            "x site:a.com site:b.com site:c.com site:d.com site:e.com site:f.com",
        );
        let (_query, tool) = plan("x ...", &parsed);
        let allowed = tool["filters"]["allowed_domains"].as_array().unwrap();
        assert_eq!(allowed.len(), 5);
    }

    #[test]
    fn exclusions_become_excluded_domains_only_without_includes() {
        let parsed = parse_search_query("cats -site:pinterest.com");
        let (_q, tool) = plan("cats -site:pinterest.com", &parsed);
        assert_eq!(
            tool["filters"]["excluded_domains"],
            json!(["pinterest.com"])
        );

        let both = parse_search_query("cats site:reddit.com -site:pinterest.com");
        let (_q, tool) = plan("cats ...", &both);
        assert_eq!(tool["filters"]["allowed_domains"], json!(["reddit.com"]));
        assert!(tool["filters"].get("excluded_domains").is_none());
    }

    #[test]
    fn a_directive_free_query_sends_a_bare_tool() {
        let parsed = parse_search_query("rust ownership");
        let (query, tool) = plan("rust ownership", &parsed);
        assert_eq!(query, "rust ownership");
        assert_eq!(tool, json!({ "type": "web_search" }));
    }

    #[test]
    fn the_body_pins_the_model_and_low_reasoning() {
        let body = build_body("rust", json!({ "type": "web_search" }));
        assert_eq!(body["model"], json!(XAI_WEB_SEARCH_MODEL));
        assert_eq!(body["reasoning"]["effort"], json!("low"));
        assert_eq!(body["input"][0]["role"], json!("user"));
        assert_eq!(body["input"][0]["content"], json!("rust"));
    }

    #[test]
    fn annotations_web_search_sources_and_top_citations_all_collect_deduped() {
        let response = json!({
            "id": "resp_1",
            "model": "grok-4.5",
            "output_text": "Rust is memory safe.",
            "annotations": [
                { "type": "url_citation", "url": "https://a", "title": "A", "cited_text": "safe" }
            ],
            "output": [
                { "type": "web_search_call", "action": { "sources": [
                    { "url": "https://b", "title": "B" },
                    { "url": "https://a", "title": "dup" }
                ]}}
            ],
            "citations": ["https://c", "https://a"],
            "usage": { "input_tokens": 3, "output_tokens": 8, "total_tokens": 11 }
        });
        let parsed = parse_response(&response, 20);
        assert_eq!(parsed.answer.as_deref(), Some("Rust is memory safe."));
        // a, b, c — the duplicate `a` is recorded once.
        assert_eq!(parsed.sources.len(), 3);
        assert_eq!(parsed.sources[0].url, "https://a");
        assert_eq!(parsed.sources[0].snippet.as_deref(), Some("safe"));
        assert_eq!(parsed.citations.len(), 3);
        assert_eq!(parsed.usage.and_then(|u| u.total_tokens), Some(11));
        assert_eq!(parsed.model.as_deref(), Some("grok-4.5"));
    }

    #[test]
    fn the_answer_falls_back_to_output_content_parts() {
        let response = json!({
            "output": [
                { "type": "message", "content": [
                    { "type": "output_text", "output_text": "part one" },
                    { "type": "output_text", "text": "part two" }
                ]}
            ]
        });
        assert_eq!(
            parse_answer(&response).as_deref(),
            Some("part one\npart two")
        );
    }

    #[test]
    fn the_result_cap_bounds_sources_and_citations() {
        let response = json!({
            "citations": ["https://a", "https://b", "https://c"],
            "output_text": "answer"
        });
        let parsed = parse_response(&response, 2);
        assert_eq!(parsed.sources.len(), 2);
        assert_eq!(parsed.citations.len(), 2);
    }
}
