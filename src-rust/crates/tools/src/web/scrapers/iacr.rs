// IACR ePrint handler: renders a paper's metadata and abstract from the
// eprint.iacr.org HTML page (CSS selectors plus citation meta tags).

use super::dom;
use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use std::fmt::Write;
use std::time::Duration;

pub struct IacrHandler;

static ID_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/(\d{4})/(\d+)(?:\.pdf)?$").expect("static iacr id regex"));
static H3_TITLE: Lazy<Selector> = Lazy::new(|| dom::selector("h3.mb-3"));
static META_TITLE: Lazy<Selector> = Lazy::new(|| dom::selector(r#"meta[name="citation_title"]"#));
static META_AUTHOR: Lazy<Selector> = Lazy::new(|| dom::selector(r#"meta[name="citation_author"]"#));
static META_DESC: Lazy<Selector> = Lazy::new(|| dom::selector(r#"meta[name="description"]"#));
static META_PUBDATE: Lazy<Selector> =
    Lazy::new(|| dom::selector(r#"meta[name="citation_publication_date"]"#));
static H5: Lazy<Selector> = Lazy::new(|| dom::selector("h5"));
static P: Lazy<Selector> = Lazy::new(|| dom::selector("p"));
static KEYWORDS: Lazy<Selector> = Lazy::new(|| dom::selector(".keywords"));

fn parse_paper_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "eprint.iacr.org" {
        return None;
    }
    let caps = ID_PATH.captures(parsed.path())?;
    Some(format!("{}/{}", &caps[1], &caps[2]))
}

fn meta_content<'a>(doc: &'a Html, sel: &Selector) -> Option<&'a str> {
    doc.select(sel).next().and_then(|m| dom::attr(m, "content"))
}

fn title_of(doc: &Html) -> Option<String> {
    doc.select(&H3_TITLE)
        .next()
        .map(dom::text)
        .filter(|t| !t.is_empty())
        .or_else(|| meta_content(doc, &META_TITLE).map(str::to_string))
}

fn authors_of(doc: &Html) -> Vec<String> {
    doc.select(&META_AUTHOR)
        .filter_map(|m| dom::attr(m, "content").map(str::to_string))
        .collect()
}

/// Abstract text: the `<p>` under the `<h5>Abstract</h5>` heading, else the
/// page's meta description.
fn abstract_of(doc: &Html) -> Option<String> {
    for heading in doc.select(&H5) {
        if !dom::text(heading).contains("Abstract") {
            continue;
        }
        let paragraph = heading
            .parent()
            .and_then(ElementRef::wrap)
            .and_then(|parent| parent.select(&P).next())
            .map(dom::text)
            .filter(|t| !t.is_empty());
        if let Some(text) = paragraph {
            return Some(text);
        }
    }
    meta_content(doc, &META_DESC).map(str::to_string)
}

fn keywords_of(doc: &Html) -> Option<String> {
    let text = doc.select(&KEYWORDS).next().map(dom::text)?;
    let trimmed = text.trim_start_matches("Keywords:").trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn render(doc: &Html, paper_id: &str) -> String {
    let title = title_of(doc).unwrap_or_else(|| "IACR ePrint Paper".to_string());
    let mut md = format!("# {title}\n\n");
    let authors = authors_of(doc);
    if !authors.is_empty() {
        let _ = writeln!(md, "**Authors:** {}", authors.join(", "));
    }
    if let Some(date) = meta_content(doc, &META_PUBDATE) {
        let _ = writeln!(md, "**Date:** {date}");
    }
    let _ = writeln!(md, "**ePrint:** {paper_id}");
    if let Some(keywords) = keywords_of(doc) {
        let _ = writeln!(md, "**Keywords:** {keywords}");
    }
    let abstract_text = abstract_of(doc).unwrap_or_else(|| "No abstract available.".to_string());
    let _ = write!(md, "\n---\n\n## Abstract\n\n{abstract_text}\n\n");
    md
}

#[async_trait]
impl SpecialHandler for IacrHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let paper_id = parse_paper_id(url)?;
        let page_url = format!("https://eprint.iacr.org/{paper_id}");
        let result = load_page(
            &page_url,
            LoadOptions {
                timeout,
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let doc = dom::parse(&result.content);
        let md = render(&doc, &paper_id);
        Some(build_result(
            &md,
            url,
            "iacr",
            vec!["Fetched from IACR ePrint Archive".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id_from_path() {
        assert_eq!(
            parse_paper_id("https://eprint.iacr.org/2023/1234"),
            Some("2023/1234".to_string())
        );
        assert_eq!(
            parse_paper_id("https://eprint.iacr.org/2023/1234.pdf"),
            Some("2023/1234".to_string())
        );
        assert_eq!(parse_paper_id("https://example.com/2023/1"), None);
    }

    #[test]
    fn render_reads_meta_and_abstract() {
        let html = r#"<html><head>
            <meta name="citation_author" content="Alice">
            <meta name="citation_author" content="Bob">
            <meta name="citation_publication_date" content="2023-08-01">
            </head><body>
            <h3 class="mb-3">A New Scheme</h3>
            <div><h5>Abstract</h5><p>We build a scheme.</p></div>
            <div class="keywords">Keywords: crypto, zk</div>
            </body></html>"#;
        let doc = dom::parse(html);
        let md = render(&doc, "2023/1234");
        assert!(md.contains("# A New Scheme"));
        assert!(md.contains("**Authors:** Alice, Bob"));
        assert!(md.contains("**Date:** 2023-08-01"));
        assert!(md.contains("**ePrint:** 2023/1234"));
        assert!(md.contains("**Keywords:** crypto, zk"));
        assert!(md.contains("## Abstract\n\nWe build a scheme."));
    }
}
