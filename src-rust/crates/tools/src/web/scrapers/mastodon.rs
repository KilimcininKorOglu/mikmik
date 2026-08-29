// Mastodon handler: renders a status or a profile from any Mastodon instance's
// v1 API, probing the instance endpoint to confirm the host.

use super::util::{
    build_result, format_number, html_to_markdown, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct MastodonHandler;

static POST_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/@([^/]+)/(\d+)$").expect("static mastodon post regex"));
static PROFILE_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/@([^/]+)$").expect("static mastodon profile regex"));

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn count(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Format an ISO timestamp as `Jan 5, 2024, 03:04 PM` (UTC), else the raw text.
fn format_date(iso: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => dt
            .with_timezone(&chrono::Utc)
            .format("%b %-d, %Y, %I:%M %p")
            .to_string(),
        Err(_) => iso.to_string(),
    }
}

fn display_name(account: &Value) -> &str {
    str_field(account, "display_name")
        .or_else(|| str_field(account, "username"))
        .unwrap_or("")
}

fn append_poll(md: &mut String, poll: &Value) {
    let Some(options) = poll.get("options").and_then(Value::as_array) else {
        return;
    };
    let total = count(poll, "votes_count");
    md.push_str("**Poll:**\n");
    for option in options {
        let title = str_field(option, "title").unwrap_or("");
        let votes = count(option, "votes_count");
        let pct = if total > 0 {
            format!("{:.1}", votes as f64 / total as f64 * 100.0)
        } else {
            "0".to_string()
        };
        let _ = writeln!(md, "- {title} ({pct}%, {votes} votes)");
    }
    let closed = if poll.get("expired").and_then(Value::as_bool) == Some(true) {
        " (closed)"
    } else {
        ""
    };
    let _ = write!(md, "Total: {total} votes{closed}\n\n");
}

fn append_attachments(md: &mut String, status: &Value) {
    let Some(media) = status
        .get("media_attachments")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())
    else {
        return;
    };
    md.push_str("**Attachments:**\n");
    for item in media {
        let kind = str_field(item, "type").unwrap_or("unknown");
        let url = str_field(item, "url").unwrap_or("");
        let desc = str_field(item, "description")
            .map(|d| format!(" - {d}"))
            .unwrap_or_default();
        let _ = writeln!(md, "- [{kind}]({url}){desc}");
    }
    md.push('\n');
}

fn format_status(status: &Value, is_reblog: bool) -> String {
    if let Some(reblog) = status.get("reblog").filter(|r| !r.is_null()) {
        if !is_reblog {
            let booster = status.get("account").map(display_name).unwrap_or("");
            return format!(
                "🔁 **{booster}** boosted:\n\n{}",
                format_status(reblog, true)
            );
        }
    }
    let account = status.get("account").cloned().unwrap_or(Value::Null);
    let mut md = String::new();
    if !is_reblog {
        let _ = write!(md, "# Post by {}\n\n", display_name(&account));
    }
    let _ = write!(md, "**@{}**", str_field(&account, "acct").unwrap_or(""));
    if account.get("bot").and_then(Value::as_bool) == Some(true) {
        md.push_str(" 🤖");
    }
    let _ = write!(
        md,
        " · {}",
        format_date(str_field(status, "created_at").unwrap_or(""))
    );
    if let Some(visibility) = str_field(status, "visibility").filter(|v| *v != "public") {
        let _ = write!(md, " · {visibility}");
    }
    md.push_str("\n\n");

    if let Some(spoiler) = str_field(status, "spoiler_text") {
        let _ = write!(md, "> ⚠️ **CW:** {spoiler}\n\n");
    }
    let _ = write!(
        md,
        "{}\n\n",
        html_to_markdown(str_field(status, "content").unwrap_or(""))
    );

    if let Some(poll) = status.get("poll").filter(|p| !p.is_null()) {
        append_poll(&mut md, poll);
    }
    append_attachments(&mut md, status);

    let _ = write!(
        md,
        "---\n💬 {} replies · 🔁 {} boosts · ⭐ {} favorites\n",
        format_number(count(status, "replies_count")),
        format_number(count(status, "reblogs_count")),
        format_number(count(status, "favourites_count"))
    );
    md
}

fn format_account(account: &Value) -> String {
    let mut md = format!("# {}\n\n", display_name(account));
    let _ = write!(md, "**@{}**", str_field(account, "acct").unwrap_or(""));
    if account.get("bot").and_then(Value::as_bool) == Some(true) {
        md.push_str(" 🤖 Bot");
    }
    md.push_str("\n\n");

    if let Some(note) = str_field(account, "note") {
        let bio = html_to_markdown(note);
        if !bio.is_empty() && bio != display_name(account) {
            let _ = write!(md, "{bio}\n\n");
        }
    }
    let _ = write!(
        md,
        "**Followers:** {} · **Following:** {} · **Posts:** {}\n\n",
        format_number(count(account, "followers_count")),
        format_number(count(account, "following_count")),
        format_number(count(account, "statuses_count"))
    );
    let _ = writeln!(
        md,
        "**Joined:** {}",
        format_date(str_field(account, "created_at").unwrap_or(""))
    );
    let _ = writeln!(
        md,
        "**Profile:** {}",
        str_field(account, "url").unwrap_or("")
    );

    if let Some(fields) = account
        .get("fields")
        .and_then(Value::as_array)
        .filter(|f| !f.is_empty())
    {
        md.push_str("\n**Profile Fields:**\n");
        for field in fields {
            let name = str_field(field, "name").unwrap_or("");
            let value = html_to_markdown(str_field(field, "value").unwrap_or(""));
            let _ = writeln!(md, "- **{name}:** {value}");
        }
    }
    md
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

/// Probe `/api/v1/instance` to confirm the host is a Mastodon-compatible server.
async fn is_mastodon_instance(instance: &str, timeout: Duration) -> bool {
    let url = format!("https://{instance}/api/v1/instance");
    match fetch_json(&url, timeout.min(Duration::from_secs(5))).await {
        Some(data) => {
            str_field(&data, "uri").is_some()
                || str_field(&data, "domain").is_some()
                || str_field(&data, "title").is_some()
        }
        None => false,
    }
}

fn append_recent_posts(md: &mut String, statuses: &[Value]) {
    if statuses.is_empty() {
        return;
    }
    md.push_str("\n---\n\n## Recent Posts\n\n");
    for status in statuses.iter().take(5) {
        let _ = write!(
            md,
            "### {}\n\n",
            format_date(str_field(status, "created_at").unwrap_or(""))
        );
        let _ = write!(
            md,
            "{}\n\n",
            html_to_markdown(str_field(status, "content").unwrap_or(""))
        );
        let _ = write!(
            md,
            "💬 {} · 🔁 {} · ⭐ {}\n\n",
            count(status, "replies_count"),
            count(status, "reblogs_count"),
            count(status, "favourites_count")
        );
    }
}

async fn handle_post(
    instance: &str,
    status_id: &str,
    url: &str,
    timeout: Duration,
) -> Option<RenderResult> {
    let api_url = format!("https://{instance}/api/v1/statuses/{status_id}");
    let status = fetch_json(&api_url, timeout).await?;
    let md = format_status(&status, false);
    Some(build_result(
        &md,
        url,
        "mastodon",
        vec![format!("Fetched via Mastodon API ({instance})")],
    ))
}

async fn handle_profile(
    instance: &str,
    username: &str,
    url: &str,
    timeout: Duration,
) -> Option<RenderResult> {
    let lookup_url = format!(
        "https://{instance}/api/v1/accounts/lookup?acct={}",
        super::util::percent_encode_component(username)
    );
    let account = fetch_json(&lookup_url, timeout).await?;
    let mut md = format_account(&account);

    if let Some(id) = str_field(&account, "id") {
        let statuses_url = format!(
            "https://{instance}/api/v1/accounts/{id}/statuses?limit=5&exclude_replies=true"
        );
        if let Some(statuses) = fetch_json(&statuses_url, timeout)
            .await
            .and_then(|v| v.as_array().cloned())
        {
            append_recent_posts(&mut md, &statuses);
        }
    }
    Some(build_result(
        &md,
        url,
        "mastodon",
        vec![format!("Fetched via Mastodon API ({instance})")],
    ))
}

#[async_trait]
impl SpecialHandler for MastodonHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let path = parsed.path();
        let post = POST_PATH.captures(path);
        let profile = PROFILE_PATH.captures(path);
        if post.is_none() && profile.is_none() {
            return None;
        }
        let instance = parsed.host_str()?;
        if !is_mastodon_instance(instance, timeout).await {
            return None;
        }
        if let Some(caps) = post {
            return handle_post(instance, &caps[2], url, timeout).await;
        }
        let caps = profile?;
        let username = super::util::percent_decode(&caps[1]);
        handle_profile(instance, &username, url, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_status_renders_content_and_stats() {
        let status = json!({
            "created_at": "2024-01-05T15:04:00Z",
            "visibility": "public",
            "content": "<p>Hello <strong>world</strong></p>",
            "account": { "acct": "alice@mastodon.social", "display_name": "Alice", "bot": false },
            "replies_count": 3,
            "reblogs_count": 5,
            "favourites_count": 10,
            "media_attachments": []
        });
        let md = format_status(&status, false);
        assert!(md.contains("# Post by Alice"));
        assert!(md.contains("**@alice@mastodon.social** · Jan 5, 2024, 03:04 PM"));
        assert!(md.contains("Hello **world**"));
        assert!(md.contains("💬 3 replies · 🔁 5 boosts · ⭐ 10 favorites"));
    }

    #[test]
    fn reblog_is_prefixed_with_booster() {
        let status = json!({
            "account": { "display_name": "Bob", "acct": "bob@x" },
            "reblog": {
                "created_at": "2024-01-05T15:04:00Z",
                "content": "<p>original</p>",
                "account": { "acct": "alice@y", "display_name": "Alice" },
                "media_attachments": []
            }
        });
        let md = format_status(&status, false);
        assert!(md.contains("🔁 **Bob** boosted:"));
        assert!(md.contains("original"));
    }

    #[test]
    fn format_account_lays_out_profile() {
        let account = json!({
            "acct": "alice@mastodon.social",
            "display_name": "Alice",
            "note": "<p>Rust dev</p>",
            "followers_count": 1200,
            "following_count": 150,
            "statuses_count": 3400,
            "created_at": "2020-01-01T00:00:00Z",
            "url": "https://mastodon.social/@alice",
            "bot": false
        });
        let md = format_account(&account);
        assert!(md.contains("# Alice"));
        assert!(md.contains("Rust dev"));
        assert!(md.contains("**Followers:** 1,200 · **Following:** 150 · **Posts:** 3,400"));
        assert!(md.contains("**Profile:** https://mastodon.social/@alice"));
    }
}
