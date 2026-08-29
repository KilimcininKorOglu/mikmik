// Bluesky handler: renders a profile or a post thread from the public AT
// Protocol API.

use super::util::{build_result, format_number, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct BlueskyHandler;

const API_BASE: &str = "https://public.api.bsky.app/xrpc";

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Format a post timestamp as `Jan 5, 2024, 03:04 PM` (UTC), else the raw text.
fn format_post_date(created_at: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(created_at) {
        Ok(dt) => dt
            .with_timezone(&chrono::Utc)
            .format("%b %-d, %Y, %I:%M %p")
            .to_string(),
        Err(_) => created_at.to_string(),
    }
}

fn format_join_date(created_at: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(created_at) {
        Ok(dt) => dt
            .with_timezone(&chrono::Utc)
            .format("%B %-d, %Y")
            .to_string(),
        Err(_) => created_at.to_string(),
    }
}

fn count(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn quote_lines(text: &str, prefix: &str) -> String {
    text.split('\n')
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Append the post's embed (link card, images, or a quoted post).
fn append_embed(md: &mut String, embed: &Value) {
    let kind = str_field(embed, "$type").unwrap_or("");
    match kind {
        "app.bsky.embed.external#view" => {
            if let Some(ext) = embed.get("external") {
                let uri = str_field(ext, "uri").unwrap_or("");
                let title = str_field(ext, "title").unwrap_or(uri);
                let _ = write!(md, "\n📎 [{title}]({uri})");
                if let Some(desc) = str_field(ext, "description") {
                    let _ = write!(md, "\n*{desc}*");
                }
                md.push('\n');
            }
        }
        "app.bsky.embed.images#view" => {
            if let Some(images) = embed.get("images").and_then(Value::as_array) {
                let _ = write!(md, "\n🖼️ {} image(s)", images.len());
                for img in images {
                    if let Some(alt) = str_field(img, "alt") {
                        let _ = write!(md, "\n- Alt: \"{alt}\"");
                    }
                }
                md.push('\n');
            }
        }
        "app.bsky.embed.record#view" | "app.bsky.embed.recordWithMedia#view" => {
            append_quoted_record(md, embed.get("record"));
        }
        _ => {}
    }
}

fn append_quoted_record(md: &mut String, record: Option<&Value>) {
    let Some(rec) = record else { return };
    let text = rec.get("value").and_then(|v| str_field(v, "text"));
    let author = rec.get("author");
    if let (Some(text), Some(author)) = (text, author) {
        let name = str_field(author, "displayName")
            .or_else(|| str_field(author, "handle"))
            .unwrap_or("");
        let handle = str_field(author, "handle").unwrap_or("");
        md.push_str("\n**Quoted post:**\n");
        let _ = writeln!(md, "> **{name}** (@{handle})");
        md.push_str(&quote_lines(text, "> "));
        md.push('\n');
    }
}

fn append_stats(md: &mut String, post: &Value) {
    let mut stats: Vec<String> = Vec::new();
    for (key, emoji) in [
        ("likeCount", "❤️"),
        ("repostCount", "🔁"),
        ("replyCount", "💬"),
        ("quoteCount", "📝"),
    ] {
        let n = count(post, key);
        if n > 0 {
            stats.push(format!("{emoji} {}", format_number(n)));
        }
    }
    if !stats.is_empty() {
        let _ = write!(md, "\n{}\n", stats.join(" • "));
    }
}

fn format_post(post: &Value, is_quote: bool) -> String {
    let author = post.get("author").cloned().unwrap_or(Value::Null);
    let handle = str_field(&author, "handle").unwrap_or("");
    let name = str_field(&author, "displayName").unwrap_or(handle);
    let record = post.get("record").cloned().unwrap_or(Value::Null);
    let text = str_field(&record, "text").unwrap_or("");
    let date = format_post_date(str_field(&record, "createdAt").unwrap_or(""));

    let mut md = String::new();
    if is_quote {
        let _ = write!(md, "> **{name}** (@{handle}) - {date}\n>\n");
        md.push_str(&quote_lines(text, "> "));
        md.push('\n');
    } else {
        let _ = writeln!(md, "**{name}** (@{handle})");
        let _ = write!(md, "*{date}*\n\n{text}\n");
    }

    if let Some(embed) = post.get("embed").filter(|e| !e.is_null()) {
        append_embed(&mut md, embed);
    }
    if !is_quote {
        append_stats(&mut md, post);
    }
    md
}

fn render_profile(profile: &Value) -> String {
    let handle = str_field(profile, "handle").unwrap_or("");
    let name = str_field(profile, "displayName").unwrap_or(handle);
    let mut md = format!("# {name}\n\n**@{handle}**\n\n");
    if let Some(desc) = str_field(profile, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    md.push_str("---\n\n");
    let _ = writeln!(
        md,
        "- **Followers:** {}",
        format_number(count(profile, "followersCount"))
    );
    let _ = writeln!(
        md,
        "- **Following:** {}",
        format_number(count(profile, "followsCount"))
    );
    let _ = writeln!(
        md,
        "- **Posts:** {}",
        format_number(count(profile, "postsCount"))
    );
    if let Some(created) = str_field(profile, "createdAt") {
        let _ = writeln!(md, "- **Joined:** {}", format_join_date(created));
    }
    let _ = write!(
        md,
        "\n**DID:** `{}`\n",
        str_field(profile, "did").unwrap_or("")
    );
    md
}

fn render_thread(thread: &Value) -> Option<String> {
    let post = thread.get("post").filter(|p| !p.is_null())?;
    let mut md = "# Bluesky Post\n\n".to_string();
    if let Some(parent_post) = thread.get("parent").and_then(|p| p.get("post")) {
        md.push_str("**Replying to:**\n");
        md.push_str(&format_post(parent_post, true));
        md.push_str("\n---\n\n");
    }
    md.push_str(&format_post(post, false));

    if let Some(replies) = thread.get("replies").and_then(Value::as_array) {
        let rendered: Vec<&Value> = replies
            .iter()
            .filter_map(|r| r.get("post"))
            .take(10)
            .collect();
        if !rendered.is_empty() {
            md.push_str("\n---\n\n## Replies\n\n");
            for reply in rendered {
                md.push_str(&format_post(reply, false));
                md.push_str("\n---\n\n");
            }
        }
    }
    Some(md)
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            headers: vec![("Accept".to_string(), "application/json".to_string())],
            ..Default::default()
        },
    )
    .await;
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}

async fn render_post_thread(
    handle: &str,
    rkey: &str,
    timeout: Duration,
) -> Option<(String, String)> {
    let profile_url = format!(
        "{API_BASE}/app.bsky.actor.getProfile?actor={}",
        super::util::percent_encode_component(handle)
    );
    let did = str_field(&fetch_json(&profile_url, timeout).await?, "did")?.to_string();
    let at_uri = format!("at://{did}/app.bsky.feed.post/{rkey}");
    let thread_url = format!(
        "{API_BASE}/app.bsky.feed.getPostThread?uri={}&depth=6&parentHeight=3",
        super::util::percent_encode_component(&at_uri)
    );
    let data = fetch_json(&thread_url, timeout).await?;
    let md = render_thread(data.get("thread")?)?;
    Some((md, at_uri))
}

#[async_trait]
impl SpecialHandler for BlueskyHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host_str()?;
        if host != "bsky.app" && host != "www.bsky.app" {
            return None;
        }
        let parts: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
        if parts.first() != Some(&"profile") || parts.get(1).is_none() {
            return None;
        }
        let handle = parts[1];

        if parts.get(2) == Some(&"post") {
            let rkey = parts.get(3)?;
            let (md, at_uri) = render_post_thread(handle, rkey, timeout).await?;
            return Some(build_result(
                &md,
                url,
                "bluesky-api",
                vec![format!("AT URI: {at_uri}")],
            ));
        }

        let profile_url = format!(
            "{API_BASE}/app.bsky.actor.getProfile?actor={}",
            super::util::percent_encode_component(handle)
        );
        let profile = fetch_json(&profile_url, timeout).await?;
        let md = render_profile(&profile);
        Some(build_result(
            &md,
            url,
            "bluesky-api",
            vec!["Fetched via AT Protocol API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn post_date_formats_to_us_style() {
        assert_eq!(
            format_post_date("2024-01-05T15:04:00Z"),
            "Jan 5, 2024, 03:04 PM"
        );
        assert_eq!(format_join_date("2024-01-05T15:04:00Z"), "January 5, 2024");
    }

    #[test]
    fn render_profile_lays_out_stats() {
        let profile = json!({
            "handle": "alice.bsky.social",
            "displayName": "Alice",
            "description": "Hello world",
            "followersCount": 1234,
            "followsCount": 56,
            "postsCount": 789,
            "did": "did:plc:abc",
            "createdAt": "2023-04-01T00:00:00Z"
        });
        let md = render_profile(&profile);
        assert!(md.contains("# Alice"));
        assert!(md.contains("**@alice.bsky.social**"));
        assert!(md.contains("- **Followers:** 1,234"));
        assert!(md.contains("- **Joined:** April 1, 2023"));
        assert!(md.contains("**DID:** `did:plc:abc`"));
    }

    #[test]
    fn format_post_shows_text_embed_and_stats() {
        let post = json!({
            "author": { "handle": "bob.bsky.social", "displayName": "Bob" },
            "record": { "text": "Check this", "createdAt": "2024-01-05T15:04:00Z" },
            "likeCount": 10,
            "replyCount": 2,
            "embed": {
                "$type": "app.bsky.embed.external#view",
                "external": { "uri": "https://example.com", "title": "Example", "description": "A site" }
            }
        });
        let md = format_post(&post, false);
        assert!(md.contains("**Bob** (@bob.bsky.social)"));
        assert!(md.contains("*Jan 5, 2024, 03:04 PM*"));
        assert!(md.contains("Check this"));
        assert!(md.contains("📎 [Example](https://example.com)"));
        assert!(md.contains("❤️ 10 • 💬 2"));
    }
}
