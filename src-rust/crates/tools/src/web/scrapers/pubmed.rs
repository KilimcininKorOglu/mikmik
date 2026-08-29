// PubMed handler: renders an article's metadata, abstract, and MeSH terms via
// the NCBI E-utilities API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct PubMedHandler;

const USER_AGENT: &str = concat!("mikmik/", env!("CARGO_PKG_VERSION"));

static PMID_DIRECT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/(\d+)").expect("static pubmed direct regex"));
static PMID_LEGACY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/pubmed/(\d+)").expect("static pubmed legacy regex"));

fn parse_pmid(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let path = parsed.path();
    if host == "pubmed.ncbi.nlm.nih.gov" {
        return Some(PMID_DIRECT.captures(path)?[1].to_string());
    }
    if host == "ncbi.nlm.nih.gov" && path.starts_with("/pubmed") {
        return Some(PMID_LEGACY.captures(path)?[1].to_string());
    }
    None
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

async fn fetch(url: &str, timeout: Duration, accept: &str) -> Option<String> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            headers: vec![
                ("Accept".to_string(), accept.to_string()),
                ("User-Agent".to_string(), USER_AGENT.to_string()),
            ],
            ..Default::default()
        },
    )
    .await;
    result.ok.then_some(result.content)
}

/// Collect the DOI and PMCID from the article's `articleids` list.
fn extract_ids(article: &Value) -> (Option<String>, Option<String>) {
    let mut doi = None;
    let mut pmcid = None;
    if let Some(ids) = article.get("articleids").and_then(Value::as_array) {
        for id in ids {
            match str_field(id, "idtype") {
                Some("doi") => doi = str_field(id, "value").map(str::to_string),
                Some("pmc") => pmcid = str_field(id, "value").map(str::to_string),
                _ => {}
            }
        }
    }
    if doi.is_none() {
        doi = str_field(article, "elocationid").map(str::to_string);
    }
    (doi, pmcid)
}

/// MeSH headings are the `MH  - ` lines in a MEDLINE-format record.
fn parse_mesh_terms(medline: &str) -> Vec<String> {
    medline
        .lines()
        .filter_map(|line| line.strip_prefix("MH  - "))
        .map(|term| term.trim().to_string())
        .collect()
}

fn append_header(md: &mut String, article: &Value, pmid: &str) {
    let title = str_field(article, "title").unwrap_or("PubMed Article");
    let _ = write!(md, "# {title}\n\n");
    if let Some(authors) = article.get("authors").and_then(Value::as_array) {
        let names: Vec<&str> = authors
            .iter()
            .filter_map(|a| str_field(a, "name"))
            .collect();
        if !names.is_empty() {
            let _ = writeln!(md, "**Authors:** {}", names.join(", "));
        }
    }
    if let Some(journal) = str_field(article, "fulljournalname") {
        let _ = write!(md, "**Journal:** {journal}");
        if let Some(pubdate) = str_field(article, "pubdate") {
            let _ = write!(md, " ({pubdate})");
        }
        md.push('\n');
    }
    let mut citation: Vec<String> = Vec::new();
    if let Some(vol) = str_field(article, "volume") {
        citation.push(format!("Vol {vol}"));
    }
    if let Some(issue) = str_field(article, "issue") {
        citation.push(format!("Issue {issue}"));
    }
    if let Some(pages) = str_field(article, "pages") {
        citation.push(format!("pp {pages}"));
    }
    if !citation.is_empty() {
        let _ = writeln!(md, "**Citation:** {}", citation.join(", "));
    }
    let _ = writeln!(md, "**PMID:** {pmid}");
    let (doi, pmcid) = extract_ids(article);
    if let Some(doi) = doi {
        let _ = writeln!(md, "**DOI:** {doi}");
    }
    if let Some(pmcid) = pmcid {
        let _ = writeln!(md, "**PMCID:** {pmcid}");
    }
}

fn render(article: &Value, pmid: &str, abstract_text: &str, mesh: &[String]) -> String {
    let mut md = String::new();
    append_header(&mut md, article, pmid);
    md.push_str("\n---\n\n");
    if abstract_text.is_empty() {
        md.push_str("## Abstract\n\nNo abstract available.\n");
    } else {
        let _ = write!(md, "## Abstract\n\n{abstract_text}\n");
    }
    if !mesh.is_empty() {
        md.push_str("\n## MeSH Terms\n\n");
        for term in mesh {
            let _ = writeln!(md, "- {term}");
        }
    }
    md
}

fn fallback(url: &str, pmid: &str, note: &str) -> RenderResult {
    let md = format!(
        "# PubMed Article\n\n**PMID:** {pmid}\n\n---\n\n## Abstract\n\nNo abstract available.\n"
    );
    build_result(&md, url, "pubmed", vec![note.to_string()])
}

#[async_trait]
impl SpecialHandler for PubMedHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let pmid = parse_pmid(url)?;

        let summary_url = format!(
            "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esummary.fcgi?db=pubmed&id={pmid}&retmode=json"
        );
        let Some(summary_body) = fetch(&summary_url, timeout, "application/json").await else {
            return Some(fallback(
                url,
                &pmid,
                "Failed to fetch PubMed summary metadata",
            ));
        };
        let Ok(summary): Result<Value, _> = serde_json::from_str(&summary_body) else {
            return Some(fallback(
                url,
                &pmid,
                "Failed to parse PubMed summary metadata",
            ));
        };
        let Some(article) = summary.get("result").and_then(|r| r.get(&pmid)) else {
            return Some(fallback(
                url,
                &pmid,
                "PubMed record unavailable from E-utilities summary endpoint",
            ));
        };

        let abstract_url = format!(
            "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/efetch.fcgi?db=pubmed&id={pmid}&rettype=abstract&retmode=text"
        );
        let mut notes: Vec<String> = Vec::new();
        let abstract_text = match fetch(&abstract_url, timeout, "text/plain, */*;q=0.8").await {
            Some(text) => {
                notes.push("Fetched abstract via NCBI E-utilities".to_string());
                text.trim().to_string()
            }
            None => String::new(),
        };

        let mesh_url = format!(
            "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/efetch.fcgi?db=pubmed&id={pmid}&rettype=medline&retmode=text"
        );
        let mesh = match fetch(
            &mesh_url,
            timeout.min(Duration::from_secs(5)),
            "text/plain, */*;q=0.8",
        )
        .await
        {
            Some(body) => {
                let terms = parse_mesh_terms(&body);
                if !terms.is_empty() {
                    notes.push("Fetched MeSH terms via NCBI E-utilities".to_string());
                }
                terms
            }
            None => Vec::new(),
        };

        if notes.is_empty() {
            notes.push("Fetched via NCBI E-utilities".to_string());
        }
        let md = render(article, &pmid, &abstract_text, &mesh);
        Some(build_result(&md, url, "pubmed", notes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_pmid_reads_both_url_shapes() {
        assert_eq!(
            parse_pmid("https://pubmed.ncbi.nlm.nih.gov/12345678/"),
            Some("12345678".to_string())
        );
        assert_eq!(
            parse_pmid("https://ncbi.nlm.nih.gov/pubmed/99999"),
            Some("99999".to_string())
        );
        assert_eq!(parse_pmid("https://example.com/12345678"), None);
    }

    #[test]
    fn mesh_terms_read_the_mh_lines() {
        let medline = "PMID- 1\nMH  - Humans\nMH  - Neoplasms/genetics\nAB  - text\n";
        assert_eq!(
            parse_mesh_terms(medline),
            vec!["Humans".to_string(), "Neoplasms/genetics".to_string()]
        );
    }

    #[test]
    fn render_lays_out_metadata_abstract_and_mesh() {
        let article = json!({
            "title": "A Study",
            "authors": [{ "name": "Smith J" }, { "name": "Doe A" }],
            "fulljournalname": "Nature",
            "pubdate": "2020 Jan",
            "volume": "5",
            "pages": "10-20",
            "articleids": [{ "idtype": "doi", "value": "10.1/x" }, { "idtype": "pmc", "value": "PMC1" }]
        });
        let md = render(
            &article,
            "12345678",
            "The abstract.",
            &["Humans".to_string()],
        );
        assert!(md.contains("# A Study"));
        assert!(md.contains("**Authors:** Smith J, Doe A"));
        assert!(md.contains("**Journal:** Nature (2020 Jan)"));
        assert!(md.contains("**Citation:** Vol 5, pp 10-20"));
        assert!(md.contains("**PMID:** 12345678"));
        assert!(md.contains("**DOI:** 10.1/x"));
        assert!(md.contains("**PMCID:** PMC1"));
        assert!(md.contains("## Abstract\n\nThe abstract."));
        assert!(md.contains("## MeSH Terms\n\n- Humans"));
    }
}
