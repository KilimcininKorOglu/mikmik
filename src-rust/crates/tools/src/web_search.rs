// WebSearch tool that queries SearXNG, the Brave Search API, or DuckDuckGo depending on which backend is configured.
//
// Mirrors the TypeScript WebSearch tool behaviour:
// - Accepts a query string
// - Returns a list of results with title, url, and snippet
// - Falls back to DuckDuckGo if no search API key is configured
//
// A backend is only tried when its own configuration is present; SearXNG is
// never probed on a guessed address, because whatever answers that port would
// receive the user's query.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

/// Largest `num_results` the tool honours. Brave documents 20 as the ceiling of
/// its `count` parameter, and the other two backends are cut with `take`.
const MAX_NUM_RESULTS: usize = 20;

pub struct WebSearchTool;

#[derive(Debug, Deserialize)]
struct WebSearchInput {
    query: String,
    #[serde(default = "default_num_results")]
    num_results: usize,
}

fn default_num_results() -> usize {
    5
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_WEB_SEARCH
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns a list of relevant web pages with \
         titles, URLs, and snippets. Use this when you need current information \
         not available in your training data, or when searching for documentation, \
         examples, or news."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "num_results": {
                    "type": "number",
                    "description": "Number of results to return (default: 5, max: 20)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: WebSearchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let num_results = params.num_results.clamp(1, MAX_NUM_RESULTS);
        debug!(query = %params.query, num_results, "Web search");

        let brave_key = std::env::var("BRAVE_SEARCH_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());

        // SearXNG is only tried when the user named an instance. Its failure
        // hands over to the next backend only if the operator asked for that.
        if let Some(base) = searxng_base_url(ctx.config.searxng_url.as_deref()) {
            let error = match search_searxng(&params.query, num_results, &base).await {
                Ok(result) => return result,
                Err(e) => e,
            };
            match after_searxng_failure(error, ctx.config.web_search_fallback, brave_key) {
                NextBackend::Stop(error) => ToolResult::error(error),
                NextBackend::Brave(api_key) => {
                    warn!("SearXNG unreachable, falling back to Brave Search");
                    let result = search_brave(&params.query, num_results, &api_key).await;
                    label_fallback("Brave Search", result)
                }
                NextBackend::DuckDuckGo => {
                    warn!("SearXNG unreachable, falling back to DuckDuckGo");
                    let result = search_duckduckgo(&params.query, num_results).await;
                    label_fallback("DuckDuckGo", result)
                }
            }
        } else if let Some(api_key) = brave_key {
            search_brave(&params.query, num_results, &api_key).await
        } else {
            search_duckduckgo(&params.query, num_results).await
        }
    }
}

/// The SearXNG instance to query, or `None` when the user named none.
///
/// `settings.json` wins over the environment, matching how `config.api_key`
/// outranks `ANTHROPIC_API_KEY`. No address is guessed when both are absent.
fn searxng_base_url(configured: Option<&str>) -> Option<String> {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("SEARXNG_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}

/// What the tool does after a SearXNG request fails.
#[derive(Debug, PartialEq, Eq)]
enum NextBackend {
    /// Report the SearXNG failure and search nothing else.
    Stop(String),
    Brave(String),
    DuckDuckGo,
}

/// Decides whether a SearXNG failure ends the search or hands over.
///
/// Split out from [`WebSearchTool::execute`] so the choice can be tested
/// without reaching the network.
fn after_searxng_failure(
    error: String,
    fallback_enabled: bool,
    brave_key: Option<String>,
) -> NextBackend {
    if !fallback_enabled {
        return NextBackend::Stop(format!(
            "{error}\n\nSet \"webSearchFallback\": true in settings.json to let \
             WebSearch continue with Brave Search or DuckDuckGo when SearXNG is down."
        ));
    }
    match brave_key {
        Some(key) => NextBackend::Brave(key),
        None => NextBackend::DuckDuckGo,
    }
}

/// Names the backend that took over, so a fallback is never silent.
fn label_fallback(backend: &str, mut result: ToolResult) -> ToolResult {
    result.content = format!(
        "SearXNG was unreachable; used {backend} instead.\n\n{}",
        result.content
    );
    result
}

/// Queries a SearXNG instance's JSON API at the given base origin.
///
/// Returns `Err` with a human-readable reason when the instance cannot answer,
/// so the caller can decide between reporting it and moving to another backend.
async fn search_searxng(query: &str, num_results: usize, base: &str) -> Result<ToolResult, String> {
    // A self-hosted SearXNG instance can be slow or unreachable; bound the
    // request so the tool can't hang the turn.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Could not build the SearXNG HTTP client: {}", e))?;
    let url = format!(
        "{}/search?q={}&format=json&safesearch=0",
        base.trim_end_matches('/'),
        urlencoding_simple(query)
    );

    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("SearXNG request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!(
            "SearXNG returned status {} (is JSON format enabled in settings.yml?)",
            resp.status().as_u16()
        ));
    }

    let data: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse SearXNG response: {}", e))?;

    Ok(ToolResult::success(format_searxng_results(
        &data,
        num_results,
    )))
}

fn format_searxng_results(data: &Value, max: usize) -> String {
    let mut output = String::new();
    if let Some(items) = data.get("results").and_then(|r| r.as_array()) {
        for (i, item) in items.iter().take(max).enumerate() {
            let title = item
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("(No title)");
            let url = item.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let snippet = item.get("content").and_then(|s| s.as_str()).unwrap_or("");
            output.push_str(&format!(
                "{}. **{}**\n   URL: {}\n   {}\n",
                i + 1,
                title,
                url,
                snippet
            ));
            // Which upstream engines surfaced a result is the one thing a
            // meta-search backend knows that the ranking does not convey.
            let engines: Vec<&str> = item
                .get("engines")
                .and_then(|e| e.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            if !engines.is_empty() {
                output.push_str(&format!("   [engines: {}]\n", engines.join(", ")));
            }
            output.push('\n');
        }
    }

    if output.is_empty() {
        "No results found.".to_string()
    } else {
        output
    }
}

/// Search using the Brave Search API.
async fn search_brave(query: &str, num_results: usize, api_key: &str) -> ToolResult {
    let client = reqwest::Client::new();
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding_simple(query),
        num_results
    );

    let resp = match client
        .get(&url)
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ToolResult::error(format!("Search request failed: {}", e)),
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return ToolResult::error(format!("Brave Search API returned status {}", status));
    }

    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Failed to parse response: {}", e)),
    };

    let results = format_brave_results(&data, num_results);
    ToolResult::success(results)
}

fn format_brave_results(data: &Value, max: usize) -> String {
    let mut output = String::new();
    let web_results = data
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array());

    if let Some(items) = web_results {
        for (i, item) in items.iter().take(max).enumerate() {
            let title = item
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("(No title)");
            let url = item.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let snippet = item
                .get("description")
                .and_then(|s| s.as_str())
                .unwrap_or("");

            output.push_str(&format!(
                "{}. **{}**\n   URL: {}\n   {}\n\n",
                i + 1,
                title,
                url,
                snippet
            ));
        }
    }

    if output.is_empty() {
        "No results found.".to_string()
    } else {
        output
    }
}

/// Fallback: DuckDuckGo Instant Answer API.
/// Note: this doesn't return full search results, only instant answers.
async fn search_duckduckgo(query: &str, num_results: usize) -> ToolResult {
    let client = reqwest::Client::new();
    let url = format!(
        "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
        urlencoding_simple(query)
    );

    let resp = match client
        .get(&url)
        .header("User-Agent", "MikMik/1.0")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ToolResult::error(format!("Search request failed: {}", e)),
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return ToolResult::error(format!("DuckDuckGo API returned status {}", status));
    }

    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Failed to parse response: {}", e)),
    };

    let output = format_ddg_results(&data, num_results);
    ToolResult::success(output)
}

fn format_ddg_results(data: &Value, max: usize) -> String {
    let mut output = String::new();
    let mut count = 0;

    // Abstract (main answer)
    if let Some(abstract_text) = data.get("Abstract").and_then(|a| a.as_str()) {
        if !abstract_text.is_empty() {
            let source = data
                .get("AbstractSource")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let url = data
                .get("AbstractURL")
                .and_then(|u| u.as_str())
                .unwrap_or("");
            output.push_str(&format!(
                "**{}**\n{}\nURL: {}\n\n",
                source, abstract_text, url
            ));
            count += 1;
        }
    }

    // Related topics
    if let Some(topics) = data.get("RelatedTopics").and_then(|t| t.as_array()) {
        for topic in topics.iter().take(max.saturating_sub(count)) {
            if let Some(text) = topic.get("Text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    let url = topic.get("FirstURL").and_then(|u| u.as_str()).unwrap_or("");
                    output.push_str(&format!("- {}\n  {}\n\n", text, url));
                }
            }
        }
    }

    if output.is_empty() {
        format!(
            "No instant answer found for '{}'. For full web search point SEARXNG_URL \
             at a SearXNG instance, or set BRAVE_SEARCH_API_KEY for the Brave Search API.",
            data.get("QuerySearchQuery")
                .and_then(|q| q.as_str())
                .unwrap_or("your query")
        )
    } else {
        output
    }
}

/// Minimal percent-encoding for URL query parameters.
fn urlencoding_simple(s: &str) -> String {
    let mut encoded = String::new();
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                encoded.push(ch);
            }
            ' ' => encoded.push('+'),
            _ => {
                for byte in ch.to_string().as_bytes() {
                    encoded.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use mikmik_core::config::Config;
    use mikmik_core::permissions::AutoPermissionHandler;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// A response shaped like the one `searx/webutils.py::get_json_response`
    /// builds: seven top-level keys, no `number_of_results`.
    fn searxng_body() -> Value {
        json!({
            "query": "rust ownership",
            "results": [
                {
                    "title": "Ownership",
                    "url": "https://doc.rust-lang.org/book/ch04-01.html",
                    "content": "Ownership is a set of rules.",
                    "engines": ["google", "duckduckgo"],
                    "score": 4.5
                },
                {
                    "title": "Borrowing",
                    "url": "https://doc.rust-lang.org/book/ch04-02.html",
                    "content": "References and borrowing.",
                    "engines": [],
                    "score": 1.0
                }
            ],
            "answers": [],
            "corrections": [],
            "infoboxes": [],
            "suggestions": [],
            "unresponsive_engines": []
        })
    }

    #[test]
    fn searxng_results_carry_their_engine_attribution() {
        let output = format_searxng_results(&searxng_body(), 20);

        assert!(output.contains("1. **Ownership**"));
        assert!(output.contains("URL: https://doc.rust-lang.org/book/ch04-01.html"));
        assert!(output.contains("Ownership is a set of rules."));
        assert!(output.contains("[engines: google, duckduckgo]"));
    }

    #[test]
    fn a_result_without_engines_gets_no_attribution_line() {
        let output = format_searxng_results(&searxng_body(), 20);
        let borrowing = output
            .split("2. **Borrowing**")
            .nth(1)
            .expect("second result");

        assert!(!borrowing.contains("[engines:"));
    }

    #[test]
    fn searxng_formatting_honours_the_result_cap() {
        let output = format_searxng_results(&searxng_body(), 1);

        assert!(output.contains("Ownership"));
        assert!(!output.contains("Borrowing"));
    }

    #[test]
    fn an_empty_or_absent_result_list_reads_as_no_results() {
        let empty = json!({ "query": "x", "results": [] });
        let absent = json!({ "query": "x" });

        assert_eq!(format_searxng_results(&empty, 20), "No results found.");
        assert_eq!(format_searxng_results(&absent, 20), "No results found.");
    }

    #[test]
    fn a_searxng_failure_stops_the_search_while_fallback_is_off() {
        let next = after_searxng_failure(
            "SearXNG request failed".to_string(),
            false,
            Some("brave-key".to_string()),
        );

        match next {
            NextBackend::Stop(message) => {
                assert!(message.contains("SearXNG request failed"));
                assert!(message.contains("webSearchFallback"));
            }
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[test]
    fn fallback_prefers_brave_when_its_key_is_present() {
        let next = after_searxng_failure("down".to_string(), true, Some("brave-key".to_string()));

        assert_eq!(next, NextBackend::Brave("brave-key".to_string()));
    }

    #[test]
    fn fallback_lands_on_duckduckgo_without_a_brave_key() {
        let next = after_searxng_failure("down".to_string(), true, None);

        assert_eq!(next, NextBackend::DuckDuckGo);
    }

    #[test]
    fn a_fallback_result_names_the_backend_that_took_over() {
        let labelled = label_fallback("DuckDuckGo", ToolResult::success("body"));

        assert!(labelled
            .content
            .starts_with("SearXNG was unreachable; used DuckDuckGo instead."));
        assert!(labelled.content.contains("body"));
    }

    /// `SEARXNG_URL` is process-global, so tests that set it cannot run side by
    /// side. Async-aware because the guard spans the tool call's `await`.
    static SEARXNG_URL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct EnvGuard {
        key: &'static str,
        saved: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let saved = std::env::var_os(key);
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
            Self { key, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn a_configured_address_outranks_the_environment() {
        let _lock = SEARXNG_URL_LOCK.blocking_lock();
        let _env = EnvGuard::set("SEARXNG_URL", Some("http://from-env"));

        assert_eq!(
            searxng_base_url(Some("http://from-settings")).as_deref(),
            Some("http://from-settings")
        );
    }

    #[test]
    fn a_blank_setting_falls_through_to_the_environment() {
        let _lock = SEARXNG_URL_LOCK.blocking_lock();
        let _env = EnvGuard::set("SEARXNG_URL", Some("http://from-env"));

        assert_eq!(
            searxng_base_url(Some("   ")).as_deref(),
            Some("http://from-env")
        );
        assert_eq!(searxng_base_url(None).as_deref(), Some("http://from-env"));
    }

    #[test]
    fn no_address_anywhere_means_no_searxng() {
        let _lock = SEARXNG_URL_LOCK.blocking_lock();
        let _env = EnvGuard::set("SEARXNG_URL", None);

        assert_eq!(searxng_base_url(None), None);
        assert_eq!(searxng_base_url(Some("")), None);
    }

    fn ctx_with_fallback(fallback: bool) -> ToolContext {
        let config = Config {
            web_search_fallback: fallback,
            ..Default::default()
        };
        ToolContext {
            working_dir: std::path::PathBuf::from("."),
            permission_mode: mikmik_core::config::PermissionMode::Default,
            permission_handler: Arc::new(AutoPermissionHandler {
                mode: mikmik_core::config::PermissionMode::Default,
            }),
            cost_tracker: mikmik_core::cost::CostTracker::new(),
            session_id: "test-web-search".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_history::FileHistory::new(),
            )),
            file_snapshots: Arc::new(parking_lot::Mutex::new(
                mikmik_core::file_snapshot::FileSnapshotStore::new(),
            )),
            current_turn: Arc::new(AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config,
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            plan_approval_tx: None,
            tool_output_tx: None,
            plan_mode_tx: None,
            advisor_note_tx: None,
            advisor_name: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            current_call: None,
            editor: None,
            inbox: Default::default(),
        }
    }

    /// Port 1 refuses the connection locally, so this never leaves the machine.
    #[tokio::test]
    async fn an_unreachable_searxng_reports_the_failure_with_fallback_off() {
        let _lock = SEARXNG_URL_LOCK.lock().await;
        let _searxng = EnvGuard::set("SEARXNG_URL", Some("http://127.0.0.1:1"));
        let _brave = EnvGuard::set("BRAVE_SEARCH_API_KEY", None);

        let result = WebSearchTool
            .execute(
                json!({ "query": "rust ownership" }),
                &ctx_with_fallback(false),
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("SearXNG request failed"));
        assert!(result.content.contains("webSearchFallback"));
    }

    /// Answers one request with `body` and hangs up. Bound to a loopback port,
    /// so nothing here leaves the machine.
    fn spawn_searxng_stub(status_line: &'static str, body: String) -> String {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();

        std::thread::spawn(move || {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut scratch = [0u8; 2048];
            let _ = stream.read(&mut scratch);
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });

        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn a_reachable_searxng_answers_the_search() {
        let _lock = SEARXNG_URL_LOCK.lock().await;
        let base = spawn_searxng_stub("200 OK", searxng_body().to_string());
        let _searxng = EnvGuard::set("SEARXNG_URL", Some(&base));

        let result = WebSearchTool
            .execute(
                json!({ "query": "rust ownership" }),
                &ctx_with_fallback(false),
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("1. **Ownership**"));
        assert!(result.content.contains("[engines: google, duckduckgo]"));
    }

    #[tokio::test]
    async fn a_searxng_instance_refusing_json_says_so() {
        let _lock = SEARXNG_URL_LOCK.lock().await;
        let base = spawn_searxng_stub("403 Forbidden", "forbidden".to_string());
        let _searxng = EnvGuard::set("SEARXNG_URL", Some(&base));

        let result = WebSearchTool
            .execute(
                json!({ "query": "rust ownership" }),
                &ctx_with_fallback(false),
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("SearXNG returned status 403"));
        assert!(result.content.contains("settings.yml"));
    }
}
