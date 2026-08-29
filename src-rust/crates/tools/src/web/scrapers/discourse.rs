// Discourse handler: renders a forum topic (or a post's topic) from any
// Discourse instance's JSON API.

use super::util::{
    build_result, format_iso_date, html_to_markdown, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct DiscourseHandler;

const MAX_POSTS: usize = 20;

static TOPIC_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(.*?)/t/(?:[^/]+/)?(\d+)(?:\.json)?(?:/|$)").expect("static discourse topic")
});
static POST_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(.*?)/posts/(\d+)(?:\.json)?(?:/|$)").expect("static discourse post")
});

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn normalize_base_path(base: &str) -> String {
    if base.is_empty() || base == "/" {
        return String::new();
    }
    base.trim_end_matches('/').to_string()
}

/// Author label combining name and `@username` when they differ.
fn format_author(name: Option<&str>, username: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|s| !s.is_empty());
    let username = username.map(str::trim).filter(|s| !s.is_empty());
    match (name, username) {
        (Some(n), Some(u)) if n != u => format!("{n} (@{u})"),
        (_, Some(u)) => format!("@{u}"),
        (Some(n), None) => n.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

fn format_category(topic: &Value) -> Option<String> {
    let category = topic.get("category");
    let mut parts: Vec<String> = Vec::new();
    if let Some(name) = category
        .and_then(|c| str_field(c, "name"))
        .or_else(|| str_field(topic, "category_slug"))
    {
        parts.push(name.to_string());
    }
    if let Some(id) = category
        .and_then(|c| c.get("id"))
        .and_then(Value::as_i64)
        .or_else(|| topic.get("category_id").and_then(Value::as_i64))
    {
        parts.push(format!("#{id}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Post body: prefer the raw markdown, else convert the cooked HTML.
fn format_post_body(post: &Value) -> String {
    if let Some(raw) = str_field(post, "raw") {
        return raw.trim().to_string();
    }
    match str_field(post, "cooked") {
        Some(cooked) => html_to_markdown(cooked),
        None => String::new(),
    }
}

fn append_topic_meta(md: &mut String, topic: &Value) {
    let mut parts: Vec<String> = Vec::new();
    for (field, label) in [
        ("id", "Topic ID"),
        ("posts_count", "Posts"),
        ("views", "Views"),
        ("like_count", "Likes"),
    ] {
        if let Some(value) = topic.get(field).and_then(Value::as_i64) {
            parts.push(format!("**{label}:** {value}"));
        }
    }
    if !parts.is_empty() {
        let _ = writeln!(md, "{}", parts.join(" | "));
    }
    if let Some(category) = format_category(topic) {
        let _ = writeln!(md, "**Category:** {category}");
    }
    if let Some(tags) = topic.get("tags").and_then(Value::as_array) {
        let list: Vec<&str> = tags.iter().filter_map(Value::as_str).collect();
        if !list.is_empty() {
            let _ = writeln!(md, "**Tags:** {}", list.join(", "));
        }
    }
    let created_by = topic
        .get("details")
        .and_then(|d| d.get("created_by"))
        .map(|u| format_author(str_field(u, "name"), str_field(u, "username")))
        .unwrap_or_else(|| "unknown".to_string());
    let created_at = str_field(topic, "created_at");
    if created_by != "unknown" || created_at.is_some() {
        let _ = writeln!(
            md,
            "**Created by:** {created_by} - {}",
            format_iso_date(created_at.unwrap_or(""))
        );
    }
}

fn collect_posts(topic: &Value, requested: Option<&Value>) -> Vec<Value> {
    let mut posts: Vec<Value> = topic
        .get("post_stream")
        .and_then(|s| s.get("posts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(req) = requested {
        let req_id = req.get("id").and_then(Value::as_i64);
        let present = posts
            .iter()
            .any(|p| p.get("id").and_then(Value::as_i64) == req_id);
        if !present {
            posts.insert(0, req.clone());
        }
    }
    posts
}

fn append_posts(md: &mut String, posts: &[Value]) {
    if posts.is_empty() {
        return;
    }
    md.push_str("## Posts\n\n");
    for post in posts.iter().take(MAX_POSTS) {
        let author = format_author(str_field(post, "name"), str_field(post, "username"));
        let date = format_iso_date(str_field(post, "created_at").unwrap_or(""));
        let likes = post.get("like_count").and_then(Value::as_i64).unwrap_or(0);
        let label = post
            .get("post_number")
            .and_then(Value::as_i64)
            .or_else(|| post.get("id").and_then(Value::as_i64))
            .map(|n| format!("Post {n}"))
            .unwrap_or_else(|| "Post".to_string());
        let _ = write!(md, "### {label} - {author} - {date} - Likes: {likes}\n\n");
        let content = format_post_body(post);
        if content.is_empty() {
            md.push_str("_No content available._\n\n---\n\n");
        } else {
            let _ = write!(md, "{content}\n\n---\n\n");
        }
    }
}

fn render(topic: &Value, posts: &[Value]) -> Option<String> {
    let title = str_field(topic, "title").or_else(|| str_field(topic, "fancy_title"))?;
    let mut md = format!("# {title}\n\n");
    append_topic_meta(&mut md, topic);
    md.push('\n');

    let description = match str_field(topic, "excerpt") {
        Some(excerpt) => html_to_markdown(excerpt),
        None => posts.first().map(format_post_body).unwrap_or_default(),
    };
    if !description.is_empty() {
        let _ = write!(md, "## Description\n\n{description}\n\n");
    }
    append_posts(&mut md, posts);
    Some(md)
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}

/// Resolve the base URL and topic id, following a post to its topic if needed.
async fn resolve_topic(
    parsed: &url::Url,
    timeout: Duration,
) -> Option<(String, String, Option<Value>)> {
    let origin = format!("{}://{}", parsed.scheme(), parsed.host_str()?);
    let path = parsed.path();
    if let Some(caps) = TOPIC_PATH.captures(path) {
        let base = format!("{origin}{}", normalize_base_path(&caps[1]));
        return Some((base, caps[2].to_string(), None));
    }
    let caps = POST_PATH.captures(path)?;
    let base = format!("{origin}{}", normalize_base_path(&caps[1]));
    let post_url = format!("{base}/posts/{}.json?include_raw=1", &caps[2]);
    let post = fetch_json(&post_url, timeout).await?;
    let topic_id = post.get("topic_id").and_then(Value::as_i64)?;
    Some((base, topic_id.to_string(), Some(post)))
}

#[async_trait]
impl SpecialHandler for DiscourseHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let (base, topic_id, requested) = resolve_topic(&parsed, timeout).await?;

        let topic_url = format!("{base}/t/{topic_id}.json?include_raw=1");
        let topic = fetch_json(&topic_url, timeout).await?;
        let posts = collect_posts(&topic, requested.as_ref());
        let md = render(&topic, &posts)?;
        Some(build_result(
            &md,
            url,
            "discourse-api",
            vec!["Fetched via Discourse API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn topic_path_extracts_base_and_id() {
        let caps = TOPIC_PATH.captures("/t/some-slug/12345").expect("match");
        assert_eq!(&caps[2], "12345");
        let sub = TOPIC_PATH.captures("/forum/t/98765.json").expect("match");
        assert_eq!(&sub[1], "/forum");
        assert_eq!(&sub[2], "98765");
    }

    #[test]
    fn author_combines_name_and_username() {
        assert_eq!(
            format_author(Some("Jane"), Some("jane_d")),
            "Jane (@jane_d)"
        );
        assert_eq!(format_author(None, Some("bob")), "@bob");
        assert_eq!(format_author(None, None), "unknown");
    }

    #[test]
    fn render_lays_out_topic_and_posts() {
        let topic = json!({
            "id": 100,
            "title": "How to use Rust",
            "posts_count": 2,
            "views": 500,
            "category": { "name": "Help", "id": 3 },
            "tags": ["rust", "beginners"],
            "details": { "created_by": { "name": "Alice", "username": "alice" } },
            "created_at": "2024-01-01T00:00:00Z"
        });
        let posts = vec![
            json!({ "id": 1, "post_number": 1, "username": "alice", "created_at": "2024-01-01T00:00:00Z", "raw": "First post.", "like_count": 5 }),
        ];
        let md = render(&topic, &posts).expect("render");
        assert!(md.contains("# How to use Rust"));
        assert!(md.contains("**Topic ID:** 100 | **Posts:** 2 | **Views:** 500"));
        assert!(md.contains("**Category:** Help #3"));
        assert!(md.contains("**Tags:** rust, beginners"));
        assert!(md.contains("**Created by:** Alice (@alice) - 2024-01-01"));
        assert!(md.contains("### Post 1 - @alice - 2024-01-01 - Likes: 5"));
        assert!(md.contains("First post."));
    }
}
