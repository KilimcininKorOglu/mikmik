// Reddit handler: renders a post (with top comments) or a listing via the
// public JSON API (append `.json` to any Reddit URL).

use super::util::{build_result, format_epoch_millis, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct RedditHandler;

/// The `.json` API URL for a Reddit page, preserving any query string.
fn json_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("reddit.com") {
        return None;
    }
    let path = parsed.path().trim_end_matches('/');
    let mut out = format!("{}://{}{}.json", parsed.scheme(), parsed.host_str()?, path);
    if let Some(query) = parsed.query() {
        let _ = write!(out, "?{query}");
    }
    Some(out)
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn i64_field(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn created_date(post: &Value) -> String {
    post.get("created_utc")
        .and_then(Value::as_f64)
        .map(|s| format_epoch_millis((s * 1000.0) as i64))
        .unwrap_or_default()
}

/// The `data` of the first child in a listing envelope.
fn first_child(listing: &Value) -> Option<&Value> {
    listing
        .get("data")?
        .get("children")?
        .as_array()?
        .first()?
        .get("data")
}

fn render_post(root: &Value) -> Option<String> {
    let arr = root.as_array()?;
    let post = first_child(arr.first()?)?;
    let mut md = format!("# {}\n\n", str_field(post, "title"));
    let _ = writeln!(
        md,
        "**r/{}** · u/{} · {} points · {} comments",
        str_field(post, "subreddit"),
        str_field(post, "author"),
        i64_field(post, "score"),
        i64_field(post, "num_comments")
    );
    let _ = write!(md, "*{}*\n\n", created_date(post));

    let is_self = post.get("is_self").and_then(Value::as_bool) == Some(true);
    let selftext = str_field(post, "selftext");
    if is_self && !selftext.is_empty() {
        let _ = write!(md, "---\n\n{selftext}\n\n");
    } else if !is_self {
        let _ = write!(md, "**Link:** {}\n\n", str_field(post, "url"));
    }

    if let Some(comments) = arr.get(1) {
        append_comments(&mut md, comments);
    }
    Some(md)
}

fn append_comments(md: &mut String, listing: &Value) {
    let Some(children) = listing
        .get("data")
        .and_then(|d| d.get("children"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let comments: Vec<&Value> = children
        .iter()
        .filter(|c| c.get("kind").and_then(Value::as_str) == Some("t1"))
        .filter_map(|c| c.get("data"))
        .take(10)
        .collect();
    if comments.is_empty() {
        return;
    }
    md.push_str("---\n\n## Top Comments\n\n");
    for comment in comments {
        let _ = write!(
            md,
            "### u/{} · {} points\n\n{}\n\n---\n\n",
            str_field(comment, "author"),
            i64_field(comment, "score"),
            str_field(comment, "body")
        );
    }
}

fn render_listing(root: &Value) -> Option<String> {
    let children = root.get("data")?.get("children")?.as_array()?;
    let posts: Vec<&Value> = children
        .iter()
        .filter_map(|c| c.get("data"))
        .take(20)
        .collect();
    let subreddit = posts
        .first()
        .map(|p| str_field(p, "subreddit"))
        .unwrap_or("");
    let name = if subreddit.is_empty() {
        "Reddit"
    } else {
        subreddit
    };
    let mut md = format!("# r/{name}\n\n");
    for post in posts {
        let _ = write!(
            md,
            "- **{}** ({} pts, {} comments)\n  by u/{}\n\n",
            str_field(post, "title"),
            i64_field(post, "score"),
            i64_field(post, "num_comments"),
            str_field(post, "author")
        );
    }
    Some(md)
}

fn render(root: &Value) -> Option<String> {
    let md = if root.is_array() {
        render_post(root)?
    } else {
        render_listing(root)?
    };
    if md.trim().is_empty() {
        None
    } else {
        Some(md)
    }
}

#[async_trait]
impl SpecialHandler for RedditHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let api = json_url(url)?;
        let result = load_page(
            &api,
            LoadOptions {
                timeout,
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let root: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&root)?;
        Some(build_result(
            &md,
            url,
            "reddit",
            vec!["Fetched via Reddit JSON API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_url_appends_and_keeps_query() {
        assert_eq!(
            json_url("https://www.reddit.com/r/rust/comments/abc/title/").as_deref(),
            Some("https://www.reddit.com/r/rust/comments/abc/title.json")
        );
        assert_eq!(
            json_url("https://reddit.com/r/rust?sort=top").as_deref(),
            Some("https://reddit.com/r/rust.json?sort=top")
        );
        assert!(json_url("https://example.com/r/rust").is_none());
    }

    #[test]
    fn render_post_lays_out_body_and_comments() {
        let root = json!([
            { "data": { "children": [{ "data": {
                "title": "Async in Rust",
                "subreddit": "rust",
                "author": "ferris",
                "score": 500,
                "num_comments": 2,
                "created_utc": 1_609_459_200.0,
                "is_self": true,
                "selftext": "Here is my question."
            } }] } },
            { "data": { "children": [
                { "kind": "t1", "data": { "author": "a", "score": 10, "body": "First" } },
                { "kind": "more", "data": {} }
            ] } }
        ]);
        let md = render(&root).expect("rendered");
        assert!(md.contains("# Async in Rust"));
        assert!(md.contains("**r/rust** · u/ferris · 500 points · 2 comments"));
        assert!(md.contains("*2021-01-01*"));
        assert!(md.contains("---\n\nHere is my question."));
        assert!(md.contains("### u/a · 10 points\n\nFirst"));
    }

    #[test]
    fn render_listing_lays_out_posts() {
        let root = json!({ "data": { "children": [
            { "data": { "title": "P1", "subreddit": "rust", "author": "x", "score": 5, "num_comments": 1 } }
        ] } });
        let md = render(&root).expect("rendered");
        assert!(md.contains("# r/rust"));
        assert!(md.contains("- **P1** (5 pts, 1 comments)\n  by u/x"));
    }
}
