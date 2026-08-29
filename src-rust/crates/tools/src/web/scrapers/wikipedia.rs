// Wikipedia handler: renders an article from the REST summary API plus the
// mobile-html section content.

use super::dom;
use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use scraper::Selector;
use std::fmt::Write;
use std::time::Duration;

pub struct WikipediaHandler;

const MIN_PARAGRAPH_LEN: usize = 20;
const SKIP_HEADINGS: [&str; 5] = [
    "References",
    "External links",
    "See also",
    "Notes",
    "Further reading",
];

static HOST: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\w+)\.wikipedia\.org$").expect("static wikipedia host regex"));
static TITLE_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/wiki/(.+)").expect("static wikipedia title regex"));
static SECTION: Lazy<Selector> = Lazy::new(|| dom::selector("section"));
static HEADING: Lazy<Selector> = Lazy::new(|| dom::selector("h2, h3, h4"));
static PARAGRAPH: Lazy<Selector> = Lazy::new(|| dom::selector("p"));

struct Article {
    lang: String,
    title: String,
}

fn parse_url(url: &str) -> Option<Article> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let lang = HOST.captures(host)?[1].to_string();
    let raw_title = TITLE_PATH.captures(parsed.path())?[1].to_string();
    Some(Article {
        lang,
        title: super::util::percent_decode(&raw_title),
    })
}

async fn fetch_text(url: &str, timeout: Duration) -> Option<String> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    result.ok.then_some(result.content)
}

fn append_summary(md: &mut String, content: &str) {
    let Ok(summary) = serde_json::from_str::<serde_json::Value>(content) else {
        return;
    };
    let title = summary
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let _ = write!(md, "# {title}\n\n");
    if let Some(description) = summary
        .get("description")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let _ = write!(md, "*{description}*\n\n");
    }
    if let Some(extract) = summary.get("extract").and_then(serde_json::Value::as_str) {
        let _ = write!(md, "{extract}\n\n---\n\n");
    }
}

fn append_sections(md: &mut String, html: &str) {
    let doc = dom::parse(html);
    for section in doc.select(&SECTION) {
        let heading = section.select(&HEADING).next();
        let heading_text = heading.map(dom::text).filter(|t| !t.is_empty());
        if let Some(text) = &heading_text {
            if SKIP_HEADINGS.contains(&text.as_str()) {
                continue;
            }
            let level = if dom::tag_name(heading.expect("heading present")) == "h2" {
                "##"
            } else {
                "###"
            };
            let _ = write!(md, "{level} {text}\n\n");
        }
        for paragraph in section.select(&PARAGRAPH) {
            let text = dom::text(paragraph);
            if text.chars().count() > MIN_PARAGRAPH_LEN {
                let _ = write!(md, "{text}\n\n");
            }
        }
    }
}

#[async_trait]
impl SpecialHandler for WikipediaHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let article = parse_url(url)?;
        let encoded = super::util::percent_encode_component(&article.title);
        let mut md = String::new();

        let summary_url = format!(
            "https://{}.wikipedia.org/api/rest_v1/page/summary/{encoded}",
            article.lang
        );
        if let Some(content) = fetch_text(&summary_url, timeout).await {
            append_summary(&mut md, &content);
        }

        let content_url = format!(
            "https://{}.wikipedia.org/api/rest_v1/page/mobile-html/{encoded}",
            article.lang
        );
        if let Some(html) = fetch_text(&content_url, timeout).await {
            append_sections(&mut md, &html);
        }

        if md.is_empty() {
            return None;
        }
        Some(build_result(
            &md,
            url,
            "wikipedia",
            vec!["Fetched via Wikipedia API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_lang_and_title() {
        let article =
            parse_url("https://en.wikipedia.org/wiki/Rust_(programming_language)").unwrap();
        assert_eq!(article.lang, "en");
        assert_eq!(article.title, "Rust_(programming_language)");
        assert!(parse_url("https://example.com/wiki/X").is_none());
    }

    #[test]
    fn summary_renders_title_and_extract() {
        let mut md = String::new();
        let content = r#"{"title":"Rust","description":"programming language","extract":"Rust is a language."}"#;
        append_summary(&mut md, content);
        assert!(md.contains("# Rust"));
        assert!(md.contains("*programming language*"));
        assert!(md.contains("Rust is a language."));
    }

    #[test]
    fn sections_skip_reference_headings() {
        let html = r#"<html><body>
            <section><h2>Overview</h2><p>This is a sufficiently long paragraph of text.</p></section>
            <section><h2>References</h2><p>Some long reference paragraph text here now.</p></section>
        </body></html>"#;
        let mut md = String::new();
        append_sections(&mut md, html);
        assert!(md.contains("## Overview"));
        assert!(md.contains("sufficiently long paragraph"));
        assert!(!md.contains("## References"));
    }
}
