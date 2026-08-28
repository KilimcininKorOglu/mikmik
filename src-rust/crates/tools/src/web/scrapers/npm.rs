// npm handler: renders an npmjs.com package page from the registry API.

use super::util::{
    build_result, format_number, load_page, percent_decode, percent_encode_component, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct NpmHandler;

/// The package name from an `npmjs.com/package/...` URL, honouring `@scope/name`.
fn package_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "www.npmjs.com" && host != "npmjs.com" {
        return None;
    }
    let path = parsed.path();
    let rest = path.strip_prefix("/package/")?;
    if rest.is_empty() {
        return None;
    }
    let decoded = percent_decode(rest);
    if decoded.starts_with('@') {
        // Scoped: keep `@scope/name`, drop anything after.
        let mut parts = decoded.splitn(3, '/');
        let scope = parts.next()?;
        let name = parts.next()?;
        Some(format!("{scope}/{name}"))
    } else {
        Some(decoded.split('/').next()?.to_string())
    }
}

/// License, which the registry gives as a string or `{ "type": ... }`.
fn license_str(pkg: &Value) -> Option<String> {
    match pkg.get("license") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Object(o)) => o.get("type").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Repository URL, given as a string or `{ "url": ... }`, cleaned of git noise.
fn repository_url(pkg: &Value) -> Option<String> {
    let raw = match pkg.get("repository") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Object(o)) => o.get("url").and_then(Value::as_str),
        _ => None,
    }?;
    Some(
        raw.trim_start_matches("git+")
            .trim_end_matches(".git")
            .to_string(),
    )
}

fn render_markdown(pkg: &Value, weekly_downloads: Option<u64>) -> String {
    let name = pkg
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("(package)");
    let mut md = format!("# {name}\n\n");
    if let Some(desc) = pkg.get("description").and_then(Value::as_str) {
        let _ = writeln!(md, "{desc}\n");
    }
    let version = pkg
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let _ = write!(md, "**Latest:** {version}");
    if let Some(license) = license_str(pkg) {
        let _ = write!(md, " · **License:** {license}");
    }
    md.push('\n');
    if let Some(downloads) = weekly_downloads {
        let _ = writeln!(md, "**Weekly Downloads:** {}", format_number(downloads));
    }
    md.push('\n');

    if let Some(homepage) = pkg.get("homepage").and_then(Value::as_str) {
        let _ = writeln!(md, "**Homepage:** {homepage}");
    }
    if let Some(repo) = repository_url(pkg) {
        let _ = writeln!(md, "**Repository:** {repo}");
    }
    append_lists(&mut md, pkg);
    if let Some(readme) = pkg.get("readme").and_then(Value::as_str) {
        let _ = write!(md, "\n---\n\n## README\n\n{readme}\n");
    }
    md
}

fn append_lists(md: &mut String, pkg: &Value) {
    if let Some(keywords) = pkg.get("keywords").and_then(Value::as_array) {
        let words: Vec<&str> = keywords.iter().filter_map(Value::as_str).collect();
        if !words.is_empty() {
            let _ = writeln!(md, "**Keywords:** {}", words.join(", "));
        }
    }
    if let Some(deps) = pkg.get("dependencies").and_then(Value::as_object) {
        if !deps.is_empty() {
            md.push_str("\n## Dependencies\n\n");
            for (dep, version) in deps {
                let v = version.as_str().unwrap_or("");
                let _ = writeln!(md, "- {dep}: {v}");
            }
        }
    }
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
    if !result.ok {
        return None;
    }
    serde_json::from_str(&result.content).ok()
}

#[async_trait]
impl SpecialHandler for NpmHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let name = package_name(url)?;
        let latest_url = format!("https://registry.npmjs.org/{name}/latest");
        let downloads_url = format!(
            "https://api.npmjs.org/downloads/point/last-week/{}",
            percent_encode_component(&name)
        );

        let pkg = fetch_json(&latest_url, timeout).await?;
        let weekly_downloads = fetch_json(&downloads_url, timeout)
            .await
            .and_then(|d| d.get("downloads").and_then(Value::as_u64));

        let md = render_markdown(&pkg, weekly_downloads);
        Some(build_result(
            &md,
            url,
            "npm",
            vec!["Fetched via npm registry".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn package_name_handles_plain_and_scoped_urls() {
        assert_eq!(
            package_name("https://www.npmjs.com/package/react").as_deref(),
            Some("react")
        );
        assert_eq!(
            package_name("https://www.npmjs.com/package/react/v/18").as_deref(),
            Some("react")
        );
        assert_eq!(
            package_name("https://www.npmjs.com/package/@types/node").as_deref(),
            Some("@types/node")
        );
        assert!(package_name("https://example.com/package/react").is_none());
        assert!(package_name("https://www.npmjs.com/").is_none());
    }

    #[test]
    fn markdown_renders_the_key_fields() {
        let pkg = json!({
            "name": "left-pad",
            "description": "pad a string",
            "version": "1.3.0",
            "license": { "type": "MIT" },
            "homepage": "https://example.com",
            "repository": "git+https://github.com/x/left-pad.git",
            "keywords": ["pad", "string"],
            "dependencies": { "dep-a": "^1.0.0" }
        });
        let md = render_markdown(&pkg, Some(1_234_567));
        assert!(md.contains("# left-pad"));
        assert!(md.contains("**Latest:** 1.3.0 · **License:** MIT"));
        assert!(md.contains("**Weekly Downloads:** 1,234,567"));
        assert!(md.contains("**Repository:** https://github.com/x/left-pad"));
        assert!(md.contains("**Keywords:** pad, string"));
        assert!(md.contains("- dep-a: ^1.0.0"));
    }

    #[test]
    fn license_and_repository_accept_both_shapes() {
        assert_eq!(
            license_str(&json!({ "license": "Apache-2.0" })).as_deref(),
            Some("Apache-2.0")
        );
        assert_eq!(
            license_str(&json!({ "license": { "type": "BSD" } })).as_deref(),
            Some("BSD")
        );
        assert_eq!(
            repository_url(&json!({ "repository": { "url": "git+https://x/y.git" } })).as_deref(),
            Some("https://x/y")
        );
    }
}
