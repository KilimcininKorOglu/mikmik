// Stack Exchange handler: renders a question and its answers from the
// api.stackexchange.com API. Covers stackoverflow.com, the *.stackexchange.com
// subdomains, and the standalone network sites.

use super::util::{
    build_result, format_epoch_millis, html_to_markdown, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct StackOverflowHandler;

const MAX_ANSWERS: usize = 5;

static QUESTION_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/questions/(\d+)").expect("static stackexchange question regex"));
static SE_SUBDOMAIN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^([a-z0-9-]+)\.stackexchange\.com$").expect("static stackexchange subdomain regex")
});

/// Map a hostname to its Stack Exchange API `site` parameter, or `None`.
fn site_param(hostname: &str) -> Option<String> {
    let host = hostname.strip_prefix("www.").unwrap_or(hostname);
    let standalone = match host {
        "stackoverflow.com" => Some("stackoverflow"),
        "superuser.com" => Some("superuser"),
        "serverfault.com" => Some("serverfault"),
        "askubuntu.com" => Some("askubuntu"),
        "mathoverflow.net" => Some("mathoverflow"),
        "stackapps.com" => Some("stackapps"),
        _ => None,
    };
    if let Some(site) = standalone {
        return Some(site.to_string());
    }
    SE_SUBDOMAIN.captures(host).map(|caps| caps[1].to_string())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn owner_name(item: &Value) -> &str {
    item.get("owner")
        .and_then(|o| str_field(o, "display_name"))
        .unwrap_or("unknown")
}

fn creation_date(item: &Value) -> String {
    item.get("creation_date")
        .and_then(Value::as_i64)
        .map(|secs| format_epoch_millis(secs * 1000))
        .unwrap_or_default()
}

fn render_question(question: &Value) -> String {
    let title = str_field(question, "title").unwrap_or("Question");
    let mut md = format!("# {title}\n\n");
    let score = question.get("score").and_then(Value::as_i64).unwrap_or(0);
    let answers = question
        .get("answer_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let _ = write!(md, "**Score:** {score} · **Answers:** {answers}");
    if question.get("is_answered").and_then(Value::as_bool) == Some(true) {
        md.push_str(" (Answered)");
    }
    let tags: Vec<&str> = question
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let _ = write!(md, "\n**Tags:** {}\n", tags.join(", "));
    let _ = write!(
        md,
        "**Asked by:** {} · {}\n\n",
        owner_name(question),
        creation_date(question)
    );
    let body = html_to_markdown(str_field(question, "body").unwrap_or(""));
    let _ = write!(md, "---\n\n## Question\n\n{body}\n\n");
    md
}

fn append_answers(md: &mut String, answers: &[Value]) {
    if answers.is_empty() {
        return;
    }
    md.push_str("---\n\n## Answers\n\n");
    for answer in answers.iter().take(MAX_ANSWERS) {
        let score = answer.get("score").and_then(Value::as_i64).unwrap_or(0);
        let accepted = if answer.get("is_accepted").and_then(Value::as_bool) == Some(true) {
            " (Accepted)"
        } else {
            ""
        };
        let _ = write!(
            md,
            "### Score: {score}{accepted} · by {}\n\n",
            owner_name(answer)
        );
        let body = html_to_markdown(str_field(answer, "body").unwrap_or(""));
        let _ = write!(md, "{body}\n\n---\n\n");
    }
}

async fn fetch_items(url: &str, timeout: Duration) -> Vec<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    if !result.ok {
        return Vec::new();
    }
    serde_json::from_str::<Value>(&result.content)
        .ok()
        .and_then(|v| v.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

#[async_trait]
impl SpecialHandler for StackOverflowHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let site = site_param(parsed.host_str()?)?;
        let question_id = QUESTION_PATH.captures(parsed.path())?[1].to_string();

        let question_url = format!(
            "https://api.stackexchange.com/2.3/questions/{question_id}?order=desc&sort=votes&site={site}&filter=withbody"
        );
        let questions = fetch_items(&question_url, timeout).await;
        let question = questions.first()?;
        let mut md = render_question(question);

        let answers_url = format!(
            "https://api.stackexchange.com/2.3/questions/{question_id}/answers?order=desc&sort=votes&site={site}&filter=withbody"
        );
        let answers = fetch_items(&answers_url, timeout).await;
        append_answers(&mut md, &answers);

        Some(build_result(
            &md,
            url,
            "stackexchange",
            vec![format!("Fetched via Stack Exchange API (site={site})")],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn site_param_maps_hosts() {
        assert_eq!(
            site_param("stackoverflow.com").as_deref(),
            Some("stackoverflow")
        );
        assert_eq!(
            site_param("www.superuser.com").as_deref(),
            Some("superuser")
        );
        assert_eq!(
            site_param("unix.stackexchange.com").as_deref(),
            Some("unix")
        );
        assert_eq!(site_param("example.com"), None);
    }

    #[test]
    fn render_question_lays_out_metadata() {
        let question = json!({
            "title": "How to reverse a string?",
            "body": "<p>I want to <strong>reverse</strong>.</p>",
            "score": 42,
            "answer_count": 3,
            "is_answered": true,
            "tags": ["rust", "strings"],
            "owner": { "display_name": "Alice" },
            "creation_date": 1_609_459_200
        });
        let md = render_question(&question);
        assert!(md.contains("# How to reverse a string?"));
        assert!(md.contains("**Score:** 42 · **Answers:** 3 (Answered)"));
        assert!(md.contains("**Tags:** rust, strings"));
        assert!(md.contains("**Asked by:** Alice · 2021-01-01"));
        assert!(md.contains("## Question\n\nI want to **reverse**."));
    }

    #[test]
    fn answers_render_accepted_first() {
        let answers = vec![json!({
            "body": "<p>Use <code>chars().rev()</code></p>",
            "score": 100,
            "is_accepted": true,
            "owner": { "display_name": "Bob" }
        })];
        let mut md = String::new();
        append_answers(&mut md, &answers);
        assert!(md.contains("### Score: 100 (Accepted) · by Bob"));
        assert!(md.contains("`chars().rev()`"));
    }
}
