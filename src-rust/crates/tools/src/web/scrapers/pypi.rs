// PyPI handler: renders a pypi.org project page from the JSON API.

use super::util::{
    build_result, format_number, load_page, percent_decode, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct PypiHandler;

/// The project name from a `pypi.org/project/{name}` URL.
fn project_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "pypi.org" && host != "www.pypi.org" {
        return None;
    }
    let rest = parsed.path().strip_prefix("/project/")?;
    let name = rest.split('/').next().filter(|s| !s.is_empty())?;
    Some(percent_decode(name))
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

fn render_markdown(info: &Value, pkg: &Value, weekly: Option<u64>) -> String {
    let name = info
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("(package)");
    let mut md = format!("# {name}\n\n");
    if let Some(summary) = info.get("summary").and_then(Value::as_str) {
        let _ = writeln!(md, "{summary}\n");
    }
    let version = info
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let _ = write!(md, "**Latest:** {version}");
    if let Some(license) = info
        .get("license")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let _ = write!(md, " · **License:** {license}");
    }
    md.push('\n');
    if let Some(weekly) = weekly {
        let _ = writeln!(md, "**Weekly Downloads:** {}", format_number(weekly));
    }
    md.push('\n');
    append_meta(&mut md, info);
    append_deps(&mut md, pkg);
    if let Some(desc) = info
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let _ = write!(md, "\n---\n\n## Description\n\n{desc}\n");
    }
    md
}

fn append_meta(md: &mut String, info: &Value) {
    if let Some(author) = info
        .get("author")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let _ = write!(md, "**Author:** {author}");
        if let Some(email) = info
            .get("author_email")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let _ = write!(md, " <{email}>");
        }
        md.push('\n');
    }
    if let Some(py) = info
        .get("requires_python")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let _ = writeln!(md, "**Python:** {py}");
    }
    if let Some(home) = info
        .get("home_page")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let _ = writeln!(md, "**Homepage:** {home}");
    }
    if let Some(urls) = info.get("project_urls").and_then(Value::as_object) {
        if !urls.is_empty() {
            md.push_str("\n**Project URLs:**\n");
            for (label, url) in urls {
                if let Some(url) = url.as_str() {
                    let _ = writeln!(md, "- {label}: {url}");
                }
            }
        }
    }
    if let Some(keywords) = info
        .get("keywords")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let _ = write!(md, "\n**Keywords:** {keywords}\n");
    }
}

fn append_deps(md: &mut String, pkg: &Value) {
    if let Some(deps) = pkg.get("requires_dist").and_then(Value::as_array) {
        let deps: Vec<&str> = deps.iter().filter_map(Value::as_str).collect();
        if !deps.is_empty() {
            md.push_str("\n## Dependencies\n\n");
            for dep in deps {
                let _ = writeln!(md, "- {dep}");
            }
        }
    }
}

#[async_trait]
impl SpecialHandler for PypiHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let name = project_name(url)?;
        let api_url = format!("https://pypi.org/pypi/{name}/json");
        let downloads_url = format!("https://pypistats.org/api/packages/{name}/recent");

        let pkg = fetch_json(&api_url, timeout).await?;
        let info = pkg.get("info")?.clone();
        let weekly = fetch_json(&downloads_url, timeout.min(Duration::from_secs(5)))
            .await
            .and_then(|d| {
                d.get("data")
                    .and_then(|x| x.get("last_week"))
                    .and_then(Value::as_u64)
            });

        let md = render_markdown(&info, &pkg, weekly);
        Some(build_result(
            &md,
            url,
            "pypi",
            vec!["Fetched via PyPI JSON API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_name_parses_and_rejects_other_hosts() {
        assert_eq!(
            project_name("https://pypi.org/project/requests/").as_deref(),
            Some("requests")
        );
        assert_eq!(
            project_name("https://pypi.org/project/Flask/2.0/").as_deref(),
            Some("Flask")
        );
        assert!(project_name("https://example.com/project/x").is_none());
    }

    #[test]
    fn markdown_renders_info_and_deps() {
        let info = json!({
            "name": "requests",
            "summary": "HTTP for Humans",
            "version": "2.31.0",
            "license": "Apache-2.0",
            "author": "Kenneth Reitz",
            "requires_python": ">=3.7",
            "project_urls": { "Source": "https://github.com/psf/requests" },
            "keywords": "http"
        });
        let pkg = json!({ "info": info, "requires_dist": ["urllib3 (>=1.21.1)"] });
        let md = render_markdown(&info, &pkg, Some(9_000_000));
        assert!(md.contains("# requests"));
        assert!(md.contains("**Latest:** 2.31.0 · **License:** Apache-2.0"));
        assert!(md.contains("**Weekly Downloads:** 9,000,000"));
        assert!(md.contains("**Author:** Kenneth Reitz"));
        assert!(md.contains("- Source: https://github.com/psf/requests"));
        assert!(md.contains("- urllib3 (>=1.21.1)"));
    }
}
