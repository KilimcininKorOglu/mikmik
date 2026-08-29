// Z.AI provider: the `web_search_prime` remote MCP server.
//
// Ported from oh-my-pi `web/search/providers/zai.ts`. Z.AI exposes web search
// as a streamable-HTTP MCP endpoint, so a search is a three-step JSON-RPC dance
// (`initialize` -> `notifications/initialized` -> `tools/call`) threaded by an
// `Mcp-Session-Id` header. The tool result arrives in several shapes; the
// extraction and mapping are pure functions the tests exercise without network.

use crate::web::search::provider::{SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax, StructuredQuery};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct ZaiProvider;

const ZAI_MCP_URL: &str = "https://api.z.ai/api/mcp/web_search_prime/mcp";
const ZAI_TOOL_NAME: &str = "web_search_prime";
const ZAI_MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const SESSION_HEADER: &str = "Mcp-Session-Id";

/// `web_search_prime` exposes no native filter args, but its Bing-flavored
/// backend parses the common inline operators. Dates and language stay with the
/// central constraint filter.
const ZAI_SYNTAX: QuerySyntax = QuerySyntax {
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

/// The Z.AI credential: the stored `zai` key first, then `ZAI_API_KEY`.
fn api_key() -> Option<String> {
    super::stored_or_env_key(SearchProviderId::Zai, "ZAI_API_KEY")
}

/// The grounded query. Directives re-emit as Bing operator syntax.
fn plan_query(raw: &str, parsed: &StructuredQuery) -> String {
    if parsed.has_directives {
        format_query(parsed, ZAI_SYNTAX)
    } else {
        raw.to_string()
    }
}

/// A trimmed non-empty string, or `None`.
fn as_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The last JSON message in an MCP response: the final `data:` SSE line, else
/// the whole body parsed as one object.
fn parse_mcp_response(raw: &str) -> Option<Value> {
    let mut last: Option<Value> = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if payload.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            last = Some(value);
        }
    }
    last.or_else(|| serde_json::from_str::<Value>(raw.trim()).ok())
}

/// Read a JSON-RPC message: surface an error envelope, else return `result`.
///
/// Tolerates both the JSON-RPC `error` object and Z.AI's flat `{success:false,
/// msg}` shape, so a backend rejection becomes a provider error rather than an
/// empty result.
fn read_jsonrpc_payload(parsed: &Value) -> Result<Value, SearchProviderError> {
    let id = SearchProviderId::Zai;
    if parsed.get("success") == Some(&Value::Bool(false)) {
        let message = as_string(parsed.get("msg").unwrap_or(&Value::Null))
            .or_else(|| as_string(parsed.get("message").unwrap_or(&Value::Null)))
            .or_else(|| as_string(parsed.get("error_message").unwrap_or(&Value::Null)));
        if let Some(message) = message {
            return Err(SearchProviderError::new(
                id,
                format!("Z.AI API error: {message}"),
            ));
        }
    }
    if let Some(error) = parsed.get("error").filter(|e| !e.is_null()) {
        let code = error.get("code").and_then(Value::as_i64);
        let message = as_string(error.get("message").unwrap_or(&Value::Null))
            .unwrap_or_else(|| "Unknown error".to_string());
        let status = code.filter(|c| (0..=599).contains(c)).map(|c| c as u16);
        return match status {
            Some(status) => Err(SearchProviderError::with_status(
                id,
                format!("Z.AI MCP error: {message}"),
                status,
            )),
            None => Err(SearchProviderError::new(
                id,
                format!("Z.AI MCP error: {message}"),
            )),
        };
    }
    Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
}

/// The joined text of a tool result flagged `isError`, when present.
fn tool_result_error(result: &Value) -> Option<String> {
    if result.get("isError") != Some(&Value::Bool(true)) {
        return None;
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| as_string(p.get("text").unwrap_or(&Value::Null)))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    Some(if text.is_empty() {
        "Z.AI MCP tool call failed".to_string()
    } else {
        text
    })
}

/// Search-result rows out of one candidate: a bare array, `search_result`, or
/// `results`.
fn get_search_results(value: &Value) -> Vec<Value> {
    if let Some(array) = value.as_array() {
        return array.clone();
    }
    for key in ["search_result", "results"] {
        if let Some(array) = value.get(key).and_then(Value::as_array) {
            return array.clone();
        }
    }
    Vec::new()
}

/// The candidate objects to search for result rows, widest first.
fn payload_candidates(raw: &Value) -> (Vec<Value>, Vec<String>) {
    let mut candidates = vec![raw.clone()];
    let mut text_parts: Vec<String> = Vec::new();
    for key in ["structuredContent", "data", "result"] {
        if let Some(v) = raw.get(key) {
            candidates.push(v.clone());
        }
    }
    if let Some(content) = raw.get("content").and_then(Value::as_array) {
        for part in content {
            let Some(text) = as_string(part.get("text").unwrap_or(&Value::Null)) else {
                continue;
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(parsed) => {
                    let inner = reparse_string(parsed);
                    if get_search_results(&inner).is_empty() {
                        text_parts.push(text);
                    }
                    candidates.push(inner);
                }
                Err(_) => text_parts.push(text),
            }
        }
    }
    (candidates, text_parts)
}

/// A JSON string that itself holds JSON is decoded one more level; anything else
/// passes through unchanged.
fn reparse_string(value: Value) -> Value {
    match value {
        Value::String(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)),
        other => other,
    }
}

/// The parsed search payload: result rows, an optional answer, a request id.
fn parse_search_payload(raw: &Value) -> (Vec<Value>, Option<String>, Option<String>) {
    let (candidates, text_parts) = payload_candidates(raw);
    let answer = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n\n"))
    };
    for candidate in &candidates {
        let results = get_search_results(candidate);
        if !results.is_empty() {
            let request_id = ["request_id", "requestId", "id"]
                .iter()
                .find_map(|k| as_string(candidate.get(*k).unwrap_or(&Value::Null)));
            return (results, answer, request_id);
        }
    }
    (Vec::new(), answer, None)
}

/// Map result rows onto sources, skipping rows without a link.
fn to_sources(results: &[Value]) -> Vec<SearchSource> {
    let field = |r: &Value, k: &str| as_string(r.get(k).unwrap_or(&Value::Null));
    let mut sources = Vec::new();
    for result in results {
        let Some(url) = field(result, "link").or_else(|| field(result, "url")) else {
            continue;
        };
        let published_date =
            field(result, "publish_date").or_else(|| field(result, "publishedDate"));
        let age_seconds = published_date
            .as_deref()
            .and_then(super::date_to_age_seconds);
        sources.push(SearchSource {
            title: field(result, "title").unwrap_or_else(|| url.clone()),
            url,
            snippet: field(result, "content"),
            published_date,
            age_seconds,
            author: field(result, "media"),
        });
    }
    sources
}

/// One JSON-RPC POST. Returns the parsed message (when a response is expected)
/// and the session id to thread into the next call.
async fn post_mcp(
    client: &reqwest::Client,
    key: &str,
    method: &str,
    params: Value,
    session_id: Option<&str>,
    expect_response: bool,
) -> Result<(Option<Value>, Option<String>), SearchProviderError> {
    let id = SearchProviderId::Zai;
    let mut body = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    if expect_response {
        body["id"] = json!(uuid::Uuid::new_v4().to_string());
    }
    let mut builder = client
        .post(ZAI_MCP_URL)
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if let Some(session) = session_id {
        builder = builder.header(SESSION_HEADER, session);
    }
    let resp = builder
        .json(&body)
        .send()
        .await
        .map_err(|e| SearchProviderError::new(id, format!("Z.AI MCP request failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return Err(SearchProviderError::with_status(
            id,
            format!("Z.AI MCP returned status {status}"),
            status,
        ));
    }
    let next_session = resp
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| session_id.map(str::to_string));
    if !expect_response {
        return Ok((None, next_session));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| SearchProviderError::new(id, format!("Failed to read Z.AI response: {e}")))?;
    Ok((parse_mcp_response(&text), next_session))
}

/// Run the `initialize` -> `notifications/initialized` -> `tools/call` dance and
/// return the tool result.
async fn call_tool(
    client: &reqwest::Client,
    key: &str,
    args: Value,
) -> Result<Value, SearchProviderError> {
    let (init, session) = post_mcp(
        client,
        key,
        "initialize",
        json!({
            "protocolVersion": ZAI_MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "mikmik-coding-agent", "version": "1.0.0" },
        }),
        None,
        true,
    )
    .await?;
    if let Some(parsed) = &init {
        read_jsonrpc_payload(parsed)?;
    }
    post_mcp(
        client,
        key,
        "notifications/initialized",
        json!({}),
        session.as_deref(),
        false,
    )
    .await?;
    let (tool_call, _) = post_mcp(
        client,
        key,
        "tools/call",
        json!({ "name": ZAI_TOOL_NAME, "arguments": args }),
        session.as_deref(),
        true,
    )
    .await?;
    let parsed = tool_call.ok_or_else(|| {
        SearchProviderError::new(SearchProviderId::Zai, "Z.AI returned no result")
    })?;
    let result = read_jsonrpc_payload(&parsed)?;
    if let Some(message) = tool_result_error(&result) {
        return Err(SearchProviderError::new(SearchProviderId::Zai, message));
    }
    Ok(if result.is_null() { parsed } else { result })
}

/// The tool argument shapes to try in order, so a rename of the query field does
/// not fail the search outright.
fn arg_attempts(query: &str, count: usize) -> Vec<Value> {
    vec![
        json!({ "query": query, "count": count }),
        json!({ "search_query": query, "count": count }),
        json!({ "search_query": query, "search_engine": "search-prime", "count": count }),
    ]
}

/// True when an error reads like an argument-shape rejection worth retrying.
fn looks_like_arg_error(error: &SearchProviderError) -> bool {
    let message = error.message.to_lowercase();
    error.status == Some(400)
        || ["invalid", "argument", "search_query", "query"]
            .iter()
            .any(|needle| message.contains(needle))
}

async fn call_search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    count: usize,
) -> Result<Value, SearchProviderError> {
    let attempts = arg_attempts(query, count);
    let last = attempts.len() - 1;
    let mut last_error: Option<SearchProviderError> = None;
    for (index, args) in attempts.into_iter().enumerate() {
        match call_tool(client, key, args).await {
            Ok(result) => return Ok(result),
            Err(error) => {
                if index == last || !looks_like_arg_error(&error) {
                    return Err(error);
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| SearchProviderError::new(SearchProviderId::Zai, "Z.AI search failed")))
}

#[async_trait]
impl SearchProvider for ZaiProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Zai
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
                "Z.AI credentials not found. Set ZAI_API_KEY or configure a zai account.",
            )
        })?;
        let query = plan_query(params.query, params.parsed);
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;
        let raw = call_search(&client, &key, &query, params.limit).await?;
        // `SearchResponse` carries no request id, so the parsed one is dropped.
        let (results, answer, _request_id) = parse_search_payload(&raw);
        let mut sources = to_sources(&results);
        if sources.len() > params.limit {
            sources.truncate(params.limit);
        }
        let mut response = SearchResponse::empty(self.id().as_str());
        response.answer = answer;
        response.sources = sources;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::query::parse_search_query;

    #[test]
    fn directives_re_emit_as_bing_syntax() {
        let parsed = parse_search_query("axum site:docs.rs");
        assert!(plan_query("axum site:docs.rs", &parsed).contains("site:docs.rs"));
        let plain = parse_search_query("plain query");
        assert_eq!(plan_query("plain query", &plain), "plain query");
    }

    #[test]
    fn the_last_sse_data_line_wins_and_a_bare_body_parses() {
        let sse = "data: {\"id\":1}\n\ndata: {\"id\":2,\"result\":{}}\n";
        assert_eq!(parse_mcp_response(sse).unwrap()["id"], json!(2));
        let bare = "{\"result\":{\"ok\":true}}";
        assert_eq!(
            parse_mcp_response(bare).unwrap()["result"]["ok"],
            json!(true)
        );
        assert!(parse_mcp_response("not json").is_none());
    }

    #[test]
    fn a_jsonrpc_error_and_a_flat_failure_both_surface() {
        let rpc = json!({ "error": { "code": 429, "message": "rate limited" } });
        let err = read_jsonrpc_payload(&rpc).unwrap_err();
        assert_eq!(err.status, Some(429));
        assert!(err.message.contains("rate limited"));

        let flat = json!({ "success": false, "msg": "bad key" });
        let err = read_jsonrpc_payload(&flat).unwrap_err();
        assert!(err.message.contains("bad key"));

        let ok = json!({ "result": { "content": [] } });
        assert_eq!(read_jsonrpc_payload(&ok).unwrap(), json!({ "content": [] }));
    }

    #[test]
    fn an_is_error_tool_result_joins_its_text() {
        let result = json!({ "isError": true, "content": [ { "text": "MCP error -32001" }, { "text": "bad" } ] });
        assert_eq!(
            tool_result_error(&result).as_deref(),
            Some("MCP error -32001\nbad")
        );
        assert!(tool_result_error(&json!({ "content": [] })).is_none());
    }

    #[test]
    fn search_results_come_from_an_array_or_a_named_field() {
        assert_eq!(get_search_results(&json!([{ "a": 1 }])).len(), 1);
        assert_eq!(
            get_search_results(&json!({ "search_result": [{}, {}] })).len(),
            2
        );
        assert_eq!(get_search_results(&json!({ "results": [{}] })).len(), 1);
        assert!(get_search_results(&json!({ "other": [] })).is_empty());
    }

    #[test]
    fn a_content_text_json_string_yields_results_and_a_request_id() {
        // The tool result nests its search rows inside a JSON string under content[].text.
        let inner = "{\"search_result\":[{\"title\":\"A\",\"link\":\"https://a\",\"content\":\"x\",\"media\":\"Site\"}],\"request_id\":\"req-1\"}";
        let raw = json!({ "content": [ { "type": "text", "text": inner } ] });
        let (results, _answer, request_id) = parse_search_payload(&raw);
        assert_eq!(results.len(), 1);
        assert_eq!(request_id.as_deref(), Some("req-1"));
        let sources = to_sources(&results);
        assert_eq!(sources[0].title, "A");
        assert_eq!(sources[0].url, "https://a");
        assert_eq!(sources[0].author.as_deref(), Some("Site"));
    }

    #[test]
    fn non_json_content_text_becomes_the_answer() {
        let raw = json!({ "content": [ { "type": "text", "text": "a prose answer" } ] });
        let (results, answer, _) = parse_search_payload(&raw);
        assert!(results.is_empty());
        assert_eq!(answer.as_deref(), Some("a prose answer"));
    }

    #[test]
    fn a_row_without_a_link_is_skipped_and_url_falls_back_for_title() {
        let rows = vec![
            json!({ "title": "no link" }),
            json!({ "url": "https://only" }),
        ];
        let sources = to_sources(&rows);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].title, "https://only");
    }

    #[test]
    fn an_arg_error_is_retryable_but_a_server_error_is_not() {
        assert!(looks_like_arg_error(&SearchProviderError::with_status(
            SearchProviderId::Zai,
            "invalid search_query",
            400
        )));
        assert!(!looks_like_arg_error(&SearchProviderError::with_status(
            SearchProviderId::Zai,
            "internal error",
            500
        )));
    }
}
