// Twitter/X handler: renders a tweet through public Nitter instances, since
// x.com blocks automated access. Falls back to a helpful message when every
// instance is unavailable.

use super::dom;
use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use scraper::{ElementRef, Html, Selector};
use std::fmt::Write;
use std::time::Duration;

pub struct TwitterHandler;

const MIN_CONTENT_LEN: usize = 500;
const MAX_REPLIES: usize = 9;
const NITTER_INSTANCES: [&str; 4] = [
    "nitter.privacyredirect.com",
    "nitter.tiekoetter.com",
    "nitter.poast.org",
    "nitter.woodland.cafe",
];
const BLOCKED_MESSAGE: &str = "Twitter/X blocks automated access. Nitter instances were unavailable.\n\nTry:\n- Opening the link in a browser\n- Using a different Nitter instance manually\n- Checking if the tweet is available via an archive service";

static TWEET_CONTENT: Lazy<Selector> = Lazy::new(|| dom::selector(".tweet-content"));
static FULLNAME: Lazy<Selector> = Lazy::new(|| dom::selector(".fullname"));
static USERNAME: Lazy<Selector> = Lazy::new(|| dom::selector(".username"));
static TWEET_DATE: Lazy<Selector> = Lazy::new(|| dom::selector(".tweet-date a"));
static TWEET_STATS: Lazy<Selector> = Lazy::new(|| dom::selector(".tweet-stats"));
static REPLY_CONTENT: Lazy<Selector> = Lazy::new(|| dom::selector(".timeline-item .tweet-content"));

fn is_twitter_host(host: &str) -> bool {
    matches!(
        host,
        "twitter.com" | "x.com" | "www.twitter.com" | "www.x.com"
    )
}

fn first_text(doc: &Html, sel: &Selector) -> Option<String> {
    doc.select(sel)
        .next()
        .map(dom::text)
        .filter(|t| !t.is_empty())
}

fn append_thread(md: &mut String, doc: &Html) {
    let replies: Vec<ElementRef<'_>> = doc.select(&REPLY_CONTENT).collect();
    if replies.len() <= 1 {
        return;
    }
    md.push_str("\n---\n\n## Thread/Replies\n\n");
    for reply in replies.iter().skip(1).take(MAX_REPLIES) {
        let user = reply
            .parent()
            .and_then(ElementRef::wrap)
            .and_then(|parent| parent.select(&USERNAME).next())
            .map(dom::text)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "@?".to_string());
        let _ = write!(md, "**{user}**: {}\n\n", dom::text(*reply));
    }
}

fn render_tweet(html: &str) -> Option<String> {
    let doc = dom::parse(html);
    let content = first_text(&doc, &TWEET_CONTENT)?;
    let fullname = first_text(&doc, &FULLNAME).unwrap_or_else(|| "Unknown".to_string());
    let username = first_text(&doc, &USERNAME).unwrap_or_else(|| "@?".to_string());
    let mut md = format!("# Tweet by {fullname} ({username})\n\n");
    if let Some(date) = first_text(&doc, &TWEET_DATE) {
        let _ = write!(md, "*{date}*\n\n");
    }
    let _ = write!(md, "{content}\n\n");
    if let Some(stats) = doc.select(&TWEET_STATS).next().map(dom::collapsed_text) {
        if !stats.is_empty() {
            let _ = write!(md, "---\n{stats}\n");
        }
    }
    append_thread(&mut md, &doc);
    Some(md)
}

async fn fetch_nitter(instance: &str, path: &str, timeout: Duration) -> Option<String> {
    let url = format!("https://{instance}{path}");
    let result = load_page(
        &url,
        LoadOptions {
            timeout: timeout.min(Duration::from_secs(10)),
            ..Default::default()
        },
    )
    .await;
    (result.ok && result.content.len() > MIN_CONTENT_LEN).then_some(result.content)
}

#[async_trait]
impl SpecialHandler for TwitterHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        if !is_twitter_host(parsed.host_str()?) {
            return None;
        }
        let path = parsed.path();
        for instance in NITTER_INSTANCES {
            let Some(html) = fetch_nitter(instance, path, timeout).await else {
                continue;
            };
            if let Some(md) = render_tweet(&html) {
                return Some(build_result(
                    &md,
                    url,
                    "twitter-nitter",
                    vec![format!("Via Nitter: {instance}")],
                ));
            }
        }
        Some(build_result(
            BLOCKED_MESSAGE,
            url,
            "twitter-blocked",
            vec!["X.com blocks bots; Nitter instances unavailable".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_twitter_hosts() {
        assert!(is_twitter_host("twitter.com"));
        assert!(is_twitter_host("x.com"));
        assert!(is_twitter_host("www.x.com"));
        assert!(!is_twitter_host("example.com"));
    }

    #[test]
    fn render_tweet_extracts_fields() {
        let html = r#"<html><body>
            <div class="fullname">Jane Doe</div>
            <div class="username">@jane</div>
            <div class="tweet-date"><a>Jan 1</a></div>
            <div class="tweet-content">Hello world</div>
            <div class="tweet-stats">  5   replies   </div>
        </body></html>"#;
        let md = render_tweet(html).unwrap();
        assert!(md.contains("# Tweet by Jane Doe (@jane)"));
        assert!(md.contains("*Jan 1*"));
        assert!(md.contains("Hello world"));
        assert!(md.contains("---\n5 replies"));
    }

    #[test]
    fn thread_lists_replies_after_first() {
        let html = r#"<html><body>
            <div class="timeline-item"><div class="username">@a</div><div class="tweet-content">main</div></div>
            <div class="timeline-item"><div class="username">@b</div><div class="tweet-content">reply one</div></div>
        </body></html>"#;
        let md = render_tweet(html).unwrap();
        assert!(md.contains("## Thread/Replies"));
        assert!(md.contains("**@b**: reply one"));
        assert!(!md.contains("**@a**: main"));
    }
}
