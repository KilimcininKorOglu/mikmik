// arXiv handler: renders a paper's metadata and abstract from the arXiv Atom
// API. The Atom feed is XML, so the fields are read with targeted patterns
// rather than an HTML DOM parse.

use super::util::{build_result, decode_html_entities, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use std::fmt::Write;
use std::time::Duration;

pub struct ArxivHandler;

static ID_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/(abs|pdf)/(.+?)(?:\.pdf)?$").expect("static arxiv id regex"));
static ENTRY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)<entry>(.*?)</entry>").expect("static arxiv entry regex"));
static TITLE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)<title>(.*?)</title>").expect("static arxiv title regex"));
static SUMMARY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)<summary>(.*?)</summary>").expect("static arxiv summary regex"));
static NAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)<name>(.*?)</name>").expect("static arxiv name regex"));
static PUBLISHED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?s)<published>(.*?)</published>").expect("static arxiv published regex")
});
static CATEGORY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"<category[^>]*\bterm="([^"]*)""#).expect("static arxiv category regex")
});
static PDF_LINK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"<link\b[^>]*>"#).expect("static arxiv link regex"));
static HREF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"href="([^"]*)""#).expect("static arxiv href regex"));

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_paper_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "arxiv.org" {
        return None;
    }
    let caps = ID_PATH.captures(parsed.path())?;
    Some(caps[2].to_string())
}

fn first_capture(re: &Regex, haystack: &str) -> Option<String> {
    re.captures(haystack)
        .map(|c| decode_html_entities(c[1].trim()))
        .filter(|s| !s.is_empty())
}

fn pdf_href(entry: &str) -> Option<String> {
    for link in PDF_LINK.find_iter(entry) {
        let tag = link.as_str();
        if tag.contains("title=\"pdf\"") {
            if let Some(href) = HREF.captures(tag) {
                return Some(href[1].to_string());
            }
        }
    }
    None
}

fn render(entry: &str, paper_id: &str) -> String {
    let title = first_capture(&TITLE, entry).map(|t| collapse_ws(&t));
    let mut md = format!("# {}\n\n", title.as_deref().unwrap_or("arXiv Paper"));

    let authors: Vec<String> = NAME
        .captures_iter(entry)
        .map(|c| decode_html_entities(c[1].trim()))
        .filter(|s| !s.is_empty())
        .collect();
    if !authors.is_empty() {
        let _ = writeln!(md, "**Authors:** {}", authors.join(", "));
    }
    if let Some(published) = first_capture(&PUBLISHED, entry) {
        let date = published.split('T').next().unwrap_or(&published);
        let _ = writeln!(md, "**Published:** {date}");
    }
    let categories: Vec<String> = CATEGORY
        .captures_iter(entry)
        .map(|c| c[1].to_string())
        .collect();
    if !categories.is_empty() {
        let _ = writeln!(md, "**Categories:** {}", categories.join(", "));
    }
    let _ = write!(md, "**arXiv:** {paper_id}\n\n");
    let summary = first_capture(&SUMMARY, entry)
        .map(|s| collapse_ws(&s))
        .unwrap_or_else(|| "No abstract available.".to_string());
    let _ = write!(md, "---\n\n## Abstract\n\n{summary}\n\n");
    md
}

#[async_trait]
impl SpecialHandler for ArxivHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let paper_id = parse_paper_id(url)?;
        let api_url = format!("https://export.arxiv.org/api/query?id_list={paper_id}");
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let entry = ENTRY.captures(&result.content)?[1].to_string();
        let _ = pdf_href(&entry); // PDF full-text conversion is out of scope.
        let md = render(&entry, &paper_id);
        Some(build_result(
            &md,
            url,
            "arxiv",
            vec!["Fetched via arXiv API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id_from_abs_and_pdf() {
        assert_eq!(
            parse_paper_id("https://arxiv.org/abs/1234.56789"),
            Some("1234.56789".to_string())
        );
        assert_eq!(
            parse_paper_id("https://arxiv.org/pdf/cs/0123456.pdf"),
            Some("cs/0123456".to_string())
        );
        assert_eq!(parse_paper_id("https://example.com/abs/1"), None);
    }

    #[test]
    fn render_lays_out_paper() {
        let entry = r#"
            <title>  Attention Is
            All You Need </title>
            <summary> We propose a new architecture. </summary>
            <author><name>Ashish Vaswani</name></author>
            <author><name>Noam Shazeer</name></author>
            <published>2017-06-12T00:00:00Z</published>
            <category term="cs.CL" scheme="http://arxiv.org/schemas/atom"/>
            <category term="cs.LG"/>
            <link title="pdf" href="https://arxiv.org/pdf/1706.03762"/>
        "#;
        let md = render(entry, "1706.03762");
        assert!(md.contains("# Attention Is All You Need"));
        assert!(md.contains("**Authors:** Ashish Vaswani, Noam Shazeer"));
        assert!(md.contains("**Published:** 2017-06-12"));
        assert!(md.contains("**Categories:** cs.CL, cs.LG"));
        assert!(md.contains("**arXiv:** 1706.03762"));
        assert!(md.contains("## Abstract\n\nWe propose a new architecture."));
    }

    #[test]
    fn pdf_href_extracts_link() {
        let entry = r#"<link title="pdf" href="https://arxiv.org/pdf/x"/>"#;
        assert_eq!(pdf_href(entry).as_deref(), Some("https://arxiv.org/pdf/x"));
    }
}
