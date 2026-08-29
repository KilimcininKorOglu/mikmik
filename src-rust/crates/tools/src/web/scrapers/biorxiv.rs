// bioRxiv/medRxiv handler: renders a preprint from the *rxiv details API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct BiorxivHandler;

static CONTENT_DOI: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/content/(10\.\d{4,}/[^\s?#]+)").expect("static biorxiv regex"));
static VERSION_SUFFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"v\d+$").expect("static biorxiv version regex"));
static FULL_SUFFIX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\.full(\.pdf)?$").expect("static biorxiv full regex"));

/// Which preprint server a URL names, or `None` for an unrelated host.
fn server_for(host: &str) -> Option<&'static str> {
    match host.to_lowercase().as_str() {
        "biorxiv.org" | "www.biorxiv.org" => Some("biorxiv"),
        "medrxiv.org" | "www.medrxiv.org" => Some("medrxiv"),
        _ => None,
    }
}

/// Extract and normalize the DOI (stripping `vN` and `.full[.pdf]` suffixes).
fn parse_doi(path: &str) -> Option<String> {
    let raw = &CONTENT_DOI.captures(path)?[1];
    let without_full = FULL_SUFFIX.replace(raw, "");
    let without_version = VERSION_SUFFIX.replace(&without_full, "");
    Some(without_version.into_owned())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn append_metadata(md: &mut String, paper: &Value, doi: &str, server_name: &str) {
    if let Some(authors) = str_field(paper, "authors") {
        let _ = writeln!(md, "**Authors:** {authors}");
    }
    if let Some(corresponding) = str_field(paper, "author_corresponding") {
        let _ = write!(md, "**Corresponding Author:** {corresponding}");
        if let Some(institution) = str_field(paper, "author_corresponding_institution") {
            let _ = write!(md, " ({institution})");
        }
        md.push('\n');
    }
    for (field, label) in [
        ("date", "Posted"),
        ("category", "Category"),
        ("version", "Version"),
        ("license", "License"),
    ] {
        if let Some(value) = str_field(paper, field) {
            let _ = writeln!(md, "**{label}:** {value}");
        }
    }
    let _ = writeln!(md, "**DOI:** [{doi}](https://doi.org/{doi})");
    let _ = writeln!(md, "**Server:** {server_name}");
}

fn render(paper: &Value, doi: &str, server: &str, server_name: &str) -> String {
    let paper_doi = str_field(paper, "biorxiv_doi")
        .or_else(|| str_field(paper, "medrxiv_doi"))
        .unwrap_or(doi);
    let mut md = format!(
        "# {}\n\n",
        str_field(paper, "title").unwrap_or("Untitled Preprint")
    );
    append_metadata(&mut md, paper, paper_doi, server_name);

    if let Some(published) = str_field(paper, "published") {
        let _ = write!(
            md,
            "\n> **Published in journal:** [{published}](https://doi.org/{published})\n"
        );
    }

    let abstract_text = str_field(paper, "abstract").unwrap_or("No abstract available.");
    let _ = write!(md, "\n---\n\n## Abstract\n\n{abstract_text}\n");

    let _ = write!(md, "\n---\n\n## Links\n\n");
    let _ = writeln!(
        md,
        "- [View on {server_name}](https://www.{server}.org/content/{paper_doi})"
    );
    let _ = writeln!(
        md,
        "- [PDF](https://www.{server}.org/content/{paper_doi}.full.pdf)"
    );
    if let Some(jats) = str_field(paper, "jatsxml") {
        let _ = writeln!(md, "- [JATS XML]({jats})");
    }
    md
}

#[async_trait]
impl SpecialHandler for BiorxivHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let server = server_for(parsed.host_str()?)?;
        let doi = parse_doi(parsed.path())?;
        let server_name = if server == "biorxiv" {
            "bioRxiv"
        } else {
            "medRxiv"
        };

        let api_url = format!("https://api.{server}.org/details/{server}/{doi}/na/json");
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
        let collection = data.get("collection").and_then(Value::as_array)?;
        let paper = collection.last()?;

        let md = render(paper, &doi, server, server_name);
        Some(build_result(
            &md,
            url,
            server,
            vec![format!("Fetched via {server_name} API")],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_doi_strips_version_and_full_suffixes() {
        assert_eq!(
            parse_doi("/content/10.1101/2024.01.01.123456v2"),
            Some("10.1101/2024.01.01.123456".to_string())
        );
        assert_eq!(
            parse_doi("/content/10.1101/2024.01.01.123456.full.pdf"),
            Some("10.1101/2024.01.01.123456".to_string())
        );
        assert_eq!(parse_doi("/about"), None);
    }

    #[test]
    fn server_for_maps_known_hosts() {
        assert_eq!(server_for("www.biorxiv.org"), Some("biorxiv"));
        assert_eq!(server_for("medrxiv.org"), Some("medrxiv"));
        assert_eq!(server_for("example.com"), None);
    }

    #[test]
    fn render_lays_out_preprint() {
        let paper = json!({
            "title": "A Preprint",
            "authors": "Doe J., Smith A.",
            "author_corresponding": "Doe J.",
            "author_corresponding_institution": "MIT",
            "date": "2024-01-01",
            "category": "genomics",
            "version": "2",
            "biorxiv_doi": "10.1101/2024.01.01.123456",
            "abstract": "We show things.",
            "published": "10.1000/journal.1"
        });
        let md = render(&paper, "10.1101/2024.01.01.123456", "biorxiv", "bioRxiv");
        assert!(md.contains("# A Preprint"));
        assert!(md.contains("**Authors:** Doe J., Smith A."));
        assert!(md.contains("**Corresponding Author:** Doe J. (MIT)"));
        assert!(md.contains(
            "**DOI:** [10.1101/2024.01.01.123456](https://doi.org/10.1101/2024.01.01.123456)"
        ));
        assert!(md.contains(
            "> **Published in journal:** [10.1000/journal.1](https://doi.org/10.1000/journal.1)"
        ));
        assert!(md.contains("## Abstract\n\nWe show things."));
        assert!(md.contains(
            "- [View on bioRxiv](https://www.biorxiv.org/content/10.1101/2024.01.01.123456)"
        ));
    }
}
