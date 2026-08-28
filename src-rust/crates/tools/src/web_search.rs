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
    /// How recent a result has to be, as `day`, `week`, `month` or `year`.
    #[serde(default)]
    recency: Option<String>,
}

fn default_num_results() -> usize {
    5
}

/// How far back a search may reach.
///
/// One shape the tool understands, mapped to each backend's own parameter, so
/// the model names a window once and every backend that has one honours it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    /// Parse the model's word, or say which words are allowed.
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            "year" => Ok(Self::Year),
            other => Err(format!(
                "recency must be one of day, week, month or year, not {other:?}"
            )),
        }
    }

    /// Brave's `freshness` code.
    fn brave_freshness(self) -> &'static str {
        match self {
            Self::Day => "pd",
            Self::Week => "pw",
            Self::Month => "pm",
            Self::Year => "py",
        }
    }

    /// SearXNG's `time_range` value, which is the same word the model gave.
    fn searxng_time_range(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }

    /// Tavily's `time_range` value, which is the same word the model gave.
    fn tavily_time_range(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
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
                },
                "recency": {
                    "type": "string",
                    "enum": ["day", "week", "month", "year"],
                    "description": "Only return results from within this window. Honoured by SearXNG, Tavily and Brave; DuckDuckGo ignores it and says so."
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
        let recency = match params.recency.as_deref().map(Recency::parse).transpose() {
            Ok(recency) => recency,
            Err(error) => return ToolResult::error(error),
        };
        debug!(query = %params.query, num_results, ?recency, "Web search");

        let tavily_key = std::env::var("TAVILY_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());
        let brave_key = std::env::var("BRAVE_SEARCH_API_KEY")
            .ok()
            .filter(|k| !k.is_empty());

        // SearXNG is only tried when the user named an instance. Its failure
        // hands over to the next backend only if the operator asked for that.
        if let Some(base) = searxng_base_url(ctx.config.searxng_url.as_deref()) {
            let error = match search_searxng(&params.query, num_results, &base, recency).await {
                Ok(result) => return result,
                Err(e) => e,
            };
            match after_searxng_failure(
                error,
                ctx.config.web_search_fallback,
                tavily_key,
                brave_key,
            ) {
                NextBackend::Stop(error) => ToolResult::error(error),
                NextBackend::Tavily(api_key) => {
                    warn!("SearXNG unreachable, falling back to Tavily");
                    let result = search_tavily(&params.query, num_results, &api_key, recency).await;
                    label_fallback("Tavily", result)
                }
                NextBackend::Brave(api_key) => {
                    warn!("SearXNG unreachable, falling back to Brave Search");
                    let result = search_brave(&params.query, num_results, &api_key, recency).await;
                    label_fallback("Brave Search", result)
                }
                NextBackend::DuckDuckGo => {
                    warn!("SearXNG unreachable, falling back to DuckDuckGo");
                    let result = search_duckduckgo(&params.query, num_results, recency).await;
                    label_fallback("DuckDuckGo", result)
                }
            }
        } else if let Some(api_key) = tavily_key {
            search_tavily(&params.query, num_results, &api_key, recency).await
        } else if let Some(api_key) = brave_key {
            search_brave(&params.query, num_results, &api_key, recency).await
        } else {
            search_duckduckgo(&params.query, num_results, recency).await
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
    Tavily(String),
    Brave(String),
    DuckDuckGo,
}

/// Decides whether a SearXNG failure ends the search or hands over.
///
/// Split out from [`WebSearchTool::execute`] so the choice can be tested
/// without reaching the network. Tavily is preferred over Brave when both
/// keys are present: it is a dedicated search API that returns full results,
/// so it is the closer stand-in for the SearXNG that just failed.
fn after_searxng_failure(
    error: String,
    fallback_enabled: bool,
    tavily_key: Option<String>,
    brave_key: Option<String>,
) -> NextBackend {
    if !fallback_enabled {
        return NextBackend::Stop(format!(
            "{error}\n\nSet \"webSearchFallback\": true in settings.json to let \
             WebSearch continue with Tavily, Brave Search or DuckDuckGo when SearXNG is down."
        ));
    }
    match (tavily_key, brave_key) {
        (Some(key), _) => NextBackend::Tavily(key),
        (None, Some(key)) => NextBackend::Brave(key),
        (None, None) => NextBackend::DuckDuckGo,
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
async fn search_searxng(
    query: &str,
    num_results: usize,
    base: &str,
    recency: Option<Recency>,
) -> Result<ToolResult, String> {
    // A self-hosted SearXNG instance can be slow or unreachable; bound the
    // request so the tool can't hang the turn.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Could not build the SearXNG HTTP client: {}", e))?;
    let url = searxng_url(base, query, recency);

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
async fn search_brave(
    query: &str,
    num_results: usize,
    api_key: &str,
    recency: Option<Recency>,
) -> ToolResult {
    let client = reqwest::Client::new();
    let url = brave_url(query, num_results, recency);

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

/// Search using the Tavily Search API.
async fn search_tavily(
    query: &str,
    num_results: usize,
    api_key: &str,
    recency: Option<Recency>,
) -> ToolResult {
    let client = reqwest::Client::new();
    let body = tavily_request_body(query, num_results, recency);

    let resp = match client
        .post("https://api.tavily.com/search")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ToolResult::error(format!("Search request failed: {}", e)),
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return ToolResult::error(format!("Tavily API returned status {}", status));
    }

    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Failed to parse response: {}", e)),
    };

    ToolResult::success(format_tavily_results(&data, num_results))
}

/// The Tavily request body, carrying `time_range` when a window was asked for.
///
/// A pure builder so the payload can be asserted without reaching the network.
fn tavily_request_body(query: &str, num_results: usize, recency: Option<Recency>) -> Value {
    let mut body = json!({
        "query": query,
        "max_results": num_results,
    });
    if let Some(recency) = recency {
        body["time_range"] = json!(recency.tavily_time_range());
    }
    body
}

fn format_tavily_results(data: &Value, max: usize) -> String {
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
async fn search_duckduckgo(
    query: &str,
    num_results: usize,
    recency: Option<Recency>,
) -> ToolResult {
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

    let output = ddg_recency_note(format_ddg_results(&data, num_results), recency);
    ToolResult::success(output)
}

/// The SearXNG search URL, carrying `time_range` when a window was asked for.
fn searxng_url(base: &str, query: &str, recency: Option<Recency>) -> String {
    let time_range = recency
        .map(|recency| format!("&time_range={}", recency.searxng_time_range()))
        .unwrap_or_default();
    format!(
        "{}/search?q={}&format=json&safesearch=0{time_range}",
        base.trim_end_matches('/'),
        urlencoding_simple(query)
    )
}

/// The Brave search URL, carrying `freshness` when a window was asked for.
fn brave_url(query: &str, num_results: usize, recency: Option<Recency>) -> String {
    let freshness = recency
        .map(|recency| format!("&freshness={}", recency.brave_freshness()))
        .unwrap_or_default();
    format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}{freshness}",
        urlencoding_simple(query),
        num_results
    )
}

/// Prefix a note when DuckDuckGo could not honour a recency the model asked
/// for, because its Instant Answer API has no recency parameter.
fn ddg_recency_note(output: String, recency: Option<Recency>) -> String {
    match recency {
        Some(_) => format!(
            "Note: DuckDuckGo's Instant Answer API has no recency filter, so the results below \
             are not limited to the window you asked for.\n\n{output}"
        ),
        None => output,
    }
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
    fn recency_parses_the_four_words_and_rejects_the_rest() {
        assert_eq!(Recency::parse("day"), Ok(Recency::Day));
        assert_eq!(Recency::parse("week"), Ok(Recency::Week));
        assert_eq!(Recency::parse("month"), Ok(Recency::Month));
        assert_eq!(Recency::parse("year"), Ok(Recency::Year));
        assert!(Recency::parse("hour").is_err());
        assert!(Recency::parse("").is_err());
    }

    #[test]
    fn each_backend_gets_its_own_recency_word() {
        // The three backends name the same window differently; a wrong mapping
        // sends a code the backend does not understand and silently drops the
        // filter.
        assert_eq!(Recency::Day.brave_freshness(), "pd");
        assert_eq!(Recency::Week.brave_freshness(), "pw");
        assert_eq!(Recency::Month.brave_freshness(), "pm");
        assert_eq!(Recency::Year.brave_freshness(), "py");
        assert_eq!(Recency::Month.searxng_time_range(), "month");
        assert_eq!(Recency::Week.tavily_time_range(), "week");
    }

    #[test]
    fn a_tavily_body_carries_the_window_and_result_cap() {
        // A wrong or missing time_range sends an unfiltered request while the
        // model asked for a window; a wrong max_results ignores the cap.
        let windowed = tavily_request_body("rust", 7, Some(Recency::Month));
        assert_eq!(windowed["time_range"], json!("month"));
        assert_eq!(windowed["max_results"], json!(7));
        assert_eq!(windowed["query"], json!("rust"));

        let plain = tavily_request_body("rust", 5, None);
        assert!(plain.get("time_range").is_none(), "{plain}");
    }

    #[test]
    fn tavily_results_render_title_url_and_snippet() {
        let body = json!({
            "results": [
                {
                    "title": "Ownership",
                    "url": "https://doc.rust-lang.org/book/ch04-01.html",
                    "content": "Ownership is a set of rules.",
                    "score": 0.98
                }
            ]
        });
        let output = format_tavily_results(&body, 20);

        assert!(output.contains("1. **Ownership**"), "{output}");
        assert!(
            output.contains("URL: https://doc.rust-lang.org/book/ch04-01.html"),
            "{output}"
        );
        assert!(output.contains("Ownership is a set of rules."), "{output}");
    }

    #[test]
    fn tavily_formatting_honours_the_result_cap_and_empty_case() {
        let body = json!({
            "results": [
                { "title": "First", "url": "https://a", "content": "one" },
                { "title": "Second", "url": "https://b", "content": "two" }
            ]
        });
        let capped = format_tavily_results(&body, 1);
        assert!(capped.contains("First"), "{capped}");
        assert!(!capped.contains("Second"), "{capped}");

        let empty = json!({ "results": [] });
        assert_eq!(format_tavily_results(&empty, 20), "No results found.");
    }

    #[test]
    fn a_recency_reaches_the_searxng_and_brave_urls() {
        let searxng = searxng_url("http://searx.example", "rust", Some(Recency::Week));
        assert!(searxng.contains("&time_range=week"), "{searxng}");

        let brave = brave_url("rust", 5, Some(Recency::Day));
        assert!(brave.contains("&freshness=pd"), "{brave}");
    }

    #[test]
    fn no_recency_leaves_the_urls_without_a_window() {
        let searxng = searxng_url("http://searx.example", "rust", None);
        assert!(!searxng.contains("time_range"), "{searxng}");

        let brave = brave_url("rust", 5, None);
        assert!(!brave.contains("freshness"), "{brave}");
    }

    #[test]
    fn duckduckgo_says_it_could_not_honour_a_recency() {
        // DuckDuckGo's Instant Answer API has no recency parameter, so a window
        // the model asked for was dropped; the note keeps that from reading as
        // an applied filter.
        let noted = ddg_recency_note("body".to_string(), Some(Recency::Year));
        assert!(noted.contains("no recency filter"), "{noted}");
        assert!(noted.contains("body"), "{noted}");

        let plain = ddg_recency_note("body".to_string(), None);
        assert_eq!(plain, "body");
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
            None,
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
    fn fallback_prefers_tavily_over_brave_when_both_keys_present() {
        // Tavily is a dedicated search API returning full results, so it is the
        // closer stand-in for SearXNG than Brave; wired the wrong way the paid
        // Tavily key would sit idle behind Brave.
        let next = after_searxng_failure(
            "down".to_string(),
            true,
            Some("tavily-key".to_string()),
            Some("brave-key".to_string()),
        );

        assert_eq!(next, NextBackend::Tavily("tavily-key".to_string()));
    }

    #[test]
    fn fallback_uses_tavily_when_only_its_key_is_present() {
        let next = after_searxng_failure(
            "down".to_string(),
            true,
            Some("tavily-key".to_string()),
            None,
        );

        assert_eq!(next, NextBackend::Tavily("tavily-key".to_string()));
    }

    #[test]
    fn fallback_prefers_brave_when_its_key_is_present() {
        let next = after_searxng_failure(
            "down".to_string(),
            true,
            None,
            Some("brave-key".to_string()),
        );

        assert_eq!(next, NextBackend::Brave("brave-key".to_string()));
    }

    #[test]
    fn fallback_lands_on_duckduckgo_without_a_brave_key() {
        let next = after_searxng_failure("down".to_string(), true, None, None);

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
