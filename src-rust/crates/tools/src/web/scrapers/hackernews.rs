// Hacker News handler: renders a story (with comments) or a front-page
// listing via the Firebase API.

use super::util::{
    build_result, decode_html_entities, format_epoch_millis, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct HackerNewsHandler;

const API_BASE: &str = "https://hacker-news.firebaseio.com/v0";

static ANCHOR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"<a\s+href="([^"]+)"[^>]*>([^<]*)</a>"#).expect("static hn anchor regex")
});
static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").expect("static hn tag regex"));

async fn fetch_item(id: i64, timeout: Duration) -> Option<Value> {
    let url = format!("{API_BASE}/item/{id}.json");
    let result = load_page(
        &url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    if !result.ok {
        return None;
    }
    serde_json::from_str(&result.content).ok()
}

/// Fetch up to `limit` items concurrently, dropping deleted or dead ones.
async fn fetch_items(ids: &[i64], timeout: Duration, limit: usize) -> Vec<Value> {
    let futures = ids
        .iter()
        .take(limit)
        .map(|id| fetch_item(*id, timeout))
        .collect::<Vec<_>>();
    futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("deleted").and_then(Value::as_bool) != Some(true)
                && item.get("dead").and_then(Value::as_bool) != Some(true)
        })
        .collect()
}

/// Convert HN's HTML comment/story text to markdown-ish plain text.
fn decode_hn_text(html: &str) -> String {
    let replaced = html
        .replace("<p>", "\n\n")
        .replace("</p>", "")
        .replace("<pre><code>", "\n```\n")
        .replace("</code></pre>", "\n```\n")
        .replace("<code>", "`")
        .replace("</code>", "`")
        .replace("<i>", "*")
        .replace("</i>", "*");
    let linked = ANCHOR.replace_all(&replaced, "[$2]($1)");
    let stripped = TAG.replace_all(&linked, "");
    decode_html_entities(&stripped).trim().to_string()
}

fn i64_field(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A relative age like `5m ago`, `3h ago`, `2d ago`, or a date past a week.
fn format_timestamp(unix_secs: i64, now_secs: i64) -> String {
    let diff = (now_secs - unix_secs).max(0);
    let hours = diff / 3600;
    let days = hours / 24;
    if days > 7 {
        return format_epoch_millis(unix_secs * 1000);
    }
    if days > 0 {
        return format!("{days}d ago");
    }
    if hours > 0 {
        return format!("{hours}h ago");
    }
    format!("{}m ago", diff / 60)
}

/// Prefix every line of `text` with `indent`.
fn indent_lines(text: &str, indent: &str) -> String {
    text.lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_comment(comment: &Value, indent: &str, now: i64) -> String {
    let by = str_field(comment, "by");
    let time = format_timestamp(i64_field(comment, "time").unwrap_or(0), now);
    let mut out = format!("{indent}**{by}** ({time})");
    if let Some(score) = i64_field(comment, "score") {
        let _ = write!(out, " [{score}]");
    }
    out.push('\n');
    let text = str_field(comment, "text");
    if !text.is_empty() {
        let _ = write!(out, "{}\n\n", indent_lines(&decode_hn_text(text), indent));
    } else {
        out.push('\n');
    }
    out
}

fn kids(item: &Value) -> Vec<i64> {
    item.get("kids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

fn render_story_header(item: &Value, now: i64) -> String {
    let mut out = format!("# {}\n\n", str_field(item, "title"));
    let url = str_field(item, "url");
    if !url.is_empty() {
        let _ = write!(out, "**URL:** {url}\n\n");
    }
    let by = str_field(item, "by");
    let score = i64_field(item, "score").unwrap_or(0);
    let time = format_timestamp(i64_field(item, "time").unwrap_or(0), now);
    let _ = write!(
        out,
        "**Posted by:** {by} | **Score:** {score} | **Time:** {time}"
    );
    if let Some(n) = i64_field(item, "descendants") {
        let _ = write!(out, " | **Comments:** {n}");
    }
    out.push_str("\n\n");
    let text = str_field(item, "text");
    if !text.is_empty() {
        let _ = write!(out, "{}\n\n", decode_hn_text(text));
    }
    out
}

async fn render_story(item: &Value, timeout: Duration, now: i64) -> String {
    let mut out = render_story_header(item, now);
    let top = kids(item);
    if top.is_empty() {
        return out;
    }
    let comments = fetch_items(&top, timeout, 20).await;
    if comments.is_empty() {
        return out;
    }
    out.push_str("---\n\n## Comments\n\n");
    for comment in &comments {
        out.push_str(&render_comment(comment, "", now));
        let child_ids = kids(comment);
        if child_ids.is_empty() {
            continue;
        }
        for child in fetch_items(&child_ids, timeout, 10).await {
            out.push_str(&render_comment(&child, "  ", now));
        }
    }
    out
}

async fn render_listing(ids: &[i64], timeout: Duration, title: &str, now: i64) -> String {
    let mut out = format!("# {title}\n\n");
    let stories = fetch_items(ids, timeout, 20).await;
    for (i, story) in stories.iter().enumerate() {
        let _ = writeln!(out, "{}. **{}**", i + 1, str_field(story, "title"));
        let url = str_field(story, "url");
        if !url.is_empty() {
            let _ = writeln!(out, "   {url}");
        }
        let score = i64_field(story, "score").unwrap_or(0);
        let by = str_field(story, "by");
        let time = format_timestamp(i64_field(story, "time").unwrap_or(0), now);
        let _ = write!(out, "   {score} points by {by} | {time}");
        if let Some(n) = i64_field(story, "descendants") {
            let _ = write!(out, " | {n} comments");
        }
        let id = i64_field(story, "id").unwrap_or(0);
        let _ = write!(out, "\n   https://news.ycombinator.com/item?id={id}\n\n");
    }
    out
}

/// The list endpoint and title for a front-page path, if it is one.
fn listing_for_path(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/news" => Some(("topstories", "Hacker News - Top Stories")),
        "/newest" => Some(("newstories", "Hacker News - New Stories")),
        "/best" => Some(("beststories", "Hacker News - Best Stories")),
        _ => None,
    }
}

async fn fetch_ids(endpoint: &str, timeout: Duration) -> Option<Vec<i64>> {
    let url = format!("{API_BASE}/{endpoint}.json");
    let result = load_page(
        &url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    if !result.ok {
        return None;
    }
    let ids: Vec<i64> = serde_json::from_str(&result.content).ok()?;
    Some(ids)
}

#[async_trait]
impl SpecialHandler for HackerNewsHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        if !parsed.host_str()?.contains("news.ycombinator.com") {
            return None;
        }
        let now = chrono::Utc::now().timestamp();

        let item_id = parsed
            .query_pairs()
            .find(|(k, _)| k == "id")
            .and_then(|(_, v)| v.parse::<i64>().ok());
        let (content, note) = if let Some(id) = item_id {
            let item = fetch_item(id, timeout).await?;
            (
                render_story(&item, timeout, now).await,
                format!("Fetched HN item {id} with top-level comments"),
            )
        } else {
            let (endpoint, title) = listing_for_path(parsed.path())?;
            let ids = fetch_ids(endpoint, timeout).await?;
            (
                render_listing(&ids, timeout, title, now).await,
                format!("Fetched top 20 from {endpoint}"),
            )
        };
        Some(build_result(&content, url, "hackernews", vec![note]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hn_text_becomes_markdown() {
        let html = "<p>Hello &amp; <i>world</i></p><p>See <a href=\"https://x.com\" rel=\"nofollow\">x</a></p><pre><code>code</code></pre>";
        let text = decode_hn_text(html);
        assert!(text.contains("Hello & *world*"));
        assert!(text.contains("[x](https://x.com)"));
        assert!(text.contains("```\ncode\n```"));
    }

    #[test]
    fn timestamps_scale_from_minutes_to_a_date() {
        let now = 1_700_000_000;
        assert_eq!(format_timestamp(now - 300, now), "5m ago");
        assert_eq!(format_timestamp(now - 7200, now), "2h ago");
        assert_eq!(format_timestamp(now - 3 * 86400, now), "3d ago");
        // Older than a week falls back to a date.
        assert_eq!(format_timestamp(1_609_459_200, 1_700_000_000), "2021-01-01");
    }

    #[test]
    fn listing_paths_map_to_endpoints() {
        assert_eq!(listing_for_path("/").unwrap().0, "topstories");
        assert_eq!(listing_for_path("/newest").unwrap().0, "newstories");
        assert_eq!(listing_for_path("/best").unwrap().0, "beststories");
        assert!(listing_for_path("/submit").is_none());
    }

    #[test]
    fn a_comment_renders_author_time_and_indented_text() {
        let now = 1_700_000_000;
        let comment = json!({ "by": "alice", "time": now - 3600, "text": "<p>Nice point</p>" });
        let out = render_comment(&comment, "  ", now);
        assert!(out.contains("  **alice** (1h ago)"));
        assert!(out.contains("  Nice point"));
    }
}
