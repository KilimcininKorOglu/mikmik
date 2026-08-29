// Hackage handler: renders a Haskell package from the Hackage JSON version
// index plus its latest `.cabal` metadata file.

use super::util::{build_result, compare_versions, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct HackageHandler;

static PACKAGE_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/package/([^/]+)(?:/|$)").expect("static hackage regex"));

fn parse_package(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "hackage.haskell.org" {
        return None;
    }
    let raw = &PACKAGE_PATH.captures(parsed.path())?[1];
    Some(super::util::percent_decode(raw))
}

/// Highest version key in the Hackage version map, by SemVer ordering.
fn latest_version(version_map: &Value) -> Option<String> {
    let obj = version_map.as_object()?;
    obj.keys().max_by(|a, b| compare_versions(a, b)).cloned()
}

/// Read a single-line cabal field (`name: value`), case-insensitive on the key.
fn cabal_field(content: &str, field: &str) -> Option<String> {
    let prefix = format!("{}:", field.to_lowercase());
    content.lines().find_map(|line| {
        let lower = line.to_lowercase();
        lower
            .strip_prefix(&prefix)
            .map(|_| line[prefix.len()..].trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Read the `description:` field plus its indented continuation lines.
fn cabal_description(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.to_lowercase().starts_with("description:"))?;
    let first = lines[start]
        .split_once(':')
        .map(|x| x.1)
        .unwrap_or("")
        .trim()
        .to_string();
    let mut chunks = vec![first];
    for line in &lines[start + 1..] {
        if !line.starts_with("  ") {
            break;
        }
        chunks.push(line.trim().to_string());
    }
    let joined = chunks.join("\n").trim().to_string();
    (!joined.is_empty()).then_some(joined)
}

fn render(content: &str, package: &str, latest: &str) -> String {
    let name = cabal_field(content, "name").unwrap_or_else(|| package.to_string());
    let mut md = format!("# {name}\n\n");
    if let Some(synopsis) = cabal_field(content, "synopsis") {
        let _ = write!(md, "{synopsis}\n\n");
    }

    let version = cabal_field(content, "version").unwrap_or_else(|| latest.to_string());
    let _ = write!(md, "**Version:** {version}");
    if let Some(license) = cabal_field(content, "license") {
        let _ = write!(md, " · **License:** {license}");
    }
    md.push('\n');

    for (field, label) in [
        ("author", "Author"),
        ("maintainer", "Maintainer"),
        ("category", "Category"),
        ("stability", "Stability"),
        ("homepage", "Homepage"),
        ("bug-reports", "Bug Reports"),
    ] {
        if let Some(value) = cabal_field(content, field) {
            let _ = writeln!(md, "**{label}:** {value}");
        }
    }

    if let Some(description) = cabal_description(content) {
        let _ = write!(md, "\n## Description\n\n{description}\n");
    }
    md
}

async fn fetch_text(url: &str, timeout: Duration, accept: &str) -> Option<String> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            headers: vec![("Accept".to_string(), accept.to_string())],
            ..Default::default()
        },
    )
    .await;
    result.ok.then_some(result.content)
}

#[async_trait]
impl SpecialHandler for HackageHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let package = parse_package(url)?;
        let encoded = super::util::percent_encode_component(&package);
        let version_url = format!("https://hackage.haskell.org/package/{encoded}.json");
        let version_body = fetch_text(&version_url, timeout, "application/json").await?;
        let version_map: Value = serde_json::from_str(&version_body).ok()?;
        let latest = latest_version(&version_map)?;

        let cabal_url =
            format!("https://hackage.haskell.org/package/{encoded}-{latest}/{encoded}.cabal");
        let cabal = fetch_text(&cabal_url, timeout, "text/plain").await?;

        let md = render(&cabal, &package, &latest);
        Some(build_result(
            &md,
            url,
            "hackage",
            vec!["Fetched via Hackage API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_package_reads_name_from_path() {
        assert_eq!(
            parse_package("https://hackage.haskell.org/package/aeson"),
            Some("aeson".to_string())
        );
        assert_eq!(
            parse_package("https://hackage.haskell.org/package/aeson-2.2.0.0/docs"),
            Some("aeson-2.2.0.0".to_string())
        );
        assert_eq!(parse_package("https://example.com/package/aeson"), None);
    }

    #[test]
    fn latest_version_picks_highest_semver() {
        let map = json!({ "1.0": "normal", "1.10": "normal", "1.9": "normal" });
        assert_eq!(latest_version(&map), Some("1.10".to_string()));
    }

    #[test]
    fn render_reads_cabal_fields_and_description() {
        let cabal = "name: aeson\nversion: 2.2.0.0\nsynopsis: Fast JSON\nlicense: BSD3\nauthor: Bryan\ncategory: Text, Web, JSON\ndescription:\n  A JSON parsing and\n  encoding library.\n";
        let md = render(cabal, "aeson", "2.2.0.0");
        assert!(md.contains("# aeson"));
        assert!(md.contains("Fast JSON"));
        assert!(md.contains("**Version:** 2.2.0.0 · **License:** BSD3"));
        assert!(md.contains("**Author:** Bryan"));
        assert!(md.contains("**Category:** Text, Web, JSON"));
        assert!(md.contains("## Description\n\nA JSON parsing and\nencoding library."));
    }
}
