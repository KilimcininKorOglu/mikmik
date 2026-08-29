// CrossRef handler: renders a DOI's metadata via the CrossRef works API.
//
// A `doi.org` URL resolves to its CrossRef record. The abstract ships as JATS
// XML; it is reduced to plain text by stripping tags and decoding entities
// (a full JATS-to-markdown conversion is left to the HTML-parse phase).

use super::util::{build_result, decode_html_entities, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct CrossrefHandler;

static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").expect("static crossref tag regex"));

fn doi(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_lowercase();
    if !matches!(host.as_str(), "doi.org" | "dx.doi.org" | "www.doi.org") {
        return None;
    }
    let raw = parsed.path().trim_start_matches('/');
    if raw.is_empty() {
        return None;
    }
    Some(super::util::percent_decode(raw))
}

fn first_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn format_authors(message: &Value) -> Option<String> {
    let authors = message.get("author").and_then(Value::as_array)?;
    let names: Vec<String> = authors
        .iter()
        .filter_map(|a| {
            if let Some(name) = str_field(a, "name") {
                return Some(name.to_string());
            }
            let parts: Vec<&str> = [str_field(a, "given"), str_field(a, "family")]
                .into_iter()
                .flatten()
                .collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        })
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// The `YYYY-MM-DD` from a CrossRef `date-parts` value, zero-padded.
fn format_date(date: &Value) -> Option<String> {
    let parts = date.get("date-parts")?.as_array()?.first()?.as_array()?;
    let year = parts.first()?.as_i64()?;
    let mut out = year.to_string();
    if let Some(month) = parts.get(1).and_then(Value::as_i64) {
        let _ = write!(out, "-{month:02}");
        if let Some(day) = parts.get(2).and_then(Value::as_i64) {
            let _ = write!(out, "-{day:02}");
        }
    }
    Some(out)
}

/// The first present date across CrossRef's several date fields.
fn best_date(message: &Value) -> Option<String> {
    [
        "published",
        "published-print",
        "published-online",
        "issued",
        "created",
    ]
    .iter()
    .filter_map(|key| message.get(key).and_then(format_date))
    .next()
}

fn clean_abstract(raw: &str) -> Option<String> {
    let stripped = TAG.replace_all(raw, " ");
    let decoded = decode_html_entities(&stripped);
    let text = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

fn render(message: &Value, fallback_doi: &str) -> String {
    let title = first_str(message, "title")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("CrossRef Record");
    let mut md = format!("# {title}\n\n");
    if let Some(authors) = format_authors(message) {
        let _ = writeln!(md, "**Authors:** {authors}");
    }
    if let Some(journal) = first_str(message, "container-title")
        .or_else(|| first_str(message, "short-container-title"))
    {
        let _ = writeln!(md, "**Journal:** {journal}");
    }
    if let Some(publisher) = str_field(message, "publisher") {
        let _ = writeln!(md, "**Publisher:** {publisher}");
    }
    if let Some(published) = best_date(message) {
        let _ = writeln!(md, "**Published:** {published}");
    }
    let doi_value = str_field(message, "DOI").unwrap_or(fallback_doi);
    let _ = writeln!(md, "**DOI:** {doi_value}");
    if let Some(kind) = str_field(message, "type") {
        let _ = writeln!(md, "**Type:** {}", kind.replace('-', " "));
    }
    md.push_str("\n---\n\n## Abstract\n\n");
    match message
        .get("abstract")
        .and_then(Value::as_str)
        .and_then(clean_abstract)
    {
        Some(abstract_) => md.push_str(&abstract_),
        None => md.push_str("No abstract available."),
    }
    md.push('\n');
    md
}

#[async_trait]
impl SpecialHandler for CrossrefHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let doi = doi(url)?;
        let api_url = format!(
            "https://api.crossref.org/works/{}",
            super::util::percent_encode_component(&doi)
        );
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                headers: vec![("Accept".to_string(), "application/json".to_string())],
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let message = data.get("message")?;
        let md = render(message, &doi);
        Some(build_result(
            &md,
            url,
            "crossref",
            vec!["Fetched via CrossRef API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn doi_reads_from_doi_hosts_only() {
        assert_eq!(
            doi("https://doi.org/10.1000/xyz123").as_deref(),
            Some("10.1000/xyz123")
        );
        assert_eq!(
            doi("https://dx.doi.org/10.1/abc").as_deref(),
            Some("10.1/abc")
        );
        assert!(doi("https://example.com/10.1/abc").is_none());
        assert!(doi("https://doi.org/").is_none());
    }

    #[test]
    fn dates_pad_month_and_day() {
        let date = json!({ "date-parts": [[2021, 3, 5]] });
        assert_eq!(format_date(&date).as_deref(), Some("2021-03-05"));
        let year_only = json!({ "date-parts": [[2020]] });
        assert_eq!(format_date(&year_only).as_deref(), Some("2020"));
    }

    #[test]
    fn render_lays_out_the_record() {
        let message = json!({
            "title": ["A Great Paper"],
            "author": [{ "given": "Ada", "family": "Lovelace" }, { "name": "Anon" }],
            "container-title": ["Journal of Things"],
            "publisher": "ACME",
            "issued": { "date-parts": [[2019, 6]] },
            "DOI": "10.1000/xyz",
            "type": "journal-article",
            "abstract": "<jats:p>Hello &amp; <b>world</b></jats:p>"
        });
        let md = render(&message, "10.1000/xyz");
        assert!(md.contains("# A Great Paper"));
        assert!(md.contains("**Authors:** Ada Lovelace, Anon"));
        assert!(md.contains("**Journal:** Journal of Things"));
        assert!(md.contains("**Published:** 2019-06"));
        assert!(md.contains("**DOI:** 10.1000/xyz"));
        assert!(md.contains("**Type:** journal article"));
        assert!(md.contains("## Abstract\n\nHello & world"));
    }
}
