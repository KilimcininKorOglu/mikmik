// pkg.go.dev handler: renders a Go package from the module proxy (version/date)
// plus the pkg.go.dev HTML page (metadata, synopsis, docs, index, imports).

use super::dom;
use super::util::{build_result, html_to_markdown, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use scraper::{Html, Selector};
use serde_json::Value;
use std::time::Duration;

pub struct GoPkgHandler;

const INDEX_LIMIT: usize = 50;
const IMPORTS_LIMIT: usize = 20;
const DOC_PARAGRAPHS: usize = 3;

static BREADCRUMB_LINK: Lazy<Selector> = Lazy::new(|| dom::selector(".go-Breadcrumb a[href^='/']"));
static CHIP: Lazy<Selector> = Lazy::new(|| dom::selector(".go-Chip"));
static LICENSE: Lazy<Selector> =
    Lazy::new(|| dom::selector("a[data-test-id='UnitHeader-license']"));
static IMPORT_PATH: Lazy<Selector> =
    Lazy::new(|| dom::selector("input[data-test-id='UnitHeader-importPath']"));
static SYNOPSIS: Lazy<Selector> = Lazy::new(|| dom::selector(".go-Main-headerContent p"));
static DOC_SECTION: Lazy<Selector> = Lazy::new(|| dom::selector("#section-documentation"));
static DOC_MESSAGE: Lazy<Selector> = Lazy::new(|| dom::selector(".go-Message"));
static DOC_CONTENT: Lazy<Selector> = Lazy::new(|| dom::selector(".Documentation-content"));
static PARAGRAPH: Lazy<Selector> = Lazy::new(|| dom::selector("p"));
static INDEX_SECTION: Lazy<Selector> = Lazy::new(|| dom::selector("#section-index"));
static INDEX_LIST: Lazy<Selector> = Lazy::new(|| dom::selector(".Documentation-indexList"));
static LIST_LINK: Lazy<Selector> = Lazy::new(|| dom::selector("li a"));
static IMPORTS_SECTION: Lazy<Selector> = Lazy::new(|| dom::selector("#section-imports"));
static ANCHOR: Lazy<Selector> = Lazy::new(|| dom::selector("a"));

struct Target {
    module_path: String,
    version: String,
}

fn parse_target(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "pkg.go.dev" {
        return None;
    }
    let path = parsed.path().trim_start_matches('/').to_string();
    if path.is_empty() {
        return None;
    }
    let target = match path.find('@') {
        Some(at) => {
            let before = path[..at].to_string();
            let after = &path[at + 1..];
            let version = after.split('/').next().unwrap_or(after).to_string();
            Target {
                module_path: before,
                version,
            }
        }
        None => Target {
            module_path: path,
            version: "latest".to_string(),
        },
    };
    Some(target)
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}

async fn fetch_module_info(target: &Target, timeout: Duration) -> Option<Value> {
    let encoded = super::util::percent_encode_component(&target.module_path);
    let url = if target.version == "latest" {
        format!("https://proxy.golang.org/{encoded}/@latest")
    } else {
        let v = super::util::percent_encode_component(&target.version);
        format!("https://proxy.golang.org/{encoded}/@v/{v}.info")
    };
    fetch_json(&url, timeout).await
}

fn breadcrumb_module(doc: &Html, fallback: &str) -> String {
    doc.select(&BREADCRUMB_LINK)
        .next()
        .and_then(|link| dom::attr(link, "href"))
        .map(|href| {
            href.trim_start_matches('/')
                .split('@')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn chip_version(doc: &Html) -> Option<String> {
    let text = doc.select(&CHIP).next().map(dom::text)?;
    text.starts_with('v').then_some(text)
}

fn append_docs(sections: &mut Vec<String>, doc: &Html) {
    let Some(doc_section) = doc.select(&DOC_SECTION).next() else {
        return;
    };
    sections.push("## Documentation".to_string());
    sections.push(String::new());
    if let Some(overview) = doc_section.select(&DOC_MESSAGE).next() {
        sections.push(html_to_markdown(&overview.inner_html()));
        sections.push(String::new());
    }
    if let Some(content) = doc_section.select(&DOC_CONTENT).next() {
        let parts: Vec<String> = content
            .select(&PARAGRAPH)
            .take(DOC_PARAGRAPHS)
            .map(|p| html_to_markdown(&p.inner_html()))
            .filter(|t| !t.is_empty())
            .collect();
        if !parts.is_empty() {
            sections.push(parts.join("\n\n"));
            sections.push(String::new());
        }
    }
}

fn append_list_section(
    sections: &mut Vec<String>,
    notes: &mut Vec<String>,
    heading: &str,
    items: Vec<String>,
    limit: usize,
    noun: &str,
) {
    if items.is_empty() {
        return;
    }
    sections.push(format!("## {heading}"));
    sections.push(String::new());
    sections.push(
        items
            .iter()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
    );
    if items.len() > limit {
        notes.push(format!("showing {limit} of {} {noun}", items.len()));
        sections.push(format!("\n[…{} {noun} elided…]", items.len() - limit));
    }
    sections.push(String::new());
}

fn index_items(doc: &Html) -> Vec<String> {
    let Some(section) = doc.select(&INDEX_SECTION).next() else {
        return Vec::new();
    };
    let Some(list) = section.select(&INDEX_LIST).next() else {
        return Vec::new();
    };
    list.select(&LIST_LINK)
        .map(dom::text)
        .filter(|t| !t.is_empty())
        .map(|name| format!("- {name}"))
        .collect()
}

fn import_items(doc: &Html) -> Vec<String> {
    let Some(section) = doc.select(&IMPORTS_SECTION).next() else {
        return Vec::new();
    };
    let Some(list) = section.select(&DOC_MESSAGE).next() else {
        return Vec::new();
    };
    list.select(&ANCHOR)
        .map(dom::text)
        .filter(|t| !t.is_empty())
        .map(|imp| format!("- {imp}"))
        .collect()
}

fn render(
    doc: &Html,
    target: &Target,
    module_info: &Option<Value>,
    notes: &mut Vec<String>,
) -> String {
    let actual_module = breadcrumb_module(doc, &target.module_path);
    let version = module_info
        .as_ref()
        .and_then(|m| m.get("Version").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| chip_version(doc))
        .unwrap_or_else(|| target.version.clone());
    let license = doc
        .select(&LICENSE)
        .next()
        .map(dom::text)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());
    let import_path = doc
        .select(&IMPORT_PATH)
        .next()
        .and_then(|input| dom::attr(input, "value"))
        .unwrap_or(&actual_module)
        .to_string();

    let mut sections = vec![
        format!("# {import_path}"),
        String::new(),
        format!("**Module:** {actual_module}"),
        format!("**Version:** {version}"),
        format!("**License:** {license}"),
        String::new(),
    ];
    if let Some(synopsis) = doc
        .select(&SYNOPSIS)
        .next()
        .map(dom::text)
        .filter(|t| !t.is_empty())
    {
        sections.push("## Synopsis".to_string());
        sections.push(String::new());
        sections.push(synopsis);
        sections.push(String::new());
    }
    append_docs(&mut sections, doc);
    append_list_section(
        &mut sections,
        notes,
        "Index",
        index_items(doc),
        INDEX_LIMIT,
        "exports",
    );
    append_list_section(
        &mut sections,
        notes,
        "Imports",
        import_items(doc),
        IMPORTS_LIMIT,
        "imports",
    );
    if let Some(time) = module_info
        .as_ref()
        .and_then(|m| m.get("Time").and_then(Value::as_str))
    {
        notes.push(format!("published {time}"));
    }
    sections.join("\n")
}

#[async_trait]
impl SpecialHandler for GoPkgHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let target = parse_target(url)?;
        let module_info = fetch_module_info(&target, timeout).await;
        let page = load_page(
            url,
            LoadOptions {
                timeout,
                ..Default::default()
            },
        )
        .await;
        if !page.ok {
            return Some(build_result(
                "Failed to fetch pkg.go.dev page",
                url,
                "go-pkg",
                vec!["error".to_string()],
            ));
        }
        let doc = dom::parse(&page.content);
        let mut notes = Vec::new();
        let md = render(&doc, &target, &module_info, &mut notes);
        if notes.is_empty() {
            notes.push("Fetched via pkg.go.dev".to_string());
        }
        Some(build_result(&md, url, "go-pkg", notes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_reads_version() {
        let plain = parse_target("https://pkg.go.dev/rsc.io/quote").unwrap();
        assert_eq!(plain.module_path, "rsc.io/quote");
        assert_eq!(plain.version, "latest");
        let versioned = parse_target("https://pkg.go.dev/rsc.io/quote/v3@v3.1.0/sub").unwrap();
        assert_eq!(versioned.module_path, "rsc.io/quote/v3");
        assert_eq!(versioned.version, "v3.1.0");
        assert!(parse_target("https://example.com/x").is_none());
    }

    #[test]
    fn render_lays_out_package() {
        let html = r#"<html><body>
            <nav class="go-Breadcrumb"><a href="/rsc.io/quote">rsc.io/quote</a></nav>
            <span class="go-Chip">v1.5.2</span>
            <a data-test-id="UnitHeader-license">MIT</a>
            <input data-test-id="UnitHeader-importPath" value="rsc.io/quote">
            <div class="go-Main-headerContent"><p>Package quote collects sayings.</p></div>
            <section id="section-index"><ul class="Documentation-indexList"><li><a>func Hello</a></li></ul></section>
        </body></html>"#;
        let doc = dom::parse(html);
        let mut notes = Vec::new();
        let target = Target {
            module_path: "rsc.io/quote".to_string(),
            version: "latest".to_string(),
        };
        let md = render(
            &doc,
            &target,
            &Some(json!({"Version":"v1.5.2","Time":"2020-01-01T00:00:00Z"})),
            &mut notes,
        );
        assert!(md.contains("# rsc.io/quote"));
        assert!(md.contains("**Version:** v1.5.2"));
        assert!(md.contains("**License:** MIT"));
        assert!(md.contains("## Synopsis"));
        assert!(md.contains("Package quote collects sayings."));
        assert!(md.contains("## Index"));
        assert!(md.contains("- func Hello"));
        assert!(notes.iter().any(|n| n.contains("published 2020-01-01")));
    }
}
