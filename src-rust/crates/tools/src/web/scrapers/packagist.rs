// Packagist handler: renders a PHP Composer package via the JSON API.

use super::util::{
    build_result, format_number, load_page, percent_decode, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::fmt::Write;
use std::time::Duration;

pub struct PackagistHandler;

/// The `vendor/name` from a `/packages/{vendor}/{name}` path.
fn package_id(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "packagist.org" && host != "www.packagist.org" {
        return None;
    }
    let rest = parsed.path().strip_prefix("/packages/")?;
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let vendor = parts.next()?;
    let name = parts.next()?;
    Some((percent_decode(vendor), percent_decode(name)))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn is_dev_key(key: &str) -> bool {
    key == "dev-master" || key == "dev-main" || key.contains("-dev")
}

/// Pick the newest stable version by release `time`, falling back to dev.
fn latest_version(versions: &Map<String, Value>) -> (String, Option<&Value>) {
    let mut best_key = String::new();
    let mut best: Option<&Value> = None;
    let mut best_time = "";
    for (key, ver) in versions {
        if is_dev_key(key) {
            continue;
        }
        let time = str_field(ver, "time").unwrap_or("");
        if best.is_none() || time > best_time {
            best = Some(ver);
            best_key = key.clone();
            best_time = time;
        }
    }
    if best.is_none() {
        for fallback in ["dev-master", "dev-main"] {
            if let Some(ver) = versions.get(fallback) {
                return (fallback.to_string(), Some(ver));
            }
        }
        if let Some((key, ver)) = versions.iter().next() {
            return (key.clone(), Some(ver));
        }
    }
    (best_key, best)
}

fn append_authors(md: &mut String, latest: &Value) {
    let Some(authors) = latest.get("authors").and_then(Value::as_array) else {
        return;
    };
    let names: Vec<String> = authors
        .iter()
        .filter_map(|a| {
            let name = str_field(a, "name")?;
            Some(match str_field(a, "email") {
                Some(email) => format!("{name} <{email}>"),
                None => name.to_string(),
            })
        })
        .collect();
    if !names.is_empty() {
        let _ = writeln!(md, "**Authors:** {}", names.join(", "));
    }
}

fn append_repository(md: &mut String, pkg: &Value, latest: &Value) {
    if let Some(repo) = str_field(pkg, "repository") {
        let _ = writeln!(md, "**Repository:** {repo}");
    } else if let Some(src) = latest.get("source").and_then(|s| str_field(s, "url")) {
        let repo = src.strip_suffix(".git").unwrap_or(src);
        let _ = writeln!(md, "**Repository:** {repo}");
    }
}

fn append_requirements(md: &mut String, latest: &Value, key: &str, heading: &str) {
    let Some(reqs) = latest.get(key).and_then(Value::as_object) else {
        return;
    };
    if reqs.is_empty() {
        return;
    }
    let _ = write!(md, "\n## {heading}\n\n");
    for (dep, version) in reqs {
        let v = version.as_str().unwrap_or("");
        let _ = writeln!(md, "- {dep}: {v}");
    }
}

fn licenses(latest: &Value) -> Vec<String> {
    latest
        .get("license")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn render(pkg: &Value) -> String {
    let name = str_field(pkg, "name").unwrap_or("(package)");
    let mut md = format!("# {name}\n\n");
    if let Some(desc) = str_field(pkg, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    let empty = Map::new();
    let versions = pkg
        .get("versions")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let (latest_key, latest) = latest_version(versions);
    let latest = latest.cloned().unwrap_or(Value::Null);

    let _ = write!(
        md,
        "**Latest:** {}",
        if latest_key.is_empty() {
            "unknown"
        } else {
            &latest_key
        }
    );
    let ls = licenses(&latest);
    if !ls.is_empty() {
        let _ = write!(md, " · **License:** {}", ls.join(", "));
    }
    if let Some(t) = str_field(pkg, "type") {
        let _ = write!(md, " · **Type:** {t}");
    }
    md.push('\n');

    if let Some(downloads) = pkg.get("downloads") {
        let total = downloads.get("total").and_then(Value::as_u64).unwrap_or(0);
        let monthly = downloads
            .get("monthly")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let _ = writeln!(
            md,
            "**Downloads:** {} total · {}/month",
            format_number(total),
            format_number(monthly)
        );
    }
    if let Some(favers) = pkg.get("favers").and_then(Value::as_u64) {
        let _ = writeln!(md, "**Stars:** {}", format_number(favers));
    }
    md.push('\n');

    append_authors(&mut md, &latest);
    if let Some(maintainers) = pkg.get("maintainers").and_then(Value::as_array) {
        let names: Vec<&str> = maintainers
            .iter()
            .filter_map(|m| str_field(m, "name"))
            .collect();
        if !names.is_empty() {
            let _ = writeln!(md, "**Maintainers:** {}", names.join(", "));
        }
    }
    if let Some(homepage) = str_field(&latest, "homepage") {
        let _ = writeln!(md, "**Homepage:** {homepage}");
    }
    append_repository(&mut md, pkg, &latest);
    append_requirements(&mut md, &latest, "require", "Requirements");
    append_requirements(&mut md, &latest, "require-dev", "Dev Requirements");
    md
}

#[async_trait]
impl SpecialHandler for PackagistHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let (vendor, name) = package_id(url)?;
        let api_url = format!("https://packagist.org/packages/{vendor}/{name}.json");
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let pkg = data.get("package")?;
        let md = render(pkg);
        Some(build_result(
            &md,
            url,
            "packagist",
            vec!["Fetched via Packagist API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn package_id_reads_vendor_and_name() {
        assert_eq!(
            package_id("https://packagist.org/packages/monolog/monolog"),
            Some(("monolog".to_string(), "monolog".to_string()))
        );
        assert!(package_id("https://example.com/packages/a/b").is_none());
    }

    #[test]
    fn latest_version_prefers_newest_stable() {
        let versions = json!({
            "1.0.0": { "time": "2020-01-01T00:00:00+00:00" },
            "2.0.0": { "time": "2022-01-01T00:00:00+00:00" },
            "dev-main": { "time": "2099-01-01T00:00:00+00:00" }
        });
        let map = versions.as_object().expect("map");
        let (key, _) = latest_version(map);
        assert_eq!(key, "2.0.0");
    }

    #[test]
    fn render_lays_out_package() {
        let pkg = json!({
            "name": "monolog/monolog",
            "description": "Logging for PHP",
            "type": "library",
            "downloads": { "total": 500_000_000, "monthly": 5_000_000 },
            "favers": 20000,
            "maintainers": [{ "name": "Seldaek" }],
            "versions": {
                "2.0.0": {
                    "time": "2022-01-01T00:00:00+00:00",
                    "license": ["MIT"],
                    "homepage": "https://github.com/Seldaek/monolog",
                    "source": { "url": "https://github.com/Seldaek/monolog.git" },
                    "require": { "php": ">=8.1" }
                }
            }
        });
        let md = render(&pkg);
        assert!(md.contains("# monolog/monolog"));
        assert!(md.contains("**Latest:** 2.0.0 · **License:** MIT · **Type:** library"));
        assert!(md.contains("**Downloads:** 500,000,000 total · 5,000,000/month"));
        assert!(md.contains("**Repository:** https://github.com/Seldaek/monolog"));
        assert!(md.contains("## Requirements\n\n- php: >=8.1"));
    }
}
