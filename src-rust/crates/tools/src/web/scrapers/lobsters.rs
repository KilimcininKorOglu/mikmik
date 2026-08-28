// Lobste.rs handler: renders a story (with nested comments) or a listing via
// the JSON API.

use super::util::{build_result, format_iso_date, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct LobstersHandler;

const MAX_DEPTH: u64 = 5;

/// Which Lobste.rs resource a URL names, with the JSON path to fetch.
enum Target {
    Story(String),
    Listing { path: String, title: String },
}

fn parse_target(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("lobste.rs") {
        return None;
    }
    let path = parsed.path();
    if let Some(rest) = path.strip_prefix("/s/") {
        let id = rest.split('/').next().filter(|s| !s.is_empty())?;
        return Some(Target::Story(id.to_string()));
    }
    match path {
        "/" => Some(Target::Listing {
            path: "hottest".to_string(),
            title: "Lobste.rs Front Page".to_string(),
        }),
        "/newest" => Some(Target::Listing {
            path: "newest".to_string(),
            title: "Lobste.rs Newest".to_string(),
        }),
        _ => {
            let tag = path.strip_prefix("/t/")?.split('/').next()?;
            if tag.is_empty() {
                return None;
            }
            Some(Target::Listing {
                path: format!("t/{tag}"),
                title: format!("Lobste.rs Tag: {tag}"),
            })
        }
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn i64_field(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn tags_of(story: &Value) -> Vec<String> {
    story
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Render the comment tree, indenting by each comment's `indent_level`.
fn render_comments(comments: &[Value], out: &mut String) {
    for comment in comments {
        let level = comment
            .get("indent_level")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if level >= MAX_DEPTH {
            continue;
        }
        let indent = "  ".repeat(level as usize);
        let _ = write!(
            out,
            "{indent}### {} · {} points\n\n",
            str_field(comment, "commenting_user"),
            i64_field(comment, "score")
        );
        let body = str_field(comment, "comment").replace('\n', &format!("\n{indent}"));
        let _ = write!(out, "{indent}{body}\n\n");
        if let Some(children) = comment.get("comments").and_then(Value::as_array) {
            if !children.is_empty() {
                render_comments(children, out);
            }
        }
        let _ = write!(out, "{indent}---\n\n");
    }
}

fn render_story(story: &Value) -> String {
    let mut md = format!("# {}\n\n", str_field(story, "title"));
    let _ = write!(
        md,
        "**{}** · {} points · {} comments",
        str_field(story, "submitter_user"),
        i64_field(story, "score"),
        i64_field(story, "comment_count")
    );
    let tags = tags_of(story);
    if !tags.is_empty() {
        let _ = write!(md, " · [{}]", tags.join(", "));
    }
    let _ = write!(
        md,
        "\n*{}*\n\n",
        format_iso_date(str_field(story, "created_at"))
    );

    let description = str_field(story, "description");
    let link = str_field(story, "url");
    if !description.is_empty() {
        let _ = write!(md, "---\n\n{description}\n\n");
    } else if !link.is_empty() {
        let _ = write!(md, "**Link:** {link}\n\n");
    }

    if let Some(comments) = story.get("comments").and_then(Value::as_array) {
        if !comments.is_empty() {
            md.push_str("---\n\n## Comments\n\n");
            render_comments(comments, &mut md);
        }
    }
    md
}

fn render_listing(title: &str, stories: &[Value]) -> String {
    let mut md = format!("# {title}\n\n");
    for story in stories.iter().take(20) {
        let _ = write!(
            md,
            "- **{}** ({} pts, {} comments)\n  by {}",
            str_field(story, "title"),
            i64_field(story, "score"),
            i64_field(story, "comment_count"),
            str_field(story, "submitter_user")
        );
        let tags = tags_of(story);
        if !tags.is_empty() {
            let _ = write!(md, " · [{}]", tags.join(", "));
        }
        md.push('\n');
        let link = str_field(story, "url");
        if !link.is_empty() {
            let _ = writeln!(md, "  {link}");
        }
        let _ = write!(
            md,
            "  https://lobste.rs/s/{}\n\n",
            str_field(story, "short_id")
        );
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
    if !result.ok {
        return None;
    }
    serde_json::from_str(&result.content).ok()
}

#[async_trait]
impl SpecialHandler for LobstersHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let md = match parse_target(url)? {
            Target::Story(id) => {
                let story = fetch_json(&format!("https://lobste.rs/s/{id}.json"), timeout).await?;
                render_story(&story)
            }
            Target::Listing { path, title } => {
                let data = fetch_json(&format!("https://lobste.rs/{path}.json"), timeout).await?;
                render_listing(&title, data.as_array()?)
            }
        };
        Some(build_result(
            &md,
            url,
            "lobsters",
            vec!["Fetched via Lobste.rs JSON API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_reads_story_and_listings() {
        assert!(matches!(
            parse_target("https://lobste.rs/s/abc123/some-title"),
            Some(Target::Story(id)) if id == "abc123"
        ));
        assert!(matches!(
            parse_target("https://lobste.rs/"),
            Some(Target::Listing { path, .. }) if path == "hottest"
        ));
        assert!(matches!(
            parse_target("https://lobste.rs/t/rust"),
            Some(Target::Listing { path, .. }) if path == "t/rust"
        ));
        assert!(parse_target("https://example.com/s/abc").is_none());
    }

    #[test]
    fn render_story_lays_out_body_and_nested_comments() {
        let story = json!({
            "title": "A Story",
            "submitter_user": "alice",
            "score": 42,
            "comment_count": 2,
            "tags": ["rust", "programming"],
            "created_at": "2024-01-01T00:00:00Z",
            "description": "Body text.",
            "comments": [
                { "commenting_user": "bob", "score": 5, "indent_level": 0, "comment": "Top",
                  "comments": [
                    { "commenting_user": "carol", "score": 3, "indent_level": 1, "comment": "Reply" }
                  ] }
            ]
        });
        let md = render_story(&story);
        assert!(md.contains("# A Story"));
        assert!(md.contains("**alice** · 42 points · 2 comments · [rust, programming]"));
        assert!(md.contains("*2024-01-01*"));
        assert!(md.contains("---\n\nBody text."));
        assert!(md.contains("### bob · 5 points"));
        assert!(md.contains("  ### carol · 3 points"));
    }

    #[test]
    fn deep_comments_beyond_the_cap_are_skipped() {
        let mut out = String::new();
        let comments = vec![json!({
            "commenting_user": "deep", "score": 1, "indent_level": MAX_DEPTH, "comment": "x"
        })];
        render_comments(&comments, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn render_listing_lays_out_stories() {
        let stories = vec![json!({
            "title": "Story One", "score": 10, "comment_count": 3,
            "submitter_user": "dan", "tags": ["ai"], "short_id": "xyz"
        })];
        let md = render_listing("Lobste.rs Front Page", &stories);
        assert!(md.contains("# Lobste.rs Front Page"));
        assert!(md.contains("- **Story One** (10 pts, 3 comments)\n  by dan · [ai]"));
        assert!(md.contains("https://lobste.rs/s/xyz"));
    }
}
