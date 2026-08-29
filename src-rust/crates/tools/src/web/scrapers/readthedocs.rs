// Read the Docs handler: extracts the main content of a Sphinx/RTD page,
// preferring the raw source behind an "Edit on GitHub/GitLab" link and falling
// back to converting the cleaned main-content HTML to markdown.

use super::dom;
use super::util::{build_result, html_to_markdown, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use scraper::{ElementRef, Html, Selector};
use std::time::Duration;

pub struct ReadTheDocsHandler;

const MAX_SOURCE_BYTES: usize = 1_000_000;

static MAIN_SELECTORS: Lazy<Vec<Selector>> = Lazy::new(|| {
    [
        ".document",
        "[role=\"main\"]",
        "main",
        ".rst-content",
        ".body",
    ]
    .iter()
    .map(|s| dom::selector(s))
    .collect()
});
static BODY: Lazy<Selector> = Lazy::new(|| dom::selector("body"));
static NOISE: Lazy<Selector> = Lazy::new(|| {
    dom::selector(
        ".headerlink, .viewcode-link, nav, .sidebar, footer, .related, .sphinxsidebar, .toctree-wrapper",
    )
});
static EDIT_LINK: Lazy<Selector> =
    Lazy::new(|| dom::selector("a[href*=\"github.com\"], a[href*=\"gitlab.com\"]"));

fn is_readthedocs(host: &str) -> bool {
    host.ends_with(".readthedocs.io") || host == "readthedocs.org" || host == "www.readthedocs.org"
}

fn find_main<'a>(doc: &'a Html, notes: &mut Vec<String>) -> Option<ElementRef<'a>> {
    for sel in MAIN_SELECTORS.iter() {
        if let Some(el) = doc.select(sel).next() {
            return Some(el);
        }
    }
    let body = doc.select(&BODY).next();
    if body.is_some() {
        notes.push("Using full body content (no main content div found)".to_string());
    }
    body
}

/// Serialize the main content's inner HTML with navigation/sidebar/footer
/// fragments removed. `scraper` cannot mutate the tree, so each unwanted
/// element's serialized HTML is stripped from the string.
fn cleaned_inner_html(main: ElementRef<'_>) -> String {
    let mut html = main.inner_html();
    for noise in main.select(&NOISE) {
        let fragment = noise.html();
        if !fragment.is_empty() {
            html = html.replace(&fragment, "");
        }
    }
    html
}

/// The raw-source URL behind an "Edit on GitHub/GitLab" link, if any.
fn source_url(doc: &Html) -> Option<String> {
    for link in doc.select(&EDIT_LINK) {
        let href = dom::attr(link, "href")?;
        let text = dom::text(link).to_lowercase();
        if text.contains("edit") || text.contains("source") {
            return Some(href.replace("/blob/", "/raw/").replace("/edit/", "/raw/"));
        }
    }
    None
}

async fn fetch(url: &str, timeout: Duration) -> super::util::LoadPageResult {
    load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await
}

async fn fetch_source(url: &str, timeout: Duration) -> Option<String> {
    let result = fetch(url, timeout.min(Duration::from_secs(10))).await;
    let len = result.content.len();
    (result.ok && len > 0 && len < MAX_SOURCE_BYTES).then_some(result.content)
}

fn extract_content(doc: &Html, notes: &mut Vec<String>) -> String {
    let main = find_main(doc, notes);
    let content = main.map(cleaned_inner_html).map(|h| html_to_markdown(&h));
    match content.filter(|c| !c.is_empty()) {
        Some(c) => c,
        None => {
            notes.push("Failed to extract content".to_string());
            "No content extracted from Read the Docs page".to_string()
        }
    }
}

#[async_trait]
impl SpecialHandler for ReadTheDocsHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        if !is_readthedocs(parsed.host_str()?) {
            return None;
        }
        let page = fetch(url, timeout).await;
        if !page.ok {
            let status = page
                .status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            return Some(build_result(
                &format!("Failed to fetch Read the Docs page (status: {status})"),
                url,
                "readthedocs",
                Vec::new(),
            ));
        }
        // `scraper::Html` is not `Send`, so everything the document yields is
        // pulled into owned values before the source fetch is awaited.
        let (src, fallback, fallback_notes) = {
            let doc = dom::parse(&page.content);
            let src = source_url(&doc);
            let mut notes = Vec::new();
            let fallback = extract_content(&doc, &mut notes);
            (src, fallback, notes)
        };

        let mut notes = Vec::new();
        let content = match src {
            Some(src) => match fetch_source(&src, timeout).await {
                Some(raw) => {
                    notes.push(format!("Fetched raw source from {src}"));
                    raw
                }
                None => {
                    notes.extend(fallback_notes);
                    fallback
                }
            },
            None => {
                notes.extend(fallback_notes);
                fallback
            }
        };
        Some(build_result(&content, url, "readthedocs", notes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_hosts() {
        assert!(is_readthedocs("myproject.readthedocs.io"));
        assert!(is_readthedocs("readthedocs.org"));
        assert!(!is_readthedocs("example.com"));
    }

    #[test]
    fn source_url_rewrites_blob_to_raw() {
        let html = r#"<html><body>
            <a href="https://github.com/o/r/blob/main/docs/index.rst">Edit on GitHub</a>
        </body></html>"#;
        let doc = dom::parse(html);
        assert_eq!(
            source_url(&doc).as_deref(),
            Some("https://github.com/o/r/raw/main/docs/index.rst")
        );
    }

    #[test]
    fn extract_drops_noise_elements() {
        let html = r#"<html><body>
            <div class="document">
                <nav>navigation menu</nav>
                <p>Real documentation content here.</p>
            </div>
        </body></html>"#;
        let doc = dom::parse(html);
        let mut notes = Vec::new();
        let content = extract_content(&doc, &mut notes);
        assert!(content.contains("Real documentation content here."));
        assert!(!content.contains("navigation menu"));
    }
}
