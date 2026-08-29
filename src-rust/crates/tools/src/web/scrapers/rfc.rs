// RFC handler: renders an IETF RFC from the RFC Editor JSON metadata plus its
// plain-text body.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct RfcHandler;

static EDITOR: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)/rfc/rfc(\d+)(?:\.(?:html|txt|pdf))?$").expect("static rfc editor")
});
static DATATRACKER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)/doc/(?:html/)?rfc(\d+)/?$").expect("static rfc datatracker"));
static TOOLS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)/html/rfc(\d+)$").expect("static rfc tools"));
static BLANK_RUN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{4,}").expect("static rfc blank run"));
static PAGE_FOOTER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*\[Page \d+\]\s*$").expect("static rfc footer"));

fn extract_rfc_number(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    let path = url.path();
    let re = match host {
        "www.rfc-editor.org" | "rfc-editor.org" => &*EDITOR,
        "datatracker.ietf.org" => &*DATATRACKER,
        "tools.ietf.org" => &*TOOLS,
        _ => return None,
    };
    Some(re.captures(path)?[1].to_string())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn str_list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Strip RFC page headers, form feeds, and `[Page N]` footers from the text.
fn clean_rfc_text(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut cleaned: Vec<&str> = Vec::with_capacity(lines.len());
    let mut skip_next = 0;
    for line in lines {
        if skip_next > 0 {
            skip_next -= 1;
            continue;
        }
        if line.contains('\u{0c}') {
            // A form feed plus the following header lines.
            skip_next = 3;
            continue;
        }
        if PAGE_FOOTER.is_match(line) {
            continue;
        }
        cleaned.push(line);
    }
    BLANK_RUN
        .replace_all(&cleaned.join("\n"), "\n\n\n")
        .into_owned()
}

fn format_authors(metadata: &Value) -> Option<String> {
    let authors = metadata.get("authors").and_then(Value::as_array)?;
    let list: Vec<String> = authors
        .iter()
        .filter_map(|a| {
            let name = str_field(a, "name")?;
            Some(match str_field(a, "affiliation") {
                Some(aff) => format!("{name} ({aff})"),
                None => name.to_string(),
            })
        })
        .collect();
    (!list.is_empty()).then(|| list.join(", "))
}

fn append_metadata(md: &mut String, metadata: &Value, number: &str) {
    let title = str_field(metadata, "title").unwrap_or("");
    let _ = write!(md, "# RFC {number}: {title}\n\n");
    if let Some(authors) = format_authors(metadata) {
        let _ = writeln!(md, "**Authors:** {authors}");
    }
    for (field, label) in [
        ("pub_date", "Published"),
        ("current_status", "Status"),
        ("stream", "Stream"),
        ("area", "Area"),
        ("wg_acronym", "Working Group"),
    ] {
        if let Some(value) = str_field(metadata, field) {
            let _ = writeln!(md, "**{label}:** {value}");
        }
    }
    if let Some(pages) = metadata.get("page_count").and_then(Value::as_i64) {
        let _ = writeln!(md, "**Pages:** {pages}");
    }
    for (field, label) in [
        ("obsoletes", "Obsoletes"),
        ("obsoleted_by", "Obsoleted by"),
        ("updates", "Updates"),
        ("updated_by", "Updated by"),
        ("keywords", "Keywords"),
    ] {
        let list = str_list(metadata, field);
        if !list.is_empty() {
            let _ = writeln!(md, "**{label}:** {}", list.join(", "));
        }
    }
    if let Some(errata) = str_field(metadata, "errata_url") {
        let _ = writeln!(md, "**Errata:** {errata}");
    }
    md.push('\n');
    if let Some(abstract_text) = str_field(metadata, "abstract") {
        let _ = write!(md, "## Abstract\n\n{abstract_text}\n\n");
    }
    md.push_str("---\n\n");
}

fn render(metadata: Option<&Value>, text: &str, number: &str) -> String {
    let mut md = String::new();
    match metadata {
        Some(meta) => append_metadata(&mut md, meta, number),
        None => {
            let _ = write!(md, "# RFC {number}\n\n");
        }
    }
    md.push_str("## Full Text\n\n```\n");
    md.push_str(&clean_rfc_text(text));
    md.push_str("\n```\n");
    md
}

#[async_trait]
impl SpecialHandler for RfcHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let number = extract_rfc_number(&parsed)?;

        let metadata_url = format!("https://www.rfc-editor.org/rfc/rfc{number}.json");
        let text_url = format!("https://www.rfc-editor.org/rfc/rfc{number}.txt");
        let meta_opts = LoadOptions {
            timeout: timeout.min(Duration::from_secs(10)),
            ..Default::default()
        };
        let text_opts = LoadOptions {
            timeout,
            ..Default::default()
        };
        let (meta_result, text_result) = tokio::join!(
            load_page(&metadata_url, meta_opts),
            load_page(&text_url, text_opts)
        );

        if !text_result.ok {
            return None;
        }
        let metadata: Option<Value> = if meta_result.ok {
            serde_json::from_str(&meta_result.content).ok()
        } else {
            None
        };
        let note = if metadata.is_some() {
            "Metadata from RFC Editor JSON API"
        } else {
            "Metadata not available, showing plain text only"
        };
        let md = render(metadata.as_ref(), &text_result.content, &number);
        Some(build_result(&md, url, "rfc", vec![note.to_string()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn number_of(url: &str) -> Option<String> {
        extract_rfc_number(&url::Url::parse(url).expect("url"))
    }

    #[test]
    fn rfc_number_reads_all_host_shapes() {
        assert_eq!(
            number_of("https://www.rfc-editor.org/rfc/rfc2616.html"),
            Some("2616".to_string())
        );
        assert_eq!(
            number_of("https://datatracker.ietf.org/doc/html/rfc9110"),
            Some("9110".to_string())
        );
        assert_eq!(
            number_of("https://tools.ietf.org/html/rfc793"),
            Some("793".to_string())
        );
        assert_eq!(number_of("https://example.com/rfc/rfc1"), None);
    }

    #[test]
    fn clean_text_drops_form_feeds_and_footers() {
        let raw = "Line one\n\u{0c}\nHeader A\nHeader B\nHeader C\nLine two\n           [Page 5]\nLine three";
        let cleaned = clean_rfc_text(raw);
        assert!(cleaned.contains("Line one"));
        assert!(cleaned.contains("Line two"));
        assert!(cleaned.contains("Line three"));
        assert!(!cleaned.contains("[Page 5]"));
        assert!(!cleaned.contains("Header A"));
    }

    #[test]
    fn render_lays_out_metadata_and_body() {
        let metadata = json!({
            "title": "HTTP/1.1",
            "authors": [{ "name": "R. Fielding", "affiliation": "Adobe" }],
            "pub_date": "June 2014",
            "current_status": "PROPOSED STANDARD",
            "page_count": 89,
            "obsoletes": ["RFC2068"],
            "keywords": ["http"]
        });
        let md = render(Some(&metadata), "Body text.", "7230");
        assert!(md.contains("# RFC 7230: HTTP/1.1"));
        assert!(md.contains("**Authors:** R. Fielding (Adobe)"));
        assert!(md.contains("**Status:** PROPOSED STANDARD"));
        assert!(md.contains("**Pages:** 89"));
        assert!(md.contains("**Obsoletes:** RFC2068"));
        assert!(md.contains("## Full Text\n\n```\nBody text.\n```"));
    }
}
