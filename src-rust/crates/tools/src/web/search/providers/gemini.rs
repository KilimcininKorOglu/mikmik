// Gemini provider: Google Search grounding through the Gemini API.
//
// Ported from oh-my-pi `web/search/providers/gemini.ts`. This slice covers the
// developer-API-key transport (`x-goog-api-key` against
// `generativelanguage.googleapis.com`); the OAuth Cloud Code Assist transport
// lands separately. Both share the grounded-response parse in `parse_stream`:
// the `googleSearch` tool answers with `groundingMetadata` (chunks, supports,
// queries) that maps onto answer, sources, citations, and search queries.
//
// Like omp (and the Anthropic provider) the request is raw HTTP rather than the
// `mikmik_api` transformer, which neither forwards the `googleSearch` tool nor
// preserves grounding metadata on the response.

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

pub struct GeminiProvider;

const DEFAULT_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_DEVELOPER_HOST: &str = "https://generativelanguage.googleapis.com";
const DEVELOPER_API_VERSION: &str = "v1beta";
const CLOUDFLARE_GATEWAY_HOST: &str = "gateway.ai.cloudflare.com";

/// A non-empty environment variable, or `None`.
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The search model: `GEMINI_SEARCH_MODEL`, else the Flash default.
fn search_model() -> String {
    nonempty_env("GEMINI_SEARCH_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// The developer API key: the stored `google` credential first (written by the
/// TUI key-entry flow), then `GOOGLE_API_KEY`/`GOOGLE_GENERATIVE_AI_API_KEY` via
/// the config resolver, then omp's `GEMINI_API_KEY` fallback.
fn developer_key(ctx: &ToolContext) -> Option<String> {
    ctx.config
        .resolve_provider_api_key("google")
        .filter(|k| !k.is_empty())
        .or_else(|| nonempty_env("GEMINI_API_KEY"))
}

/// The developer endpoint: `GOOGLE_GEMINI_BASE_URL`, else the default host, plus
/// whether it is a Cloudflare AI Gateway (which takes a different auth header).
fn developer_endpoint() -> (String, bool) {
    let host = nonempty_env("GOOGLE_GEMINI_BASE_URL")
        .map(|h| h.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_DEVELOPER_HOST.to_string());
    let is_cloudflare = url::Url::parse(&host)
        .ok()
        .and_then(|u| u.host_str().map(|h| h == CLOUDFLARE_GATEWAY_HOST))
        .unwrap_or(false);
    (host, is_cloudflare)
}

/// The grounded query. `googleSearch` forwards to Google Search, which parses
/// the classic operator set, so directives re-emit as query text; a
/// directive-free query passes through byte-identical.
fn plan_query(raw: &str, parsed: &StructuredQuery) -> String {
    if parsed.has_directives {
        format_query(parsed, QuerySyntax::google())
    } else {
        raw.to_string()
    }
}

/// The developer-transport request body.
fn request_body(query: &str) -> Value {
    json!({
        "contents": [{ "role": "user", "parts": [{ "text": query }] }],
        "tools": [{ "googleSearch": {} }],
    })
}

/// The grounded result folded out of the response stream.
#[derive(Default)]
struct Grounded {
    answer: String,
    sources: Vec<SearchSource>,
    citations: Vec<SearchCitation>,
    search_queries: Vec<String>,
    model: Option<String>,
    usage: Option<SearchUsage>,
    seen_urls: HashSet<String>,
}

/// A string field on a JSON object, when present and a string.
fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The `web` object of a grounding chunk, when it carries a `uri`.
fn chunk_web(chunk: &Value) -> Option<(&str, &str)> {
    let web = chunk.get("web")?;
    let uri = str_field(web, "uri")?;
    let title = str_field(web, "title").unwrap_or(uri);
    Some((uri, title))
}

/// Fold `groundingChunks` into deduplicated sources.
fn absorb_chunks(chunks: &[Value], acc: &mut Grounded) {
    for chunk in chunks {
        if let Some((uri, title)) = chunk_web(chunk) {
            if acc.seen_urls.insert(uri.to_string()) {
                acc.sources.push(SearchSource {
                    title: title.to_string(),
                    url: uri.to_string(),
                    ..Default::default()
                });
            }
        }
    }
}

/// Fold `groundingSupports` into citations that point back at their chunks.
fn absorb_supports(supports: &[Value], chunks: &[Value], acc: &mut Grounded) {
    for support in supports {
        let cited = support
            .get("segment")
            .and_then(|s| str_field(s, "text"))
            .map(str::to_string);
        let indices = support
            .get("groundingChunkIndices")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for idx in indices {
            let Some((uri, title)) = idx
                .as_u64()
                .and_then(|i| chunks.get(i as usize))
                .and_then(chunk_web)
            else {
                continue;
            };
            acc.citations.push(SearchCitation {
                url: uri.to_string(),
                title: title.to_string(),
                cited_text: cited.clone(),
            });
        }
    }
}

/// Fold one candidate's grounding metadata into the accumulator.
fn absorb_grounding(meta: &Value, acc: &mut Grounded) {
    let chunks = meta
        .get("groundingChunks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    absorb_chunks(&chunks, acc);
    if let Some(supports) = meta.get("groundingSupports").and_then(Value::as_array) {
        absorb_supports(supports, &chunks, acc);
    }
    if let Some(queries) = meta.get("webSearchQueries").and_then(Value::as_array) {
        for q in queries.iter().filter_map(Value::as_str) {
            if !acc.search_queries.iter().any(|s| s == q) {
                acc.search_queries.push(q.to_string());
            }
        }
    }
}

/// Fold `usageMetadata` into the accumulator.
fn absorb_usage(usage: &Value, acc: &mut Grounded) {
    let count = |key: &str| usage.get(key).and_then(Value::as_u64);
    acc.usage = Some(SearchUsage {
        input_tokens: count("promptTokenCount"),
        output_tokens: count("candidatesTokenCount"),
        total_tokens: count("totalTokenCount"),
        ..Default::default()
    });
}

/// Fold one response object (SSE chunk or whole body) into the accumulator.
fn absorb_response(response: &Value, acc: &mut Grounded) {
    // Cloud Code Assist wraps the model response under `response`; the developer
    // API returns it bare.
    let data = response.get("response").unwrap_or(response);
    let candidate = data
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    if let Some(candidate) = candidate {
        if let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for text in parts.iter().filter_map(|p| str_field(p, "text")) {
                acc.answer.push_str(text);
            }
        }
        if let Some(meta) = candidate.get("groundingMetadata") {
            absorb_grounding(meta, acc);
        }
    }
    if let Some(usage) = data.get("usageMetadata") {
        absorb_usage(usage, acc);
    }
    if let Some(version) = str_field(data, "modelVersion") {
        acc.model = Some(version.to_string());
    }
}

/// Parse a grounded response body: SSE `data:` lines when present, else the
/// whole body as a single JSON object (non-streaming responses).
fn parse_stream(body: &str, fallback_model: &str) -> Grounded {
    let mut acc = Grounded {
        model: Some(fallback_model.to_string()),
        ..Default::default()
    };
    let mut saw_data = false;
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if payload.is_empty() {
            continue;
        }
        saw_data = true;
        if let Ok(chunk) = serde_json::from_str::<Value>(payload) {
            absorb_response(&chunk, &mut acc);
        }
    }
    if !saw_data {
        if let Ok(whole) = serde_json::from_str::<Value>(body.trim()) {
            absorb_response(&whole, &mut acc);
        }
    }
    acc
}

/// Map the accumulated grounding onto a `SearchResponse`, capping the sources.
fn to_response(mut grounded: Grounded, limit: usize) -> SearchResponse {
    if grounded.sources.len() > limit {
        grounded.sources.truncate(limit);
    }
    let mut resp = SearchResponse::empty(SearchProviderId::Gemini.as_str());
    if !grounded.answer.is_empty() {
        resp.answer = Some(grounded.answer);
    }
    resp.sources = grounded.sources;
    resp.citations = grounded.citations;
    resp.search_queries = grounded.search_queries;
    resp.usage = grounded.usage;
    resp.model = grounded.model;
    resp
}

/// Post the developer-transport request and return the response body text.
async fn call_developer(
    key: &str,
    query: &str,
    model: &str,
    timeout: std::time::Duration,
) -> Result<String, SearchProviderError> {
    let id = SearchProviderId::Gemini;
    let (host, is_cloudflare) = developer_endpoint();
    let url =
        format!("{host}/{DEVELOPER_API_VERSION}/models/{model}:streamGenerateContent?alt=sse");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| SearchProviderError::new(id, format!("HTTP client: {e}")))?;
    let mut builder = client
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    builder = if is_cloudflare {
        builder.header("cf-aig-authorization", format!("Bearer {key}"))
    } else {
        builder.header("x-goog-api-key", key)
    };
    let resp = builder
        .json(&request_body(query))
        .send()
        .await
        .map_err(|e| SearchProviderError::new(id, format!("Search request failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return Err(SearchProviderError::with_status(
            id,
            format!("Gemini Developer API returned status {status}"),
            status,
        ));
    }
    resp.text()
        .await
        .map_err(|e| SearchProviderError::new(id, format!("Failed to read response: {e}")))
}

#[async_trait]
impl SearchProvider for GeminiProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Gemini
    }

    async fn is_available(&self, ctx: &ToolContext) -> bool {
        developer_key(ctx).is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = developer_key(params.ctx).ok_or_else(|| {
            SearchProviderError::new(
                self.id(),
                "No Gemini credentials found. Set GEMINI_API_KEY or configure an API key for the google provider.",
            )
        })?;
        let model = search_model();
        let query = plan_query(params.query, params.parsed);
        let body = call_developer(&key, &query, &model, params.timeout).await?;
        let grounded = parse_stream(&body, &model);
        if grounded.answer.is_empty() && grounded.sources.is_empty() {
            return Err(SearchProviderError::with_status(
                self.id(),
                "Gemini API returned an empty grounded response",
                502,
            ));
        }
        Ok(to_response(grounded, params.limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::query::parse_search_query;

    #[test]
    fn a_directive_free_query_passes_through() {
        let parsed = parse_search_query("rust async runtime");
        assert_eq!(
            plan_query("rust async runtime", &parsed),
            "rust async runtime"
        );
    }

    #[test]
    fn directives_re_emit_as_google_syntax() {
        let parsed = parse_search_query("tokio site:docs.rs filetype:pdf");
        let planned = plan_query("tokio site:docs.rs filetype:pdf", &parsed);
        assert!(planned.contains("site:docs.rs"));
        assert!(planned.contains("filetype:pdf"));
    }

    #[test]
    fn the_body_carries_the_google_search_tool() {
        let body = request_body("rust");
        assert_eq!(body["tools"][0]["googleSearch"], json!({}));
        assert_eq!(body["contents"][0]["parts"][0]["text"], json!("rust"));
        assert_eq!(body["contents"][0]["role"], json!("user"));
    }

    #[test]
    fn sse_chunks_fold_into_answer_sources_citations_and_queries() {
        let body = concat!(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Tokio is \"}]}}]}}\n",
            "\n",
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"an async runtime.\"}]},",
            "\"groundingMetadata\":{",
            "\"groundingChunks\":[{\"web\":{\"uri\":\"https://a\",\"title\":\"A\"}},{\"web\":{\"uri\":\"https://b\"}}],",
            "\"groundingSupports\":[{\"segment\":{\"text\":\"async runtime\"},\"groundingChunkIndices\":[0]}],",
            "\"webSearchQueries\":[\"tokio runtime\",\"tokio runtime\"]",
            "}}],",
            "\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":9,\"totalTokenCount\":14},",
            "\"modelVersion\":\"gemini-2.5-flash-001\"}}\n",
        );
        let grounded = parse_stream(body, "gemini-2.5-flash");
        let resp = to_response(grounded, 20);
        assert_eq!(resp.answer.as_deref(), Some("Tokio is an async runtime."));
        assert_eq!(resp.sources.len(), 2);
        assert_eq!(resp.sources[0].title, "A");
        // A chunk without a title falls back to its url.
        assert_eq!(resp.sources[1].title, "https://b");
        assert_eq!(resp.citations.len(), 1);
        assert_eq!(resp.citations[0].url, "https://a");
        assert_eq!(
            resp.citations[0].cited_text.as_deref(),
            Some("async runtime")
        );
        // Duplicate query collapses.
        assert_eq!(resp.search_queries, vec!["tokio runtime"]);
        assert_eq!(resp.usage.and_then(|u| u.total_tokens), Some(14));
        assert_eq!(resp.model.as_deref(), Some("gemini-2.5-flash-001"));
    }

    #[test]
    fn a_non_streaming_body_parses_as_one_object() {
        let body = "{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]},\"groundingMetadata\":{\"groundingChunks\":[{\"web\":{\"uri\":\"https://x\",\"title\":\"X\"}}]}}]}";
        let grounded = parse_stream(body, "gemini-2.5-flash");
        assert_eq!(grounded.answer, "hi");
        assert_eq!(grounded.sources.len(), 1);
        assert_eq!(grounded.sources[0].url, "https://x");
    }

    #[test]
    fn duplicate_grounding_urls_dedupe_across_chunks() {
        let body = concat!(
            "data: {\"candidates\":[{\"groundingMetadata\":{\"groundingChunks\":[{\"web\":{\"uri\":\"https://a\",\"title\":\"A\"}}]}}]}\n",
            "data: {\"candidates\":[{\"groundingMetadata\":{\"groundingChunks\":[{\"web\":{\"uri\":\"https://a\",\"title\":\"A2\"}}]}}]}\n",
        );
        let grounded = parse_stream(body, "m");
        assert_eq!(grounded.sources.len(), 1);
        assert_eq!(grounded.sources[0].title, "A");
    }

    #[test]
    fn the_cloudflare_gateway_host_is_detected() {
        std::env::set_var(
            "GOOGLE_GEMINI_BASE_URL",
            "https://gateway.ai.cloudflare.com/v1/acct/gw/google-ai-studio",
        );
        let (_, is_cf) = developer_endpoint();
        std::env::remove_var("GOOGLE_GEMINI_BASE_URL");
        assert!(is_cf);
        // The default host is not a gateway.
        let (host, is_cf) = developer_endpoint();
        assert_eq!(host, DEFAULT_DEVELOPER_HOST);
        assert!(!is_cf);
    }
}
