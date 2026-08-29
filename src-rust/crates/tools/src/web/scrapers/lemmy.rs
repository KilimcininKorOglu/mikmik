// Lemmy handler: renders a post (or a comment's post) with a threaded comment
// tree from any Lemmy instance's v3 API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::time::Duration;

pub struct LemmyHandler;

static POST_OR_COMMENT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/(post|comment)/(\d+)").expect("static lemmy regex"));

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Host taken from an ActivityPub `actor_id`, if it parses.
fn actor_host(actor: &Value) -> Option<String> {
    let actor_id = str_field(actor, "actor_id")?;
    url::Url::parse(actor_id)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

fn format_community(community: &Value) -> String {
    let name = str_field(community, "name").unwrap_or("");
    match actor_host(community) {
        Some(host) => format!("!{name}@{host}"),
        None => format!("!{name}"),
    }
}

fn format_author(creator: &Value) -> String {
    let name = str_field(creator, "name").unwrap_or("unknown");
    match actor_host(creator) {
        Some(host) => format!("@{name}@{host}"),
        None => name.to_string(),
    }
}

fn indent_block(text: &str, indent: &str) -> String {
    text.split('\n')
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render one thread level and recurse into each comment's children.
fn render_thread(
    md: &mut String,
    children: &HashMap<i64, Vec<usize>>,
    comments: &[Value],
    parent: i64,
    depth: usize,
) {
    let Some(items) = children.get(&parent) else {
        return;
    };
    for &index in items {
        let view = &comments[index];
        let comment = view.get("comment").cloned().unwrap_or(Value::Null);
        let author = view
            .get("creator")
            .filter(|c| str_field(c, "name").is_some())
            .map(format_author)
            .unwrap_or_else(|| "unknown".to_string());
        let score = view
            .get("counts")
            .and_then(|c| c.get("score"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let indent = "  ".repeat(depth);
        let _ = writeln!(md, "{indent}- **{author}** · {score} points");
        if let Some(content) = str_field(&comment, "content") {
            let _ = writeln!(
                md,
                "{}",
                indent_block(content.trim(), &format!("{indent}  "))
            );
        }
        let id = comment.get("id").and_then(Value::as_i64).unwrap_or(0);
        render_thread(md, children, comments, id, depth + 1);
        md.push('\n');
    }
}

/// Group comments by resolved parent id (0 for top level) and render the tree.
fn render_comments(comments: &[Value]) -> String {
    let ids: HashSet<i64> = comments
        .iter()
        .filter_map(|v| {
            v.get("comment")
                .and_then(|c| c.get("id"))
                .and_then(Value::as_i64)
        })
        .collect();
    let mut children: HashMap<i64, Vec<usize>> = HashMap::new();
    for (index, view) in comments.iter().enumerate() {
        let parent = view
            .get("comment")
            .and_then(|c| c.get("parent_id"))
            .and_then(Value::as_i64)
            .filter(|p| ids.contains(p))
            .unwrap_or(0);
        children.entry(parent).or_default().push(index);
    }
    let mut md = String::new();
    render_thread(&mut md, &children, comments, 0, 0);
    md.trim().to_string()
}

fn render_post(post_view: &Value, comments: &[Value]) -> String {
    let post = post_view.get("post").cloned().unwrap_or(Value::Null);
    let mut md = format!("# {}\n\n", str_field(&post, "name").unwrap_or("(post)"));

    let community = post_view
        .get("community")
        .map(format_community)
        .unwrap_or_default();
    let author = post_view
        .get("creator")
        .map(format_author)
        .unwrap_or_default();
    let counts = post_view.get("counts").cloned().unwrap_or(Value::Null);
    let score = counts.get("score").and_then(Value::as_i64).unwrap_or(0);
    let comment_count = counts
        .get("comments")
        .and_then(Value::as_i64)
        .unwrap_or(comments.len() as i64);
    let _ = writeln!(
        md,
        "**Community:** {community} · **Author:** {author} · **Score:** {score} · **Comments:** {comment_count}"
    );
    if let Some(link) = str_field(&post, "url") {
        let _ = writeln!(md, "**Link:** {link}");
    }
    md.push('\n');

    if let Some(body) = str_field(&post, "body") {
        let _ = write!(md, "---\n\n{body}\n\n");
    }
    if !comments.is_empty() {
        let threaded = render_comments(comments);
        if !threaded.is_empty() {
            let _ = write!(md, "---\n\n## Comments\n\n{threaded}\n");
        }
    }
    md
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

/// Resolve the post id, following a comment to its parent post when needed.
async fn resolve_post_id(base: &str, kind: &str, id: i64, timeout: Duration) -> Option<i64> {
    if kind != "comment" {
        return Some(id);
    }
    let data = fetch_json(&format!("{base}/api/v3/comment?id={id}"), timeout).await?;
    data.get("comment_view")
        .and_then(|v| v.get("comment"))
        .and_then(|c| c.get("post_id"))
        .and_then(Value::as_i64)
}

#[async_trait]
impl SpecialHandler for LemmyHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let caps = POST_OR_COMMENT.captures(parsed.path())?;
        let kind = &caps[1];
        let id: i64 = caps[2].parse().ok()?;
        let base = format!("{}://{}", parsed.scheme(), parsed.host_str()?);

        let post_id = resolve_post_id(&base, kind, id, timeout).await?;
        let post_url = format!("{base}/api/v3/post?id={post_id}");
        let comments_url = format!("{base}/api/v3/comment/list?post_id={post_id}");
        let (post_data, comments_data) = tokio::join!(
            fetch_json(&post_url, timeout),
            fetch_json(&comments_url, timeout)
        );

        let post_view = post_data?.get("post_view")?.clone();
        let comments = comments_data
            .and_then(|d| d.get("comments").and_then(Value::as_array).cloned())
            .unwrap_or_default();

        let md = render_post(&post_view, &comments);
        Some(build_result(
            &md,
            url,
            "lemmy-api",
            vec!["Fetched via Lemmy API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn community_and_author_use_the_actor_host() {
        let community = json!({ "name": "rust", "actor_id": "https://lemmy.ml/c/rust" });
        assert_eq!(format_community(&community), "!rust@lemmy.ml");
        let creator = json!({ "name": "alice", "actor_id": "https://lemmy.world/u/alice" });
        assert_eq!(format_author(&creator), "@alice@lemmy.world");
        assert_eq!(format_author(&json!({ "name": "bob" })), "bob");
    }

    #[test]
    fn comments_render_as_a_nested_thread() {
        let comments = vec![
            json!({ "comment": { "id": 1, "content": "top", "parent_id": null }, "creator": { "name": "a" }, "counts": { "score": 5 } }),
            json!({ "comment": { "id": 2, "content": "reply", "parent_id": 1 }, "creator": { "name": "b" }, "counts": { "score": 3 } }),
        ];
        let rendered = render_comments(&comments);
        assert!(rendered.contains("- **a** · 5 points"));
        assert!(rendered.contains("  - **b** · 3 points"));
    }

    #[test]
    fn render_post_lays_out_header_and_body() {
        let post_view = json!({
            "post": { "name": "Hello", "body": "Post body", "url": "https://example.com" },
            "community": { "name": "test", "actor_id": "https://lemmy.ml/c/test" },
            "creator": { "name": "alice", "actor_id": "https://lemmy.ml/u/alice" },
            "counts": { "score": 42, "comments": 1 }
        });
        let comments = vec![
            json!({ "comment": { "id": 1, "content": "nice", "parent_id": null }, "creator": { "name": "bob" }, "counts": { "score": 2 } }),
        ];
        let md = render_post(&post_view, &comments);
        assert!(md.contains("# Hello"));
        assert!(md.contains("**Community:** !test@lemmy.ml · **Author:** @alice@lemmy.ml · **Score:** 42 · **Comments:** 1"));
        assert!(md.contains("**Link:** https://example.com"));
        assert!(md.contains("---\n\nPost body"));
        assert!(md.contains("## Comments\n\n- **bob** · 2 points"));
    }
}
