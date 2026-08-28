// WebFetch tool: HTTP GET with HTML-to-text conversion and LLM-powered semantic extraction
// for edge cases (JS-heavy pages, minimal content).

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, warn};

pub struct WebFetchTool;

#[derive(Debug, Deserialize)]
struct WebFetchInput {
    url: String,
    #[serde(default)]
    #[allow(dead_code)]
    prompt: Option<String>,
}

/// Rewrite a GitHub file page into the address that serves the file itself.
///
/// `github.com/o/r/blob/main/src/lib.rs` answers an HTML page that wraps the
/// file in navigation chrome, and this tool's tag stripping then hands the
/// model the chrome along with the code. `raw.githubusercontent.com` answers
/// the bytes.
///
/// Everything after `blob` moves across untouched, because a ref may itself
/// hold slashes (`refs/heads/feature/x`) and both hosts lay the ref and the
/// path out the same way, so nothing has to decide where one ends.
///
/// The query and the fragment are dropped: `?plain=1` and `#L10-L20` mean
/// something to the page and nothing to the raw host.
///
/// Returns `None` for every other address, including a host that merely ends
/// in `github.com`, because the host is compared whole rather than by suffix.
fn github_blob_to_raw(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if !matches!(parsed.host_str()?, "github.com" | "www.github.com") {
        return None;
    }

    let segments: Vec<&str> = parsed.path_segments()?.collect();
    // owner / repo / blob / ref-and-path...
    if segments.len() < 5 || segments[2] != "blob" {
        return None;
    }
    if segments[3..].iter().any(|s| s.is_empty()) {
        return None;
    }

    Some(format!(
        "https://raw.githubusercontent.com/{}/{}/{}",
        segments[0],
        segments[1],
        segments[3..].join("/")
    ))
}

/// Compute a simple hash of the URL for cache purposes.
fn url_hash(url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Get the cache directory for web_fetch content.
fn get_cache_dir() -> PathBuf {
    mikmik_core::config::Settings::config_dir().join("web_cache")
}

/// Attempt to load cached extracted content for a URL.
fn load_cached_extraction(url: &str) -> Option<String> {
    let cache_dir = get_cache_dir();
    let cache_file = cache_dir.join(format!("{}.txt", url_hash(url)));

    if cache_file.exists() {
        match fs::read_to_string(&cache_file) {
            Ok(content) => {
                debug!(file = ?cache_file, "Loaded cached web content");
                return Some(content);
            }
            Err(e) => {
                debug!(file = ?cache_file, error = %e, "Failed to load cache");
            }
        }
    }
    None
}

/// Save extracted content to cache.
fn save_cached_extraction(url: &str, content: &str) {
    let cache_dir = get_cache_dir();
    if let Err(e) = fs::create_dir_all(&cache_dir) {
        warn!(dir = ?cache_dir, error = %e, "Failed to create cache directory");
        return;
    }

    let cache_file = cache_dir.join(format!("{}.txt", url_hash(url)));
    if let Err(e) = fs::write(&cache_file, content) {
        warn!(file = ?cache_file, error = %e, "Failed to write cache file");
    } else {
        debug!(file = ?cache_file, "Cached extracted web content");
    }
}

/// Detect if HTML is likely a JS-heavy page with minimal semantic content.
fn is_edge_case_html(html: &str, extracted_text: &str) -> bool {
    // Check word count (rough estimate)
    let word_count = extracted_text.split_whitespace().count();
    if word_count < 100 {
        debug!(word_count, "Edge case: low word count");
        return true;
    }

    // Check for semantic HTML tags
    let lower = html.to_lowercase();
    let has_semantic =
        lower.contains("<article") || lower.contains("<main") || lower.contains("<body");

    if !has_semantic {
        debug!("Edge case: no semantic HTML tags");
        return true;
    }

    false
}

/// Ask a small model to pull the readable text out of a page.
///
/// Through the session's own account, on whatever small model that account
/// serves. It used to build an Anthropic client and ask for a hardcoded
/// `claude-haiku-4-5`, so a session on any other provider never reached this
/// path at all and always fell back to tag stripping.
async fn semantic_extraction(html: &str, ctx: &ToolContext) -> Option<String> {
    let route =
        mikmik_api::resolve_small_model_route(&ctx.config, &mikmik_api::ModelRegistry::new());

    let provider = match mikmik_api::provider_for_account(&ctx.config, &route.account).await {
        Ok(provider) => provider,
        Err(e) => {
            warn!(account = %route.account, error = %e, "No provider for semantic extraction");
            return None;
        }
    };

    // Truncate HTML to avoid exceeding token limits
    let html_excerpt = if html.len() > 20000 {
        format!("{}...", &html[..20000])
    } else {
        html.to_string()
    };

    let system = "You are a content extraction expert. Given HTML, extract and return only the main text content. Return just plain text, no markdown or formatting.";

    let request = mikmik_api::ProviderRequest {
        model: route.model.clone(),
        messages: vec![mikmik_core::types::Message::user(format!(
            "Extract the main content from this HTML and return only the text:\n\n{}",
            html_excerpt
        ))],
        system_prompt: Some(mikmik_api::SystemPrompt::Text(system.to_string())),
        // No tools: this call reads a page, it does not act on one.
        tools: Vec::new(),
        max_tokens: 2000,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: Vec::new(),
        thinking: None,
        provider_options: serde_json::Value::Object(Default::default()),
    };

    match provider.create_message(request).await {
        Ok(response) => {
            let extracted: String = response
                .content
                .iter()
                .filter_map(|block| match block {
                    mikmik_core::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();

            if extracted.is_empty() {
                warn!("No text block in semantic extraction response");
                return None;
            }

            debug!(
                extracted_len = extracted.len(),
                "Semantic extraction successful"
            );
            Some(extracted)
        }
        Err(e) => {
            warn!(error = %e, "Semantic extraction API call failed");
            None
        }
    }
}

/// Naively strip HTML tags and decode common entities.
fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;

    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if !in_tag && chars[i] == '<' {
            in_tag = true;
            // Check for script/style open/close
            let rest: String = lower_chars[i..].iter().take(20).collect();
            if rest.starts_with("<script") {
                in_script = true;
            } else if rest.starts_with("</script") {
                in_script = false;
            } else if rest.starts_with("<style") {
                in_style = true;
            } else if rest.starts_with("</style") {
                in_style = false;
            }
            // Block tags => newline
            let block_tags = [
                "<br", "<p ", "<p>", "</p>", "<div", "</div>", "<h1", "<h2", "<h3", "<h4", "<h5",
                "<h6", "</h1", "</h2", "</h3", "</h4", "</h5", "</h6", "<li", "</li", "<tr",
                "</tr", "<hr",
            ];
            for tag in &block_tags {
                if rest.starts_with(tag) {
                    result.push('\n');
                    break;
                }
            }
            i += 1;
            continue;
        }

        if in_tag {
            if chars[i] == '>' {
                in_tag = false;
            }
            i += 1;
            continue;
        }

        if in_script || in_style {
            i += 1;
            continue;
        }

        // Decode basic entities
        if chars[i] == '&' {
            let rest: String = chars[i..].iter().take(10).collect();
            if rest.starts_with("&amp;") {
                result.push('&');
                i += 5;
            } else if rest.starts_with("&lt;") {
                result.push('<');
                i += 4;
            } else if rest.starts_with("&gt;") {
                result.push('>');
                i += 4;
            } else if rest.starts_with("&quot;") {
                result.push('"');
                i += 6;
            } else if rest.starts_with("&#39;") || rest.starts_with("&apos;") {
                result.push('\'');
                i += if rest.starts_with("&#39;") { 5 } else { 6 };
            } else if rest.starts_with("&nbsp;") {
                result.push(' ');
                i += 6;
            } else {
                result.push('&');
                i += 1;
            }
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    // Collapse multiple blank lines
    let mut collapsed = String::new();
    let mut blank_count = 0;
    for line in result.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                collapsed.push('\n');
            }
        } else {
            blank_count = 0;
            collapsed.push_str(trimmed);
            collapsed.push('\n');
        }
    }

    collapsed.trim().to_string()
}

#[async_trait]
impl Tool for WebFetchTool {
    // Gates itself: calls `ctx.check_permission` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_WEB_FETCH
    }

    fn description(&self) -> &str {
        "Fetches a web page URL and returns its content as text. HTML is \
         automatically converted to plain text. Use this for reading documentation, \
         APIs, and other web resources."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "prompt": {
                    "type": "string",
                    "description": "Optional prompt for how to process the content"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let mut params: WebFetchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // Rewritten once, here, so the permission prompt, the cache key and
        // every error message name the address that is actually fetched.
        if let Some(raw) = github_blob_to_raw(&params.url) {
            debug!(from = %params.url, to = %raw, "Rewrote a GitHub file page to its raw address");
            params.url = raw;
        }

        // Permission check
        if let Err(e) = ctx.check_permission(
            self.name(),
            &format!("Fetch {}", params.url),
            true, // read-only
        ) {
            return ToolResult::error(e.to_string());
        }

        debug!(url = %params.url, "Fetching web page");

        // Site-aware handlers first: a recognized host (npm, package registries,
        // security DBs, …) renders structured markdown from its API instead of
        // the generic HTML strip. An unclaimed URL falls through unchanged.
        if let Some(result) =
            crate::web::scrapers::dispatch(&params.url, std::time::Duration::from_secs(30)).await
        {
            let mut body = result.content;
            for note in &result.notes {
                debug!(note = %note, url = %params.url, "scraper note");
            }
            if result.truncated {
                body.push_str("\n\n... (truncated)");
            }
            return ToolResult::success(body);
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to create HTTP client: {}", e)),
        };

        let resp = match client
            .get(&params.url)
            .header("User-Agent", "Claude-Code/1.0")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("Failed to fetch {}: {}", params.url, e)),
        };

        let status = resp.status();
        if !status.is_success() {
            return ToolResult::error(format!("HTTP {} when fetching {}", status, params.url));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(format!("Failed to read response body: {}", e)),
        };

        // Try to load from cache first
        if let Some(cached) = load_cached_extraction(&params.url) {
            return ToolResult::success(cached);
        }

        // Convert HTML to text if applicable
        let mut text = if content_type.contains("html") {
            strip_html(&body)
        } else {
            body.clone()
        };

        // Detect and handle edge cases with semantic extraction
        if content_type.contains("html") && is_edge_case_html(&body, &text) {
            debug!(url = %params.url, "Attempting semantic extraction for edge case");
            if let Some(extracted) = semantic_extraction(&body, ctx).await {
                text = extracted;
            } else {
                debug!("Semantic extraction failed, using basic HTML stripping");
            }
        }

        // Truncate very long content
        const MAX_LEN: usize = 100_000;
        let text = if text.len() > MAX_LEN {
            format!(
                "{}\n\n... (truncated, {} total characters)",
                &text[..MAX_LEN],
                text.len()
            )
        } else {
            text
        };

        // Cache the final result
        save_cached_extraction(&params.url, &text);

        ToolResult::success(text)
    }
}

#[cfg(test)]
mod tests {
    use super::github_blob_to_raw;

    #[test]
    fn a_file_page_becomes_the_address_that_serves_the_file() {
        assert_eq!(
            github_blob_to_raw("https://github.com/can1357/oh-my-pi/blob/main/README.md"),
            Some("https://raw.githubusercontent.com/can1357/oh-my-pi/main/README.md".to_string())
        );
    }

    #[test]
    fn a_ref_that_holds_slashes_crosses_untouched() {
        assert_eq!(
            github_blob_to_raw("https://github.com/o/r/blob/refs/heads/feature/x/src/lib.rs"),
            Some(
                "https://raw.githubusercontent.com/o/r/refs/heads/feature/x/src/lib.rs".to_string()
            )
        );
    }

    #[test]
    fn the_query_and_the_fragment_are_dropped() {
        // `?plain=1` and `#L10-L20` address the rendered page. The raw host
        // serves bytes and would answer the same file either way, so carrying
        // them across only makes two cache keys for one file.
        assert_eq!(
            github_blob_to_raw("https://github.com/o/r/blob/main/a.rs?plain=1#L10-L20"),
            Some("https://raw.githubusercontent.com/o/r/main/a.rs".to_string())
        );
    }

    #[test]
    fn a_host_that_only_ends_in_github_com_is_left_alone() {
        // A suffix comparison would hand this attacker-controlled host the
        // rewrite and send the fetch somewhere the user never named.
        assert_eq!(
            github_blob_to_raw("https://evil-github.com/o/r/blob/main/a.rs"),
            None
        );
        assert_eq!(
            github_blob_to_raw("https://github.com.evil.example/o/r/blob/main/a.rs"),
            None
        );
    }

    #[test]
    fn a_directory_listing_and_a_repo_root_are_left_alone() {
        assert_eq!(
            github_blob_to_raw("https://github.com/o/r/tree/main/src"),
            None
        );
        assert_eq!(github_blob_to_raw("https://github.com/o/r"), None);
        assert_eq!(github_blob_to_raw("https://github.com/o/r/pull/1428"), None);
    }

    #[test]
    fn a_blob_deeper_in_the_tree_is_not_mistaken_for_the_marker() {
        // Only the third segment names the page kind. A directory called
        // `blob` inside the repository must not trigger the rewrite.
        assert_eq!(
            github_blob_to_raw("https://github.com/o/r/tree/main/blob/x.rs"),
            None
        );
        // ...and a file that really does live under `blob/` still rewrites.
        assert_eq!(
            github_blob_to_raw("https://github.com/o/r/blob/main/blob/x.rs"),
            Some("https://raw.githubusercontent.com/o/r/main/blob/x.rs".to_string())
        );
    }

    #[test]
    fn a_trailing_slash_leaves_no_empty_segment_behind() {
        assert_eq!(
            github_blob_to_raw("https://github.com/o/r/blob/main/"),
            None
        );
    }

    #[test]
    fn a_non_http_scheme_is_left_alone() {
        assert_eq!(
            github_blob_to_raw("ftp://github.com/o/r/blob/main/a.rs"),
            None
        );
        assert_eq!(github_blob_to_raw("not a url at all"), None);
    }
}
