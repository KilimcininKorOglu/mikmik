// Ollama handler: renders a model from the ollama.com tags API plus regex
// scraping of the model page.

use super::util::{
    build_result, decode_html_entities, format_bytes, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::time::Duration;

pub struct OllamaHandler;

const MAX_TAGS: usize = 40;
const RESERVED_ROOTS: [&str; 13] = [
    "models", "blog", "docs", "download", "cloud", "signin", "signout", "search", "api", "terms",
    "privacy", "license", "settings",
];

static META_DESC: Lazy<[Regex; 3]> = Lazy::new(|| {
    [
        Regex::new(r#"(?i)<meta[^>]+name=["']description["'][^>]*content=["']([^"']+)["']"#)
            .expect("static ollama desc regex"),
        Regex::new(r#"(?i)<meta[^>]+property=["']og:description["'][^>]*content=["']([^"']+)["']"#)
            .expect("static ollama og regex"),
        Regex::new(
            r#"(?i)<meta[^>]+property=["']twitter:description["'][^>]*content=["']([^"']+)["']"#,
        )
        .expect("static ollama twitter regex"),
    ]
});
static SIZE_SPAN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)x-test-size[^>]*>([^<]+)</span>").expect("static ollama size"));
static LIBRARY_HREF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)href=["']/library/([^"']+)["']"#).expect("static ollama href"));

/// The references and page URL parsed from an ollama.com model URL.
struct OllamaRef {
    model_ref: String,
    base_ref: String,
    page_url: String,
}

fn base_of(name: &str) -> &str {
    name.split(':').next().unwrap_or(name)
}

fn parse_ollama_url(url: &str) -> Option<OllamaRef> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "ollama.com" && host != "www.ollama.com" {
        return None;
    }
    let origin = format!("https://{host}");
    let parts: Vec<String> = parsed
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(super::util::percent_decode)
        .collect();
    if parts.is_empty() {
        return None;
    }
    if parts[0] == "library" && parts.len() >= 2 {
        let model_ref = parts[1].clone();
        let base_ref = base_of(&model_ref).to_string();
        let page_url = format!("{origin}/library/{}", encode(&base_ref));
        return Some(OllamaRef {
            model_ref,
            base_ref,
            page_url,
        });
    }
    if parts.len() >= 2 && !RESERVED_ROOTS.contains(&parts[0].as_str()) {
        let namespace = &parts[0];
        let model = &parts[1];
        let model_base = base_of(model);
        let page_url = format!("{origin}/{}/{}", encode(namespace), encode(model_base));
        return Some(OllamaRef {
            model_ref: format!("{namespace}/{model}"),
            base_ref: format!("{namespace}/{model_base}"),
            page_url,
        });
    }
    None
}

fn encode(s: &str) -> String {
    super::util::percent_encode_component(s)
}

fn extract_meta_description(html: &str) -> Option<String> {
    META_DESC
        .iter()
        .find_map(|re| re.captures(html))
        .map(|caps| decode_html_entities(caps[1].trim()))
}

fn extract_parameter_sizes(html: &str) -> Vec<String> {
    SIZE_SPAN
        .captures_iter(html)
        .filter_map(|c| {
            let raw = c[1].trim();
            (!raw.is_empty()).then(|| raw.to_uppercase())
        })
        .collect()
}

/// `/library/<tag>` hrefs on the page that name this model or one of its tags.
fn extract_tags_from_html(html: &str, base_ref: &str) -> Vec<String> {
    let prefix = format!("{base_ref}:");
    LIBRARY_HREF
        .captures_iter(html)
        .filter_map(|c| {
            let decoded = decode_html_entities(c[1].trim());
            (decoded == base_ref || decoded.starts_with(&prefix)).then_some(decoded)
        })
        .collect()
}

fn model_name(model: &Value) -> String {
    model
        .get("model")
        .or_else(|| model.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `:latest` sorts first, then lexical order.
fn sort_tags(tags: BTreeSet<String>) -> Vec<String> {
    let mut list: Vec<String> = tags.into_iter().collect();
    list.sort_by(|a, b| {
        let a_latest = a.ends_with(":latest");
        let b_latest = b.ends_with(":latest");
        match (a_latest, b_latest) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });
    list
}

fn format_tag_list(tags: &[String]) -> String {
    let shown: Vec<String> = tags
        .iter()
        .take(MAX_TAGS)
        .map(|t| format!("`{t}`"))
        .collect();
    let joined = shown.join(", ");
    if tags.len() > MAX_TAGS {
        format!("{joined} […{} tags elided…]", tags.len() - MAX_TAGS)
    } else {
        joined
    }
}

fn collect_parameter_sizes(models: &[&Value], html_sizes: &[String]) -> Vec<String> {
    let mut sizes: BTreeSet<String> = BTreeSet::new();
    for model in models {
        if let Some(param) = model
            .get("details")
            .and_then(|d| d.get("parameter_size"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            sizes.insert(param.to_uppercase());
        }
    }
    for size in html_sizes {
        sizes.insert(size.clone());
    }
    sizes.into_iter().collect()
}

/// The single size, or a `min - max` range across the matching models.
fn size_line(selected: Option<&Value>, matching: &[&Value]) -> Option<String> {
    if let Some(size) = selected.and_then(|m| m.get("size")).and_then(Value::as_u64) {
        return Some(format_bytes(size));
    }
    let sizes: Vec<u64> = matching
        .iter()
        .filter_map(|m| m.get("size").and_then(Value::as_u64))
        .collect();
    let min = *sizes.iter().min()?;
    let max = *sizes.iter().max()?;
    Some(if min == max {
        format_bytes(min)
    } else {
        format!("{} - {}", format_bytes(min), format_bytes(max))
    })
}

/// Assembled inputs the markdown renderer needs.
struct ModelView {
    base_ref: String,
    tag_ref: Option<String>,
    description: Option<String>,
    parameter_sizes: Vec<String>,
    size_line: Option<String>,
    tags: Vec<String>,
}

fn render(view: &ModelView) -> String {
    let mut md = format!("# {}\n\n", view.base_ref);
    if let Some(desc) = &view.description {
        let _ = write!(md, "{desc}\n\n");
    }
    let _ = writeln!(md, "**Model:** {}", view.base_ref);
    if let Some(tag) = &view.tag_ref {
        let _ = writeln!(md, "**Tag:** {tag}");
    }
    if !view.parameter_sizes.is_empty() {
        let _ = writeln!(md, "**Parameters:** {}", view.parameter_sizes.join(", "));
    }
    if let Some(size) = &view.size_line {
        let label = if size.contains(" - ") {
            "Size Range"
        } else {
            "Size"
        };
        let _ = writeln!(md, "**{label}:** {size}");
    }
    if !view.tags.is_empty() {
        let _ = writeln!(md, "**Available Tags:** {}", format_tag_list(&view.tags));
    }
    md
}

/// Build the view from the tags API models and the scraped page HTML.
fn build_view(reference: &OllamaRef, models: &[Value], html: &str) -> ModelView {
    let base_lower = reference.base_ref.to_lowercase();
    let base_prefix = format!("{base_lower}:");
    let matching: Vec<&Value> = models
        .iter()
        .filter(|m| {
            let name = model_name(m).to_lowercase();
            name == base_lower || name.starts_with(&base_prefix)
        })
        .collect();

    let tag_ref = reference
        .model_ref
        .contains(':')
        .then(|| reference.model_ref.clone());
    let selected = tag_ref
        .as_ref()
        .and_then(|t| matching.iter().find(|m| model_name(m) == *t).copied());

    let api_tags: BTreeSet<String> = matching
        .iter()
        .map(|m| model_name(m))
        .filter(|t| !t.is_empty())
        .collect();
    let tags = if api_tags.is_empty() {
        sort_tags(
            extract_tags_from_html(html, &reference.base_ref)
                .into_iter()
                .collect(),
        )
    } else {
        sort_tags(api_tags)
    };

    let size_source: Vec<&Value> = selected
        .map(|s| vec![s])
        .unwrap_or_else(|| matching.clone());
    ModelView {
        base_ref: reference.base_ref.clone(),
        tag_ref,
        description: extract_meta_description(html),
        parameter_sizes: collect_parameter_sizes(&size_source, &extract_parameter_sizes(html)),
        size_line: size_line(selected, &matching),
        tags,
    }
}

#[async_trait]
impl SpecialHandler for OllamaHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let reference = parse_ollama_url(url)?;
        let tags_opts = LoadOptions {
            timeout,
            headers: vec![("Accept".to_string(), "application/json".to_string())],
            ..Default::default()
        };
        let page_opts = LoadOptions {
            timeout,
            ..Default::default()
        };
        let (tags_result, page_result) = tokio::join!(
            load_page("https://ollama.com/api/tags", tags_opts),
            load_page(&reference.page_url, page_opts)
        );

        let models: Vec<Value> = if tags_result.ok {
            serde_json::from_str::<Value>(&tags_result.content)
                .ok()
                .and_then(|v| v.get("models").and_then(Value::as_array).cloned())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let html = if page_result.ok {
            page_result.content.as_str()
        } else {
            ""
        };

        let view = build_view(&reference, &models, html);
        let md = render(&view);
        Some(build_result(
            &md,
            url,
            "ollama",
            vec!["Fetched via Ollama API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_url_handles_library_and_namespace_forms() {
        let lib = parse_ollama_url("https://ollama.com/library/llama3:8b").unwrap();
        assert_eq!(lib.base_ref, "llama3");
        assert_eq!(lib.model_ref, "llama3:8b");
        assert_eq!(lib.page_url, "https://ollama.com/library/llama3");

        let ns = parse_ollama_url("https://ollama.com/library/llama3").unwrap();
        assert_eq!(ns.base_ref, "llama3");

        assert!(parse_ollama_url("https://ollama.com/blog/post").is_none());
        assert!(parse_ollama_url("https://example.com/library/x").is_none());
    }

    #[test]
    fn html_helpers_scrape_description_sizes_and_tags() {
        let html = r#"<meta name="description" content="A great model &amp; friend">
            <span class="x-test-size">7b</span><span class="x-test-size">13b</span>
            <a href="/library/llama3:8b">8b</a><a href="/library/other">x</a>"#;
        assert_eq!(
            extract_meta_description(html),
            Some("A great model & friend".to_string())
        );
        assert_eq!(extract_parameter_sizes(html), vec!["7B", "13B"]);
        assert_eq!(
            extract_tags_from_html(html, "llama3"),
            vec!["llama3:8b".to_string()]
        );
    }

    #[test]
    fn build_view_prefers_api_tags_and_size_range() {
        let reference = parse_ollama_url("https://ollama.com/library/llama3").unwrap();
        let models = vec![
            json!({ "model": "llama3:8b", "size": 4_000_000_000u64, "details": { "parameter_size": "8B" } }),
            json!({ "model": "llama3:70b", "size": 40_000_000_000u64, "details": { "parameter_size": "70B" } }),
            json!({ "model": "mistral:7b", "size": 4_000_000_000u64 }),
        ];
        let view = build_view(&reference, &models, "");
        assert_eq!(
            view.tags,
            vec!["llama3:70b".to_string(), "llama3:8b".to_string()]
        );
        assert_eq!(
            view.parameter_sizes,
            vec!["70B".to_string(), "8B".to_string()]
        );
        let md = render(&view);
        assert!(md.contains("# llama3"));
        assert!(md.contains("**Size Range:** 3.7GB - 37.3GB"));
        assert!(md.contains("**Available Tags:** `llama3:70b`, `llama3:8b`"));
    }
}
