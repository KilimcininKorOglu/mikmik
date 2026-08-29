// Open VSX handler: renders an editor extension from the Open VSX API.

use super::util::{build_result, format_number, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct OpenVsxHandler;

static EXTENSION_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/extension/([^/]+)/([^/]+)(?:/([^/]+))?/?$").expect("static open-vsx regex")
});

/// The `{namespace, extension, version?}` an Open VSX URL names.
struct ExtensionRef {
    namespace: String,
    extension: String,
    version: Option<String>,
}

fn parse_extension(url: &str) -> Option<ExtensionRef> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "open-vsx.org" && host != "www.open-vsx.org" {
        return None;
    }
    let caps = EXTENSION_PATH.captures(parsed.path())?;
    Some(ExtensionRef {
        namespace: super::util::percent_decode(&caps[1]),
        extension: super::util::percent_decode(&caps[2]),
        version: caps.get(3).map(|m| super::util::percent_decode(m.as_str())),
    })
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Repository URL from either the string or `{ url }` object shape, cleaned.
fn repository_url(data: &Value) -> Option<String> {
    let raw = match data.get("repository") {
        Some(Value::String(s)) if !s.is_empty() => s.as_str(),
        Some(obj) => str_field(obj, "url")?,
        None => return None,
    };
    Some(
        raw.trim_start_matches("git+")
            .trim_end_matches(".git")
            .to_string(),
    )
}

fn str_list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn render(data: &Value, ext: &ExtensionRef, readme: Option<&str>) -> String {
    let fallback_name = format!("{}/{}", ext.namespace, ext.extension);
    let display_name = str_field(data, "displayName")
        .or_else(|| str_field(data, "name"))
        .unwrap_or(&fallback_name);
    let mut md = format!("# {display_name}\n\n");
    if let Some(desc) = str_field(data, "description") {
        let _ = write!(md, "{desc}\n\n");
    }

    let _ = writeln!(
        md,
        "**Namespace:** {}",
        str_field(data, "namespace").unwrap_or(&ext.namespace)
    );
    let _ = writeln!(
        md,
        "**Extension:** {}",
        str_field(data, "name").unwrap_or(&ext.extension)
    );
    let version = str_field(data, "version")
        .or(ext.version.as_deref())
        .unwrap_or("unknown");
    let _ = write!(md, "**Version:** {version}");
    if let Some(license) = str_field(data, "license") {
        let _ = write!(md, " | **License:** {license}");
    }
    md.push('\n');

    if let Some(downloads) = data.get("downloadCount").and_then(Value::as_u64) {
        let _ = writeln!(md, "**Downloads:** {}", format_number(downloads));
    }
    if let Some(rating) = data.get("averageRating").and_then(Value::as_f64) {
        let suffix = data
            .get("reviewCount")
            .and_then(Value::as_i64)
            .map(|r| format!(" ({r} reviews)"))
            .unwrap_or_default();
        let _ = writeln!(md, "**Rating:** {rating}{suffix}");
    }
    if let Some(repo) = repository_url(data) {
        let _ = writeln!(md, "**Repository:** {repo}");
    }
    if let Some(homepage) = str_field(data, "homepage") {
        let _ = writeln!(md, "**Homepage:** {homepage}");
    }
    let categories = str_list(data, "categories");
    if !categories.is_empty() {
        let _ = writeln!(md, "**Categories:** {}", categories.join(", "));
    }

    if let Some(readme) = readme {
        let _ = write!(md, "\n---\n\n## README\n\n{readme}\n");
    }
    md
}

async fn fetch(url: &str, timeout: Duration) -> Option<String> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    result.ok.then_some(result.content)
}

#[async_trait]
impl SpecialHandler for OpenVsxHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let ext = parse_extension(url)?;
        let ns = super::util::percent_encode_component(&ext.namespace);
        let name = super::util::percent_encode_component(&ext.extension);
        let api_url = match &ext.version {
            Some(v) => format!(
                "https://open-vsx.org/api/{ns}/{name}/{}",
                super::util::percent_encode_component(v)
            ),
            None => format!("https://open-vsx.org/api/{ns}/{name}"),
        };
        let body = fetch(&api_url, timeout).await?;
        let data: Value = serde_json::from_str(&body).ok()?;

        let readme = match data.get("files").and_then(|f| str_field(f, "readme")) {
            Some(readme_url) => fetch(readme_url, timeout.min(Duration::from_secs(10))).await,
            None => None,
        };
        let md = render(&data, &ext, readme.as_deref());
        Some(build_result(
            &md,
            url,
            "open-vsx",
            vec!["Fetched via Open VSX API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_extension_reads_namespace_and_name() {
        let e = parse_extension("https://open-vsx.org/extension/rust-lang/rust-analyzer").unwrap();
        assert_eq!(
            (e.namespace.as_str(), e.extension.as_str()),
            ("rust-lang", "rust-analyzer")
        );
        assert!(e.version.is_none());
        let v = parse_extension("https://open-vsx.org/extension/a/b/1.2.3").unwrap();
        assert_eq!(v.version.as_deref(), Some("1.2.3"));
        assert!(parse_extension("https://example.com/extension/a/b").is_none());
    }

    #[test]
    fn repository_url_cleans_git_prefix_and_suffix() {
        assert_eq!(
            repository_url(&json!({ "repository": "git+https://github.com/x/y.git" })),
            Some("https://github.com/x/y".to_string())
        );
        assert_eq!(
            repository_url(&json!({ "repository": { "url": "https://gitlab.com/a/b" } })),
            Some("https://gitlab.com/a/b".to_string())
        );
    }

    #[test]
    fn render_lays_out_extension_metadata() {
        let data = json!({
            "displayName": "rust-analyzer",
            "namespace": "rust-lang",
            "name": "rust-analyzer",
            "version": "0.4.0",
            "description": "Rust language support",
            "license": "MIT",
            "downloadCount": 1234567,
            "averageRating": 4.8,
            "reviewCount": 42,
            "categories": ["Programming Languages"]
        });
        let ext =
            parse_extension("https://open-vsx.org/extension/rust-lang/rust-analyzer").unwrap();
        let md = render(&data, &ext, Some("Readme body."));
        assert!(md.contains("# rust-analyzer"));
        assert!(md.contains("Rust language support"));
        assert!(md.contains("**Version:** 0.4.0 | **License:** MIT"));
        assert!(md.contains("**Downloads:** 1,234,567"));
        assert!(md.contains("**Rating:** 4.8 (42 reviews)"));
        assert!(md.contains("## README\n\nReadme body."));
    }
}
