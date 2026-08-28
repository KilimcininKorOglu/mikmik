// NuGet handler: renders a package via the v3 registration and search APIs.

use super::util::{
    build_result, format_iso_date, format_number, load_page, percent_encode_component, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct NuGetHandler;

/// The `(package, requested_version?)` from a `/packages/name[/version]` path.
fn parse_target(url: &str) -> Option<(String, Option<String>)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "www.nuget.org" && host != "nuget.org" {
        return None;
    }
    let rest = parsed.path().strip_prefix("/packages/")?;
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let name = super::util::percent_decode(parts.next()?);
    let version = parts.next().map(super::util::percent_decode);
    Some((name, version))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
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

/// Return a page's inlined `items`, fetching the page by `@id` when absent.
async fn resolve_page_items(page: &Value, timeout: Duration) -> Option<Vec<Value>> {
    if let Some(items) = page.get("items").and_then(Value::as_array) {
        return Some(items.clone());
    }
    let id = str_field(page, "@id")?;
    let fetched = fetch_json(id, timeout).await?;
    fetched
        .get("items")
        .and_then(Value::as_array)
        .map(|a| a.to_vec())
}

fn catalog_entry(item: &Value) -> Option<&Value> {
    item.get("catalogEntry")
}

/// Search every registration page for the requested version.
async fn find_requested(pages: &[Value], version: &str, timeout: Duration) -> Option<Value> {
    let wanted = version.to_lowercase();
    for page in pages {
        let items = resolve_page_items(page, timeout.min(Duration::from_secs(5))).await?;
        for item in &items {
            let entry = catalog_entry(item)?;
            if str_field(entry, "version")
                .map(str::to_lowercase)
                .as_deref()
                == Some(&wanted)
            {
                return Some(entry.clone());
            }
        }
    }
    None
}

/// Total downloads across all versions, via the search query API.
async fn fetch_downloads(name: &str, timeout: Duration) -> Option<u64> {
    let url = format!(
        "https://api.nuget.org/v3/query?q=packageid:{}&prerelease=true&take=1",
        percent_encode_component(name)
    );
    let data = fetch_json(&url, timeout.min(Duration::from_secs(5))).await?;
    data.get("data")
        .and_then(Value::as_array)?
        .first()?
        .get("totalDownloads")
        .and_then(Value::as_u64)
}

fn append_dependencies(md: &mut String, entry: &Value) {
    let Some(groups) = entry.get("dependencyGroups").and_then(Value::as_array) else {
        return;
    };
    let has_any = groups.iter().any(|g| {
        g.get("dependencies")
            .and_then(Value::as_array)
            .is_some_and(|d| !d.is_empty())
    });
    if !has_any {
        return;
    }
    md.push_str("\n## Dependencies\n\n");
    for group in groups {
        let Some(deps) = group.get("dependencies").and_then(Value::as_array) else {
            continue;
        };
        if deps.is_empty() {
            continue;
        }
        let framework = str_field(group, "targetFramework").unwrap_or("All Frameworks");
        let _ = write!(md, "### {framework}\n\n");
        for dep in deps {
            let id = str_field(dep, "id").unwrap_or("?");
            let range = str_field(dep, "range").unwrap_or("");
            let _ = writeln!(md, "- {id} ({range})");
        }
        md.push('\n');
    }
}

fn append_recent_versions(md: &mut String, items: &[Value]) {
    if items.len() <= 1 {
        return;
    }
    md.push_str("## Recent Versions\n\n");
    for item in items.iter().rev().take(5) {
        let Some(entry) = catalog_entry(item) else {
            continue;
        };
        let version = str_field(entry, "version").unwrap_or("?");
        let date = str_field(entry, "published")
            .map(format_iso_date)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let _ = writeln!(md, "- **{version}** ({date})");
    }
}

fn render(entry: &Value, downloads: Option<u64>, latest_items: &[Value]) -> String {
    let id = str_field(entry, "id").unwrap_or("(package)");
    let mut md = format!("# {id}\n\n");
    if let Some(desc) = str_field(entry, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    let version = str_field(entry, "version").unwrap_or("unknown");
    let _ = write!(md, "**Version:** {version}");
    if let Some(license) = str_field(entry, "licenseExpression") {
        let _ = write!(md, " · **License:** {license}");
    } else if let Some(url) = str_field(entry, "licenseUrl") {
        let _ = write!(md, " · **License:** [View]({url})");
    }
    md.push('\n');
    if let Some(total) = downloads {
        let _ = writeln!(md, "**Total Downloads:** {}", format_number(total));
    }
    if let Some(authors) = str_field(entry, "authors") {
        let _ = writeln!(md, "**Authors:** {authors}");
    }
    if let Some(project) = str_field(entry, "projectUrl") {
        let _ = writeln!(md, "**Project URL:** {project}");
    }
    if let Some(tags) = entry.get("tags").and_then(Value::as_array) {
        let list: Vec<&str> = tags.iter().filter_map(Value::as_str).collect();
        if !list.is_empty() {
            let _ = writeln!(md, "**Tags:** {}", list.join(", "));
        }
    }
    if let Some(published) = str_field(entry, "published") {
        let date = format_iso_date(published);
        if !date.is_empty() {
            let _ = writeln!(md, "**Published:** {date}");
        }
    }
    append_dependencies(&mut md, entry);
    md.push('\n');
    append_recent_versions(&mut md, latest_items);
    md
}

#[async_trait]
impl SpecialHandler for NuGetHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let (name, requested) = parse_target(url)?;
        let index_url = format!(
            "https://api.nuget.org/v3/registration5-gz-semver2/{}/index.json",
            name.to_lowercase()
        );
        let index = fetch_json(&index_url, timeout).await?;
        let pages = index.get("items").and_then(Value::as_array)?;
        if pages.is_empty() {
            return None;
        }
        let latest_items = resolve_page_items(pages.last()?, timeout).await?;
        if latest_items.is_empty() {
            return None;
        }

        let mut entry: Option<Value> = None;
        if let Some(version) = &requested {
            entry = find_requested(pages, version, timeout).await;
        }
        let entry = match entry {
            Some(e) => e,
            None => catalog_entry(latest_items.last()?)?.clone(),
        };

        let downloads = fetch_downloads(&name, timeout).await;
        let md = render(&entry, downloads, &latest_items);
        Some(build_result(
            &md,
            url,
            "nuget",
            vec!["Fetched via NuGet API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_reads_name_and_optional_version() {
        assert_eq!(
            parse_target("https://www.nuget.org/packages/Newtonsoft.Json"),
            Some(("Newtonsoft.Json".to_string(), None))
        );
        assert_eq!(
            parse_target("https://www.nuget.org/packages/Newtonsoft.Json/13.0.3"),
            Some(("Newtonsoft.Json".to_string(), Some("13.0.3".to_string())))
        );
        assert!(parse_target("https://example.com/packages/x").is_none());
    }

    #[test]
    fn render_lays_out_entry_downloads_and_deps() {
        let entry = json!({
            "id": "Newtonsoft.Json",
            "version": "13.0.3",
            "description": "JSON framework for .NET",
            "licenseExpression": "MIT",
            "authors": "James Newton-King",
            "projectUrl": "https://www.newtonsoft.com/json",
            "tags": ["json"],
            "published": "2023-03-08T00:00:00Z",
            "dependencyGroups": [{
                "targetFramework": ".NETStandard2.0",
                "dependencies": [{ "id": "Microsoft.CSharp", "range": "[4.3.0, )" }]
            }]
        });
        let items = vec![
            json!({ "catalogEntry": { "version": "13.0.2", "published": "2022-01-01T00:00:00Z" } }),
            json!({ "catalogEntry": { "version": "13.0.3", "published": "2023-03-08T00:00:00Z" } }),
        ];
        let md = render(&entry, Some(1_000_000_000), &items);
        assert!(md.contains("# Newtonsoft.Json"));
        assert!(md.contains("**Version:** 13.0.3 · **License:** MIT"));
        assert!(md.contains("**Total Downloads:** 1,000,000,000"));
        assert!(md.contains("### .NETStandard2.0\n\n- Microsoft.CSharp ([4.3.0, ))"));
        assert!(md.contains("## Recent Versions"));
        assert!(md.contains("- **13.0.3** (2023-03-08)"));
    }
}
