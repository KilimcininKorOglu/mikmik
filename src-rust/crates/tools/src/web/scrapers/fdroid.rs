// F-Droid handler: renders an Android app from the F-Droid package API.

use super::util::{build_result, load_page, localized_text, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::time::Duration;

pub struct FdroidHandler;

static PACKAGE_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/(?:en/)?packages/([^/]+)").expect("static fdroid regex"));

fn parse_package(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "f-droid.org" && host != "www.f-droid.org" {
        return None;
    }
    let raw = &PACKAGE_PATH.captures(parsed.path())?[1];
    Some(super::util::percent_decode(raw))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Author display name from the several shapes F-Droid uses.
fn normalize_author(data: &Value) -> Option<String> {
    if let Some(name) = str_field(data, "authorName") {
        return Some(name.to_string());
    }
    match data.get("author") {
        Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
        Some(obj) => {
            if let Some(name) = str_field(obj, "name") {
                return Some(name.to_string());
            }
        }
        None => {}
    }
    str_field(data, "authorEmail").map(str::to_string)
}

fn normalize_author_email(data: &Value) -> Option<String> {
    if let Some(email) = str_field(data, "authorEmail") {
        return Some(email.to_string());
    }
    data.get("author")
        .filter(|a| !a.is_string())
        .and_then(|a| str_field(a, "email"))
        .map(str::to_string)
}

/// Union of anti-features declared on the app and on each package build.
fn collect_anti_features(data: &Value) -> Vec<String> {
    let mut values = BTreeSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut push = |feature: &str| {
        if values.insert(feature.to_string()) {
            order.push(feature.to_string());
        }
    };
    for feature in str_list(data, "antiFeatures") {
        push(feature);
    }
    if let Some(packages) = data.get("packages").and_then(Value::as_array) {
        for pkg in packages {
            for feature in str_list(pkg, "antiFeatures") {
                push(feature);
            }
        }
    }
    order
}

fn str_list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Suggested version name, resolved through the version code when needed.
fn resolve_suggested_version(data: &Value) -> Option<String> {
    if let Some(name) = str_field(data, "suggestedVersionName") {
        return Some(name.to_string());
    }
    let packages = data.get("packages").and_then(Value::as_array);
    if let Some(code) = data.get("suggestedVersionCode").and_then(Value::as_i64) {
        let matched = packages.and_then(|list| {
            list.iter()
                .find(|p| p.get("versionCode").and_then(Value::as_i64) == Some(code))
                .and_then(|p| str_field(p, "versionName"))
        });
        if let Some(name) = matched {
            return Some(name.to_string());
        }
    }
    packages
        .and_then(|list| list.first())
        .and_then(|p| str_field(p, "versionName"))
        .map(str::to_string)
}

fn append_version_history(md: &mut String, data: &Value) {
    let Some(packages) = data
        .get("packages")
        .and_then(Value::as_array)
        .filter(|p| !p.is_empty())
    else {
        return;
    };
    md.push_str("\n## Version History\n\n");
    for version in packages.iter().take(10) {
        let label = str_field(version, "versionName").unwrap_or("unknown");
        let code = version
            .get("versionCode")
            .and_then(Value::as_i64)
            .map(|c| format!(" ({c})"))
            .unwrap_or_default();
        let _ = writeln!(md, "- {label}{code}");
    }
}

fn render(data: &Value, package_name: &str) -> String {
    let display_name = data
        .get("name")
        .and_then(localized_text)
        .unwrap_or_else(|| package_name.to_string());
    let summary = data.get("summary").and_then(localized_text);
    let description = data.get("description").and_then(localized_text);

    let mut md = format!("# {display_name}\n\n");
    if let Some(summary) = &summary {
        let _ = write!(md, "{summary}\n\n");
    }

    let _ = write!(md, "**Package:** {package_name}");
    if let Some(latest) = resolve_suggested_version(data) {
        let _ = write!(md, " · **Latest:** {latest}");
    }
    if let Some(license) = str_field(data, "license") {
        let _ = write!(md, " · **License:** {license}");
    }
    md.push('\n');

    if let Some(author) = normalize_author(data) {
        let _ = write!(md, "**Author:** {author}");
        if let Some(email) = normalize_author_email(data).filter(|e| *e != author) {
            let _ = write!(md, " <{email}>");
        }
        md.push('\n');
    }
    if let Some(source) = str_field(data, "sourceCode") {
        let _ = writeln!(md, "**Source Code:** {source}");
    }
    let categories = str_list(data, "categories");
    if !categories.is_empty() {
        let _ = writeln!(md, "**Categories:** {}", categories.join(", "));
    }
    let anti_features = collect_anti_features(data);
    if !anti_features.is_empty() {
        let _ = writeln!(md, "**Anti-Features:** {}", anti_features.join(", "));
    }

    if let Some(description) = description {
        let _ = write!(md, "\n## Description\n\n{description}\n");
    }
    append_version_history(&mut md, data);
    md
}

#[async_trait]
impl SpecialHandler for FdroidHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let package_name = parse_package(url)?;
        let api_url = format!(
            "https://f-droid.org/api/v1/packages/{}",
            super::util::percent_encode_component(&package_name)
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
        let md = render(&data, &package_name);
        Some(build_result(
            &md,
            url,
            "fdroid",
            vec!["Fetched via F-Droid API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_package_reads_name_with_optional_locale() {
        assert_eq!(
            parse_package("https://f-droid.org/packages/org.fdroid.fdroid"),
            Some("org.fdroid.fdroid".to_string())
        );
        assert_eq!(
            parse_package("https://f-droid.org/en/packages/org.fdroid.fdroid/"),
            Some("org.fdroid.fdroid".to_string())
        );
        assert_eq!(parse_package("https://example.com/packages/x"), None);
    }

    #[test]
    fn suggested_version_resolves_through_code() {
        let data = json!({
            "suggestedVersionCode": 1002,
            "packages": [
                { "versionName": "1.2", "versionCode": 1002 },
                { "versionName": "1.1", "versionCode": 1001 }
            ]
        });
        assert_eq!(resolve_suggested_version(&data), Some("1.2".to_string()));
    }

    #[test]
    fn render_lays_out_app_metadata() {
        let data = json!({
            "name": { "en-US": "F-Droid" },
            "summary": { "en-US": "The app store" },
            "description": { "en-US": "An installable catalogue." },
            "authorName": "F-Droid Team",
            "license": "GPL-3.0",
            "sourceCode": "https://gitlab.com/fdroid/fdroidclient",
            "categories": ["System"],
            "antiFeatures": ["NonFreeNet"],
            "suggestedVersionName": "1.16",
            "packages": [{ "versionName": "1.16", "versionCode": 1016 }]
        });
        let md = render(&data, "org.fdroid.fdroid");
        assert!(md.contains("# F-Droid"));
        assert!(md.contains("The app store"));
        assert!(
            md.contains("**Package:** org.fdroid.fdroid · **Latest:** 1.16 · **License:** GPL-3.0")
        );
        assert!(md.contains("**Author:** F-Droid Team"));
        assert!(md.contains("**Anti-Features:** NonFreeNet"));
        assert!(md.contains("## Description\n\nAn installable catalogue."));
        assert!(md.contains("- 1.16 (1016)"));
    }
}
