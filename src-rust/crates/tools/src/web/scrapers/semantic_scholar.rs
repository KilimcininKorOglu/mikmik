// Semantic Scholar handler: renders a paper via the Graph API.

use super::util::{build_result, format_number, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct SemanticScholarHandler;

const FIELDS: &str = "title,abstract,authors,year,citationCount,referenceCount,fieldsOfStudy,publicationTypes,journal,externalIds,tldr,openAccessPdf";

static PAPER_ID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:api\.)?semanticscholar\.org/(?:.*/)?paper/(?:[^/]+/)?([a-f0-9]{40})")
        .expect("static s2 regex")
});

fn paper_id(url: &str) -> Option<String> {
    if !url.contains("semanticscholar.org") {
        return None;
    }
    PAPER_ID.captures(url).map(|m| m[1].to_lowercase())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn append_authors(md: &mut String, paper: &Value) {
    let Some(authors) = paper.get("authors").and_then(Value::as_array) else {
        return;
    };
    let names: Vec<&str> = authors
        .iter()
        .filter_map(|a| str_field(a, "name"))
        .collect();
    if !names.is_empty() {
        let _ = write!(md, "**Authors:** {}\n\n", names.join(", "));
    }
}

fn append_metadata(md: &mut String, paper: &Value) {
    let mut parts: Vec<String> = Vec::new();
    if let Some(year) = paper.get("year").and_then(Value::as_i64) {
        parts.push(format!("Year: {year}"));
    }
    if let Some(venue) = paper.get("journal").and_then(|j| str_field(j, "name")) {
        parts.push(format!("Venue: {venue}"));
    }
    if let Some(citations) = paper.get("citationCount").and_then(Value::as_u64) {
        parts.push(format!("Citations: {}", format_number(citations)));
    }
    if let Some(refs) = paper.get("referenceCount").and_then(Value::as_u64) {
        parts.push(format!("References: {}", format_number(refs)));
    }
    if !parts.is_empty() {
        let _ = write!(md, "{}\n\n", parts.join(" • "));
    }
}

fn append_links(md: &mut String, paper: &Value) {
    let mut links: Vec<String> = Vec::new();
    if let Some(pdf) = paper.get("openAccessPdf").and_then(|p| str_field(p, "url")) {
        links.push(format!("[PDF]({pdf})"));
    }
    let ext = paper.get("externalIds").cloned().unwrap_or(Value::Null);
    if let Some(arxiv) = str_field(&ext, "ArXiv") {
        links.push(format!("[arXiv](https://arxiv.org/abs/{arxiv})"));
    }
    if let Some(doi) = str_field(&ext, "DOI") {
        links.push(format!("[DOI](https://doi.org/{doi})"));
    }
    if let Some(pubmed) = str_field(&ext, "PubMed") {
        links.push(format!(
            "[PubMed](https://pubmed.ncbi.nlm.nih.gov/{pubmed}/)"
        ));
    }
    if let Some(id) = str_field(paper, "paperId") {
        links.push(format!(
            "[Semantic Scholar](https://www.semanticscholar.org/paper/{id})"
        ));
    }
    if !links.is_empty() {
        let _ = write!(md, "## Links\n\n{}\n", links.join(" • "));
    }
}

fn render(paper: &Value) -> String {
    let mut md = format!("# {}\n\n", str_field(paper, "title").unwrap_or("Untitled"));
    append_authors(&mut md, paper);
    append_metadata(&mut md, paper);
    if let Some(fields) = paper.get("fieldsOfStudy").and_then(Value::as_array) {
        let list: Vec<&str> = fields.iter().filter_map(Value::as_str).collect();
        if !list.is_empty() {
            let _ = write!(md, "**Fields:** {}\n\n", list.join(", "));
        }
    }
    if let Some(tldr) = paper.get("tldr").and_then(|t| str_field(t, "text")) {
        let _ = write!(md, "## TL;DR\n\n{tldr}\n\n");
    }
    if let Some(abstract_) = str_field(paper, "abstract") {
        let _ = write!(md, "## Abstract\n\n{abstract_}\n\n");
    }
    append_links(&mut md, paper);
    md
}

#[async_trait]
impl SpecialHandler for SemanticScholarHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let id = paper_id(url)?;
        let api_url =
            format!("https://api.semanticscholar.org/graph/v1/paper/{id}?fields={FIELDS}");
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
        let paper: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&paper);
        Some(build_result(
            &md,
            url,
            "semantic-scholar",
            vec!["Fetched via Semantic Scholar API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paper_id_reads_the_forty_hex_id_in_several_forms() {
        let id = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            paper_id(&format!(
                "https://www.semanticscholar.org/paper/Some-Title/{id}"
            ))
            .as_deref(),
            Some(id)
        );
        assert_eq!(
            paper_id(&format!("https://www.semanticscholar.org/paper/{id}")).as_deref(),
            Some(id)
        );
        assert!(paper_id("https://example.com/paper/x").is_none());
    }

    #[test]
    fn render_lays_out_paper_sections() {
        let paper = json!({
            "paperId": "abc",
            "title": "Attention Is All You Need",
            "authors": [{ "name": "Vaswani" }, { "name": "Shazeer" }],
            "year": 2017,
            "citationCount": 100000,
            "journal": { "name": "NeurIPS" },
            "fieldsOfStudy": ["Computer Science"],
            "tldr": { "text": "Transformers." },
            "abstract": "The dominant sequence models...",
            "externalIds": { "DOI": "10.5555/x", "ArXiv": "1706.03762" },
            "openAccessPdf": { "url": "https://arxiv.org/pdf/1706.03762" }
        });
        let md = render(&paper);
        assert!(md.contains("# Attention Is All You Need"));
        assert!(md.contains("**Authors:** Vaswani, Shazeer"));
        assert!(md.contains("Year: 2017 • Venue: NeurIPS • Citations: 100,000"));
        assert!(md.contains("**Fields:** Computer Science"));
        assert!(md.contains("## TL;DR\n\nTransformers."));
        assert!(md.contains("## Abstract\n\nThe dominant sequence models..."));
        assert!(md.contains("[arXiv](https://arxiv.org/abs/1706.03762)"));
        assert!(md.contains("[DOI](https://doi.org/10.5555/x)"));
    }
}
