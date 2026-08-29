// Firefox Add-ons handler: renders an add-on from the addons.mozilla.org API.

use super::util::{
    build_result, format_number, html_to_markdown, load_page, localized_text, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::time::Duration;

pub struct FirefoxAddonsHandler;

const MAX_PERMISSIONS: usize = 40;

fn parse_slug(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "addons.mozilla.org" {
        return None;
    }
    let segments: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    let index = segments.iter().position(|s| *s == "addon")?;
    let slug = segments.get(index + 1)?;
    Some(super::util::percent_decode(slug))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Flatten categories, which arrive as a list or a `{app: [names]}` map.
fn normalize_categories(categories: &Value) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut push = |item: &str| {
        if !item.is_empty() && seen.insert(item.to_string()) {
            out.push(item.to_string());
        }
    };
    match categories {
        Value::Array(list) => {
            for item in list.iter().filter_map(Value::as_str) {
                push(item);
            }
        }
        Value::Object(map) => {
            for list in map.values() {
                for item in list
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    push(item);
                }
            }
        }
        _ => {}
    }
    out
}

/// De-duplicated permissions across the four manifest permission fields.
fn collect_permissions(file: &Value) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for key in [
        "permissions",
        "host_permissions",
        "optional_permissions",
        "optional_host_permissions",
    ] {
        for item in file
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(item) = item.as_str().filter(|s| !s.is_empty()) {
                if seen.insert(item.to_string()) {
                    out.push(item.to_string());
                }
            }
        }
    }
    out
}

fn append_authors(md: &mut String, data: &Value) {
    let Some(authors) = data.get("authors").and_then(Value::as_array) else {
        return;
    };
    let names: Vec<String> = authors
        .iter()
        .filter_map(|a| str_field(a, "name"))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    if !names.is_empty() {
        let label = if names.len() > 1 { "Authors" } else { "Author" };
        let _ = writeln!(md, "**{label}:** {}", names.join(", "));
    }
}

fn append_license(md: &mut String, license: &Value) {
    let name = localized_text(license.get("name").unwrap_or(&Value::Null))
        .or_else(|| str_field(license, "slug").map(str::to_string));
    let url = str_field(license, "url");
    match (name, url) {
        (Some(name), Some(url)) => {
            let _ = writeln!(md, "**License:** [{name}]({url})");
        }
        (Some(name), None) => {
            let _ = writeln!(md, "**License:** {name}");
        }
        (None, Some(url)) => {
            let _ = writeln!(md, "**License:** {url}");
        }
        (None, None) => {}
    }
}

fn append_permissions(md: &mut String, permissions: &[String]) {
    if permissions.is_empty() {
        return;
    }
    let _ = write!(md, "\n## Permissions ({})\n\n", permissions.len());
    for permission in permissions.iter().take(MAX_PERMISSIONS) {
        let _ = writeln!(md, "- {permission}");
    }
    if permissions.len() > MAX_PERMISSIONS {
        let _ = write!(
            md,
            "\n[…{} permissions elided…]\n",
            permissions.len() - MAX_PERMISSIONS
        );
    }
}

fn render(data: &Value, slug: &str) -> String {
    let name = localized_text(data.get("name").unwrap_or(&Value::Null))
        .unwrap_or_else(|| slug.to_string());
    let mut md = format!("# {name}\n\n");
    if let Some(summary) = localized_text(data.get("summary").unwrap_or(&Value::Null)) {
        let _ = write!(md, "{summary}\n\n");
    }
    append_authors(&mut md, data);

    let ratings = data.get("ratings").cloned().unwrap_or(Value::Null);
    if let Some(average) = ratings.get("average").and_then(Value::as_f64) {
        let _ = write!(md, "**Rating:** {average:.2}");
        if let Some(count) = ratings.get("count").and_then(Value::as_u64) {
            let _ = write!(md, " ({} reviews)", format_number(count));
        }
        md.push('\n');
    }
    let users = data
        .get("average_daily_users")
        .and_then(Value::as_u64)
        .or_else(|| data.get("weekly_downloads").and_then(Value::as_u64));
    if let Some(users) = users {
        let _ = writeln!(md, "**Users:** {}", format_number(users));
    }
    let version = data.get("current_version").cloned().unwrap_or(Value::Null);
    if let Some(v) = str_field(&version, "version") {
        let _ = writeln!(md, "**Version:** {v}");
    }
    let categories = normalize_categories(data.get("categories").unwrap_or(&Value::Null));
    if !categories.is_empty() {
        let _ = writeln!(md, "**Categories:** {}", categories.join(", "));
    }
    if let Some(license) = version.get("license") {
        append_license(&mut md, license);
    }
    if let Some(homepage) = data.get("homepage").and_then(|h| {
        localized_text(h.get("url").unwrap_or(&Value::Null))
            .or_else(|| localized_text(h.get("outgoing").unwrap_or(&Value::Null)))
    }) {
        let _ = writeln!(md, "**Homepage:** {homepage}");
    }

    if let Some(description) = localized_text(data.get("description").unwrap_or(&Value::Null)) {
        let text = html_to_markdown(&description);
        if !text.is_empty() {
            let _ = write!(md, "\n## Description\n\n{text}\n");
        }
    }
    let permissions = version
        .get("file")
        .map(collect_permissions)
        .unwrap_or_default();
    append_permissions(&mut md, &permissions);
    md
}

#[async_trait]
impl SpecialHandler for FirefoxAddonsHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let slug = parse_slug(url)?;
        let api_url = format!(
            "https://addons.mozilla.org/api/v5/addons/addon/{}/",
            super::util::percent_encode_component(&slug)
        );
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
        let md = render(&data, &slug);
        Some(build_result(
            &md,
            url,
            "firefox-addons",
            vec!["Fetched via Firefox Add-ons API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_slug_reads_addon_segment() {
        assert_eq!(
            parse_slug("https://addons.mozilla.org/en-US/firefox/addon/ublock-origin/"),
            Some("ublock-origin".to_string())
        );
        assert_eq!(parse_slug("https://example.com/addon/x"), None);
    }

    #[test]
    fn categories_flatten_from_map() {
        let categories = json!({ "firefox": ["privacy", "security"], "android": ["privacy"] });
        assert_eq!(
            normalize_categories(&categories),
            vec!["privacy".to_string(), "security".to_string()]
        );
    }

    #[test]
    fn render_lays_out_addon() {
        let data = json!({
            "name": { "en-US": "uBlock Origin" },
            "summary": { "en-US": "An efficient blocker." },
            "authors": [{ "name": "Raymond Hill" }],
            "ratings": { "average": 4.78, "count": 15000 },
            "average_daily_users": 6000000,
            "categories": ["privacy"],
            "current_version": {
                "version": "1.54.0",
                "license": { "name": { "en-US": "GPL-3.0" }, "url": "https://x.test/gpl" },
                "file": { "permissions": ["tabs", "webRequest"] }
            },
            "description": { "en-US": "<p>Blocks <strong>ads</strong>.</p>" }
        });
        let md = render(&data, "ublock-origin");
        assert!(md.contains("# uBlock Origin"));
        assert!(md.contains("**Author:** Raymond Hill"));
        assert!(md.contains("**Rating:** 4.78 (15,000 reviews)"));
        assert!(md.contains("**Users:** 6,000,000"));
        assert!(md.contains("**License:** [GPL-3.0](https://x.test/gpl)"));
        assert!(md.contains("## Description\n\nBlocks **ads**."));
        assert!(md.contains("## Permissions (2)"));
        assert!(md.contains("- tabs"));
    }
}
