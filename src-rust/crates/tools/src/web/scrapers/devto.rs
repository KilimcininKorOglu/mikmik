// dev.to handler: renders an article (markdown body) or a tag/user listing
// via the Forem API.
//
// An article that ships only `body_html` (no `body_markdown`) needs the DOM
// parser deferred to the HTML-parse scraper phase; such a body is omitted with
// a note rather than dumped as raw HTML.

use super::util::{
    build_result, format_iso_date, format_number, load_page, percent_encode_component, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct DevToHandler;

/// Which dev.to resource a URL names, with the API path to fetch.
enum Target {
    Tag(String),
    User(String),
    Article { username: String, slug: String },
}

fn parse_target(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "dev.to" {
        return None;
    }
    let parts: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        ["t", tag, ..] => Some(Target::Tag((*tag).to_string())),
        [username] => Some(Target::User((*username).to_string())),
        [username, slug, ..] => Some(Target::Article {
            username: (*username).to_string(),
            slug: (*slug).to_string(),
        }),
        _ => None,
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn tags_of(article: &Value) -> Vec<String> {
    let from = |key: &str| {
        article.get(key).and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
    };
    from("tag_list")
        .or_else(|| from("tags"))
        .unwrap_or_default()
}

fn reactions(article: &Value) -> u64 {
    article
        .get("positive_reactions_count")
        .or_else(|| article.get("public_reactions_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn published_date(article: &Value) -> String {
    let raw = str_field(article, "published_at")
        .or_else(|| str_field(article, "published_timestamp"))
        .unwrap_or("");
    format_iso_date(raw)
}

/// One entry in a tag or user listing. `with_author` prints the byline (tag
/// pages carry it; user pages already name the author in the heading).
fn append_listing_entry(md: &mut String, article: &Value, with_author: bool) {
    let title = str_field(article, "title").unwrap_or("(untitled)");
    let _ = write!(md, "### {title}\n\n");
    let read = article
        .get("reading_time_minutes")
        .and_then(Value::as_u64)
        .map(|m| format!(" · {m} min read"))
        .unwrap_or_default();
    let reacts = match reactions(article) {
        0 => String::new(),
        n => format!(" · {} reactions", format_number(n)),
    };
    if with_author {
        let user = article.get("user").cloned().unwrap_or(Value::Null);
        let name = str_field(&user, "name").unwrap_or("Unknown");
        let username = str_field(&user, "username").unwrap_or("unknown");
        let _ = writeln!(md, "by **{name}** (@{username}){read}{reacts}");
    } else {
        let trimmed = read.trim_start_matches(" · ");
        let _ = writeln!(md, "{trimmed}{reacts}");
    }
    let _ = writeln!(md, "*{}*", published_date(article));
    let tags = tags_of(article);
    if !tags.is_empty() {
        let list = tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(md, "Tags: {list}");
    }
    if let Some(desc) = str_field(article, "description") {
        let _ = write!(md, "\n{desc}\n");
    }
    md.push_str("\n---\n\n");
}

fn render_listing(title: &str, articles: &[Value], with_author: bool) -> String {
    let mut md = format!("# {title}\n\n## Recent Articles ({})\n\n", articles.len());
    for article in articles {
        append_listing_entry(&mut md, article, with_author);
    }
    md
}

fn render_article(article: &Value, fallback_user: &str) -> String {
    let title = str_field(article, "title").unwrap_or("(untitled)");
    let user = article.get("user").cloned().unwrap_or(Value::Null);
    let name = str_field(&user, "name").unwrap_or("Unknown");
    let username = str_field(&user, "username").unwrap_or(fallback_user);
    let mut md = format!("# {title}\n\n");
    let _ = writeln!(md, "**Author:** {name} (@{username})");
    let _ = writeln!(md, "**Published:** {}", published_date(article));
    if let Some(read) = article.get("reading_time_minutes").and_then(Value::as_u64) {
        if read > 0 {
            let _ = writeln!(md, "**Reading time:** {read} min");
        }
    }
    let reacts = reactions(article);
    if reacts > 0 {
        let _ = writeln!(md, "**Reactions:** {}", format_number(reacts));
    }
    if let Some(comments) = article.get("comments_count").and_then(Value::as_u64) {
        if comments > 0 {
            let _ = writeln!(md, "**Comments:** {}", format_number(comments));
        }
    }
    let tags = tags_of(article);
    if !tags.is_empty() {
        let list = tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(md, "**Tags:** {list}");
    }
    md.push_str("\n---\n\n");
    match str_field(article, "body_markdown") {
        Some(body) => md.push_str(body),
        None => md.push_str("*(HTML-only body omitted)*"),
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

impl DevToHandler {
    async fn render(&self, target: Target, timeout: Duration) -> Option<String> {
        match target {
            Target::Tag(tag) => {
                let url = format!(
                    "https://dev.to/api/articles?tag={}&per_page=20",
                    percent_encode_component(&tag)
                );
                let articles = fetch_json(&url, timeout).await?;
                let list = articles.as_array().filter(|a| !a.is_empty())?;
                Some(render_listing(&format!("dev.to/t/{tag}"), list, true))
            }
            Target::User(username) => {
                let url = format!(
                    "https://dev.to/api/articles?username={}&per_page=20",
                    percent_encode_component(&username)
                );
                let articles = fetch_json(&url, timeout).await?;
                let list = articles.as_array().filter(|a| !a.is_empty())?;
                Some(render_listing(&format!("dev.to/{username}"), list, false))
            }
            Target::Article { username, slug } => {
                let url = format!(
                    "https://dev.to/api/articles/{}/{}",
                    percent_encode_component(&username),
                    percent_encode_component(&slug)
                );
                let article = fetch_json(&url, timeout).await?;
                str_field(&article, "title")?;
                Some(render_article(&article, &username))
            }
        }
    }
}

#[async_trait]
impl SpecialHandler for DevToHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let target = parse_target(url)?;
        let md = self.render(target, timeout).await?;
        Some(build_result(
            &md,
            url,
            "devto",
            vec!["Fetched via dev.to API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_distinguishes_tag_user_and_article() {
        assert!(
            matches!(parse_target("https://dev.to/t/rust"), Some(Target::Tag(t)) if t == "rust")
        );
        assert!(matches!(parse_target("https://dev.to/ben"), Some(Target::User(u)) if u == "ben"));
        assert!(matches!(
            parse_target("https://dev.to/ben/my-post-123"),
            Some(Target::Article { slug, .. }) if slug == "my-post-123"
        ));
        assert!(parse_target("https://example.com/ben").is_none());
    }

    #[test]
    fn render_article_uses_the_markdown_body() {
        let article = json!({
            "title": "Learning Rust",
            "user": { "name": "Ferris", "username": "ferris" },
            "published_at": "2024-01-01T00:00:00Z",
            "reading_time_minutes": 5,
            "positive_reactions_count": 200,
            "tag_list": ["rust", "beginners"],
            "body_markdown": "## Intro\n\nHello."
        });
        let md = render_article(&article, "ferris");
        assert!(md.contains("# Learning Rust"));
        assert!(md.contains("**Author:** Ferris (@ferris)"));
        assert!(md.contains("**Reading time:** 5 min"));
        assert!(md.contains("**Reactions:** 200"));
        assert!(md.contains("**Tags:** #rust, #beginners"));
        assert!(md.contains("## Intro\n\nHello."));
    }

    #[test]
    fn an_html_only_body_is_omitted_with_a_note() {
        let article = json!({ "title": "X", "body_html": "<p>hi</p>" });
        let md = render_article(&article, "x");
        assert!(md.contains("*(HTML-only body omitted)*"));
    }

    #[test]
    fn render_listing_lays_out_entries() {
        let articles = vec![json!({
            "title": "Post One",
            "user": { "name": "Ben", "username": "ben" },
            "published_at": "2024-01-01T00:00:00Z",
            "reading_time_minutes": 3,
            "tag_list": ["webdev"]
        })];
        let md = render_listing("dev.to/t/webdev", &articles, true);
        assert!(md.contains("# dev.to/t/webdev"));
        assert!(md.contains("## Recent Articles (1)"));
        assert!(md.contains("### Post One"));
        assert!(md.contains("by **Ben** (@ben) · 3 min read"));
        assert!(md.contains("Tags: #webdev"));
    }
}
