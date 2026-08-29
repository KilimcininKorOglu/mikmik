// Flathub handler: renders an app from the Flathub AppStream API.

use super::util::{
    build_result, format_epoch_millis, format_number, html_to_markdown, load_page, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::time::Duration;

pub struct FlathubHandler;

static DETAILS_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/apps/details/([^/]+)/?$").expect("static flathub details regex"));
static APP_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/apps/([^/]+)/?$").expect("static flathub app regex"));

fn parse_app_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "flathub.org" && host != "www.flathub.org" {
        return None;
    }
    let path = parsed.path();
    let caps = DETAILS_PATH
        .captures(path)
        .or_else(|| APP_PATH.captures(path))?;
    Some(super::util::percent_decode(&caps[1]))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Parse a numeric value that may arrive as a number or a digit string.
fn parse_number(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        }
        _ => None,
    }
}

fn extract_installs(app: &Value) -> Option<u64> {
    if let Some(n) = app.get("installs").and_then(parse_number) {
        return Some(n);
    }
    let metadata = app.get("metadata").and_then(Value::as_object)?;
    metadata
        .iter()
        .find(|(k, _)| k.to_lowercase().contains("install"))
        .and_then(|(_, v)| parse_number(v))
}

/// Largest screenshot source by pixel area, else the first available.
fn best_screenshot_url(screenshot: &Value) -> Option<String> {
    let sizes = screenshot.get("sizes").and_then(Value::as_array)?;
    let area = |s: &Value| -> u64 {
        let w = s.get("width").and_then(parse_number).unwrap_or(0);
        let h = s.get("height").and_then(parse_number).unwrap_or(0);
        w * h
    };
    let best = sizes.iter().max_by_key(|s| area(s));
    best.and_then(|s| str_field(s, "src"))
        .or_else(|| sizes.first().and_then(|s| str_field(s, "src")))
        .map(str::to_string)
}

fn append_categories(md: &mut String, app: &Value) {
    if let Some(categories) = app
        .get("categories")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty())
    {
        md.push_str("\n## Categories\n\n");
        for category in categories.iter().filter_map(Value::as_str) {
            let _ = writeln!(md, "- {category}");
        }
    }
}

fn append_screenshots(md: &mut String, app: &Value) {
    let Some(shots) = app
        .get("screenshots")
        .and_then(Value::as_array)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    md.push_str("\n## Screenshots\n\n");
    for shot in shots.iter().take(5) {
        if let Some(url) = best_screenshot_url(shot) {
            let caption = str_field(shot, "caption")
                .map(|c| format!(" - {c}"))
                .unwrap_or_default();
            let _ = writeln!(md, "- {url}{caption}");
        }
    }
}

fn append_releases(md: &mut String, app: &Value) {
    let Some(releases) = app
        .get("releases")
        .and_then(Value::as_array)
        .filter(|r| !r.is_empty())
    else {
        return;
    };
    md.push_str("\n## Releases\n\n");
    for release in releases.iter().take(5) {
        let version = str_field(release, "version").unwrap_or("unknown");
        let mut line = format!("- **{version}**");
        if let Some(ts) = release.get("timestamp").and_then(parse_number) {
            let date = format_epoch_millis(ts as i64 * 1000);
            if !date.is_empty() {
                let _ = write!(line, " ({date})");
            }
        }
        if let Some(kind) = str_field(release, "type") {
            let _ = write!(line, " · {kind}");
        }
        if let Some(url) = str_field(release, "url") {
            let _ = write!(line, " · {url}");
        }
        let _ = writeln!(md, "{line}");
        if let Some(desc) = str_field(release, "description") {
            let text = html_to_markdown(desc).replace('\n', " ");
            let text = text.trim();
            if !text.is_empty() {
                let _ = writeln!(md, "  - {text}");
            }
        }
    }
}

fn render(app: &Value, app_id: &str) -> String {
    let name = str_field(app, "name")
        .or_else(|| str_field(app, "id"))
        .unwrap_or(app_id);
    let mut md = format!("# {name}\n\n");
    if let Some(summary) = str_field(app, "summary") {
        let _ = write!(md, "{summary}\n\n");
    }
    md.push_str("## Metadata\n\n");
    let _ = writeln!(md, "**App ID:** {}", str_field(app, "id").unwrap_or(app_id));
    if let Some(developer) = str_field(app, "developer_name") {
        let _ = writeln!(md, "**Developer:** {developer}");
    }
    if let Some(installs) = extract_installs(app) {
        let _ = writeln!(md, "**Installs:** {}", format_number(installs));
    }

    append_categories(&mut md, app);
    if let Some(description) = str_field(app, "description") {
        let text = html_to_markdown(description);
        if !text.is_empty() {
            let _ = write!(md, "\n## Description\n\n{text}\n");
        }
    }
    append_permissions(&mut md, app);
    append_screenshots(&mut md, app);
    append_releases(&mut md, app);
    md
}

/// De-duplicated permission strings from the app and its metadata map.
fn append_permissions(md: &mut String, app: &Value) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut push = |value: String| {
        if seen.insert(value.clone()) {
            order.push(value);
        }
    };
    if let Some(list) = app.get("permissions").and_then(Value::as_array) {
        for value in list.iter().filter_map(Value::as_str) {
            push(value.to_string());
        }
    }
    if let Some(metadata) = app.get("metadata").and_then(Value::as_object) {
        for (key, value) in metadata {
            if !key.to_lowercase().contains("permission") {
                continue;
            }
            match value {
                Value::Array(items) => {
                    for item in items.iter().filter_map(Value::as_str) {
                        push(item.to_string());
                    }
                }
                Value::String(s) => push(format!("{key}: {s}")),
                Value::Number(n) => push(format!("{key}: {n}")),
                Value::Bool(b) => push(format!("{key}: {b}")),
                _ => {}
            }
        }
    }
    if !order.is_empty() {
        md.push_str("\n## Permissions\n\n");
        for permission in order {
            let _ = writeln!(md, "- {permission}");
        }
    }
}

#[async_trait]
impl SpecialHandler for FlathubHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let app_id = parse_app_id(url)?;
        let api_url = format!(
            "https://flathub.org/api/v2/appstream/{}",
            super::util::percent_encode_component(&app_id)
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
        let app: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&app, &app_id);
        Some(build_result(
            &md,
            url,
            "flathub-appstream",
            vec!["Fetched via Flathub Appstream API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_app_id_reads_both_url_shapes() {
        assert_eq!(
            parse_app_id("https://flathub.org/apps/org.gimp.GIMP"),
            Some("org.gimp.GIMP".to_string())
        );
        assert_eq!(
            parse_app_id("https://flathub.org/apps/details/org.gimp.GIMP"),
            Some("org.gimp.GIMP".to_string())
        );
        assert_eq!(parse_app_id("https://example.com/apps/x"), None);
    }

    #[test]
    fn best_screenshot_picks_largest_area() {
        let shot = json!({ "sizes": [
            { "src": "small.png", "width": "100", "height": "100" },
            { "src": "big.png", "width": "800", "height": "600" }
        ] });
        assert_eq!(best_screenshot_url(&shot), Some("big.png".to_string()));
    }

    #[test]
    fn render_lays_out_app_with_html_description() {
        let app = json!({
            "id": "org.gimp.GIMP",
            "name": "GIMP",
            "summary": "GNU Image Manipulation Program",
            "developer_name": "The GIMP Team",
            "installs": 1234567,
            "categories": ["Graphics"],
            "description": "<p>An <strong>image editor</strong>.</p>",
            "releases": [{ "version": "2.10", "timestamp": 1609459200, "type": "stable" }]
        });
        let md = render(&app, "org.gimp.GIMP");
        assert!(md.contains("# GIMP"));
        assert!(md.contains("**App ID:** org.gimp.GIMP"));
        assert!(md.contains("**Installs:** 1,234,567"));
        assert!(md.contains("- Graphics"));
        assert!(md.contains("## Description\n\nAn **image editor**."));
        assert!(md.contains("- **2.10** (2021-01-01) · stable"));
    }
}
