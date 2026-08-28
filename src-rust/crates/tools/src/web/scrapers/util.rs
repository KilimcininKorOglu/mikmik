// Shared helpers for site-aware web-fetch handlers.
//
// Ported from oh-my-pi `web/scrapers/types.ts`: a `load_page` fetcher that
// rotates user agents on bot-block, honours a bounded `Retry-After`, and caps
// the body; plus `RenderResult` and the small formatting helpers the handlers
// build their markdown with.

use std::time::Duration;

/// Largest markdown payload a handler returns; longer output is truncated.
pub const MAX_OUTPUT_CHARS: usize = 500_000;

/// User agents tried in order; a bot-block on one falls through to the next.
const USER_AGENTS: [&str; 3] = [
    "curl/8.0",
    "Mozilla/5.0 (compatible; TextBot/1.0)",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
];

/// A rendered fetch result handed back to the web-fetch tool.
#[derive(Debug, Clone)]
pub struct RenderResult {
    pub url: String,
    pub final_url: String,
    pub content_type: String,
    pub method: String,
    pub content: String,
    pub truncated: bool,
    pub notes: Vec<String>,
}

/// Outcome of [`load_page`].
#[derive(Debug, Clone)]
pub struct LoadPageResult {
    pub content: String,
    pub content_type: String,
    pub final_url: String,
    pub ok: bool,
    pub status: Option<u16>,
    pub error: Option<String>,
}

/// True when a 403/503 body looks like a bot-block challenge.
fn is_bot_blocked(status: u16, content: &str) -> bool {
    if status != 403 && status != 503 {
        return false;
    }
    let lower = content.to_lowercase();
    [
        "cloudflare",
        "captcha",
        "challenge",
        "blocked",
        "access denied",
        "bot detection",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Parse a `Retry-After` header (seconds only) into a bounded delay.
fn parse_retry_after(value: Option<&str>) -> Duration {
    const MAX: u64 = 10;
    let seconds = value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(1)
        .min(MAX);
    Duration::from_secs(seconds)
}

/// Options for [`load_page`].
pub struct LoadOptions {
    pub timeout: Duration,
    pub method: reqwest::Method,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            method: reqwest::Method::GET,
            headers: Vec::new(),
            body: None,
        }
    }
}

/// Fetch a page, rotating user agents on a bot-block and retrying once on 429.
///
/// Never returns `Err`; transport failures land in [`LoadPageResult::error`]
/// with `ok = false`, so a handler decides whether to give up or move on.
pub async fn load_page(url: &str, options: LoadOptions) -> LoadPageResult {
    let mut last_error: Option<String> = None;
    let mut retried_429 = false;
    let mut attempt = 0usize;

    while attempt < USER_AGENTS.len() {
        let outcome = one_attempt(url, &options, USER_AGENTS[attempt]).await;
        match outcome {
            Attempt::Done(result) => return result,
            Attempt::RateLimited(delay) if !retried_429 => {
                retried_429 = true;
                tokio::time::sleep(delay).await;
                // Reuse the same user agent for the retry.
                continue;
            }
            Attempt::BotBlocked if attempt < USER_AGENTS.len() - 1 => {
                attempt += 1;
                continue;
            }
            Attempt::BotBlocked => return bot_block_result(url),
            Attempt::Error(message) => {
                last_error = Some(message);
                attempt += 1;
            }
            Attempt::RateLimited(_) => {
                // Already retried once; treat as a failure and move on.
                attempt += 1;
            }
        }
    }

    LoadPageResult {
        content: String::new(),
        content_type: String::new(),
        final_url: url.to_string(),
        ok: false,
        status: None,
        error: last_error,
    }
}

enum Attempt {
    Done(LoadPageResult),
    RateLimited(Duration),
    BotBlocked,
    Error(String),
}

async fn one_attempt(url: &str, options: &LoadOptions, user_agent: &str) -> Attempt {
    let client = match reqwest::Client::builder().timeout(options.timeout).build() {
        Ok(c) => c,
        Err(e) => return Attempt::Error(format!("HTTP client: {e}")),
    };
    let mut req = client
        .request(options.method.clone(), url)
        .header("User-Agent", user_agent)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "en-US,en;q=0.5")
        // Cloudflare's Markdown-for-Agents corrupts a compressed body.
        .header("Accept-Encoding", "identity");
    for (k, v) in &options.headers {
        req = req.header(k, v);
    }
    if let Some(body) = &options.body {
        req = req.body(body.clone());
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return Attempt::Error(e.to_string()),
    };
    let status = resp.status();
    if status.as_u16() == 429 {
        let delay = parse_retry_after(
            resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
        );
        return Attempt::RateLimited(delay);
    }
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or("").trim().to_lowercase())
        .unwrap_or_default();
    let final_url = resp.url().to_string();
    let content = match resp.text().await {
        Ok(c) => c,
        Err(e) => return Attempt::Error(e.to_string()),
    };

    if is_bot_blocked(status.as_u16(), &content) {
        return Attempt::BotBlocked;
    }
    Attempt::Done(LoadPageResult {
        content,
        content_type,
        final_url,
        ok: status.is_success(),
        status: Some(status.as_u16()),
        error: None,
    })
}

fn bot_block_result(url: &str) -> LoadPageResult {
    LoadPageResult {
        content: String::new(),
        content_type: String::new(),
        final_url: url.to_string(),
        ok: false,
        status: Some(403),
        error: Some("bot-blocked".to_string()),
    }
}

/// Collapse 3+ newlines, trim, and cap at [`MAX_OUTPUT_CHARS`].
pub fn finalize_output(content: &str) -> (String, bool) {
    let mut cleaned = String::with_capacity(content.len());
    let mut newline_run = 0usize;
    for ch in content.chars() {
        if ch == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                cleaned.push('\n');
            }
        } else {
            newline_run = 0;
            cleaned.push(ch);
        }
    }
    let cleaned = cleaned.trim().to_string();
    if cleaned.chars().count() > MAX_OUTPUT_CHARS {
        let capped: String = cleaned.chars().take(MAX_OUTPUT_CHARS).collect();
        (capped, true)
    } else {
        (cleaned, false)
    }
}

/// Build a [`RenderResult`] from markdown, applying [`finalize_output`].
pub fn build_result(md: &str, url: &str, method: &str, notes: Vec<String>) -> RenderResult {
    let (content, truncated) = finalize_output(md);
    RenderResult {
        url: url.to_string(),
        final_url: url.to_string(),
        content_type: "text/markdown".to_string(),
        method: method.to_string(),
        content,
        truncated,
        notes,
    }
}

/// Decode `%XX` escapes in a URL path/segment (UTF-8, lossy on bad bytes).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a value for use as one URL path/query component
/// (unreserved characters pass through; everything else becomes `%XX`).
pub fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Format a number with thousands separators (e.g. `1,234,567`).
pub fn format_number(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        // A comma precedes every group of three counted from the right.
        if i != 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Format a byte count as `B`/`KB`/`MB`/`GB` (1024-based, one decimal above 1K).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes}B")
    } else if b < MB {
        format!("{:.1}KB", b / KB)
    } else if b < GB {
        format!("{:.1}MB", b / MB)
    } else {
        format!("{:.1}GB", b / GB)
    }
}

/// Format a millisecond epoch as `YYYY-MM-DD`, or empty when out of range.
pub fn format_epoch_millis(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Format a date value as `YYYY-MM-DD`, or empty on unparseable input.
pub fn format_iso_date(value: &str) -> String {
    if let Some(prefix) = value.get(..10) {
        if chrono::NaiveDate::parse_from_str(prefix, "%Y-%m-%d").is_ok() {
            return prefix.to_string();
        }
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return dt.format("%Y-%m-%d").to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_block_only_fires_on_challenge_bodies() {
        assert!(is_bot_blocked(403, "Cloudflare says no"));
        assert!(is_bot_blocked(503, "please solve this CAPTCHA"));
        assert!(!is_bot_blocked(403, "plain forbidden"));
        assert!(!is_bot_blocked(200, "cloudflare"));
    }

    #[test]
    fn retry_after_is_parsed_and_bounded() {
        assert_eq!(parse_retry_after(Some("5")), Duration::from_secs(5));
        assert_eq!(parse_retry_after(Some("999")), Duration::from_secs(10));
        assert_eq!(parse_retry_after(None), Duration::from_secs(1));
        assert_eq!(parse_retry_after(Some("junk")), Duration::from_secs(1));
    }

    #[test]
    fn finalize_collapses_newlines_and_caps() {
        let (out, truncated) = finalize_output("a\n\n\n\nb\n\n\n c  ");
        assert_eq!(out, "a\n\nb\n\n c");
        assert!(!truncated);

        let long = "x".repeat(MAX_OUTPUT_CHARS + 50);
        let (capped, truncated) = finalize_output(&long);
        assert_eq!(capped.chars().count(), MAX_OUTPUT_CHARS);
        assert!(truncated);
    }

    #[test]
    fn number_formatting_groups_thousands() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1_234), "1,234");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }

    #[test]
    fn iso_date_extracts_or_reformats() {
        assert_eq!(format_iso_date("2024-06-30T12:00:00Z"), "2024-06-30");
        assert_eq!(format_iso_date("2024-06-30"), "2024-06-30");
        assert_eq!(format_iso_date("not a date"), "");
    }

    #[test]
    fn epoch_millis_formats_a_date() {
        // 2021-01-01T00:00:00Z in milliseconds.
        assert_eq!(format_epoch_millis(1_609_459_200_000), "2021-01-01");
        assert_eq!(format_epoch_millis(0), "1970-01-01");
    }

    #[test]
    fn bytes_scale_through_the_units() {
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(1536), "1.5KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0GB");
    }

    #[test]
    fn build_result_finalizes_and_labels() {
        let result = build_result(
            "# Title\n\n\n\nbody",
            "https://x",
            "npm",
            vec!["note".into()],
        );
        assert_eq!(result.content, "# Title\n\nbody");
        assert_eq!(result.method, "npm");
        assert_eq!(result.content_type, "text/markdown");
        assert_eq!(result.notes, vec!["note"]);
    }
}
