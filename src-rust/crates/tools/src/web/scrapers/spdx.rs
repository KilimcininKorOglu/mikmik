// SPDX handler: renders a license from the SPDX license JSON API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct SpdxHandler;

static LICENSE_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^/licenses/([^/]+?)(?:\.html)?/?$").expect("static spdx regex"));

fn parse_license_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "spdx.org" && host != "www.spdx.org" {
        return None;
    }
    let id = &LICENSE_PATH.captures(parsed.path())?[1];
    let decoded = super::util::percent_decode(id);
    (!decoded.is_empty()).then_some(decoded)
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn yes_no(v: &Value, key: &str) -> &'static str {
    match v.get(key).and_then(Value::as_bool) {
        Some(true) => "Yes",
        Some(false) => "No",
        None => "Unknown",
    }
}

/// Cross references, ordered by `order`, then any `seeAlso` URLs, deduplicated.
fn cross_references(license: &Value) -> Vec<String> {
    let mut ordered: Vec<(i64, String)> = license
        .get("crossRef")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .filter_map(|r| {
                    let url = str_field(r, "url")?;
                    let order = r.get("order").and_then(Value::as_i64).unwrap_or(0);
                    Some((order, url.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    ordered.sort_by_key(|(order, _)| *order);

    let mut combined: Vec<String> = ordered.into_iter().map(|(_, url)| url).collect();
    if let Some(see_also) = license.get("seeAlso").and_then(Value::as_array) {
        for url in see_also.iter().filter_map(Value::as_str) {
            combined.push(url.to_string());
        }
    }
    let mut seen = std::collections::HashSet::new();
    combined.retain(|url| seen.insert(url.clone()));
    combined
}

fn render(license: &Value, fallback_id: &str) -> String {
    let license_id = str_field(license, "licenseId").unwrap_or(fallback_id);
    let title = str_field(license, "name").unwrap_or(license_id);
    let mut md = format!("# {title}\n\n");
    let _ = writeln!(md, "**License ID:** `{license_id}`");
    let _ = writeln!(md, "**OSI Approved:** {}", yes_no(license, "isOsiApproved"));
    let _ = writeln!(md, "**FSF Libre:** {}", yes_no(license, "isFsfLibre"));

    if let Some(desc) =
        str_field(license, "licenseComments").or_else(|| str_field(license, "comment"))
    {
        let _ = write!(md, "\n## Description\n\n{desc}\n");
    }

    let refs = cross_references(license);
    if !refs.is_empty() {
        md.push_str("\n## Cross References\n\n");
        for r in refs {
            let _ = writeln!(md, "- {r}");
        }
    }

    if let Some(text) = str_field(license, "licenseText") {
        let _ = write!(md, "\n## License Text\n\n```\n{text}\n```\n");
    }
    md
}

#[async_trait]
impl SpecialHandler for SpdxHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let license_id = parse_license_id(url)?;
        let api_url = format!(
            "https://spdx.org/licenses/{}.json",
            super::util::percent_encode_component(&license_id)
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
        let license: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&license, &license_id);
        Some(build_result(
            &md,
            url,
            "spdx-api",
            vec!["Fetched via SPDX license API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_license_id_reads_slug_and_strips_html() {
        assert_eq!(
            parse_license_id("https://spdx.org/licenses/MIT.html"),
            Some("MIT".to_string())
        );
        assert_eq!(
            parse_license_id("https://spdx.org/licenses/Apache-2.0"),
            Some("Apache-2.0".to_string())
        );
        assert_eq!(parse_license_id("https://example.com/licenses/MIT"), None);
    }

    #[test]
    fn render_lays_out_flags_and_text() {
        let license = json!({
            "licenseId": "MIT",
            "name": "MIT License",
            "isOsiApproved": true,
            "isFsfLibre": true,
            "licenseText": "Permission is hereby granted...",
            "seeAlso": ["https://opensource.org/licenses/MIT"],
            "crossRef": [{ "url": "https://mit-license.org/", "order": 1 }]
        });
        let md = render(&license, "MIT");
        assert!(md.contains("# MIT License"));
        assert!(md.contains("**License ID:** `MIT`"));
        assert!(md.contains("**OSI Approved:** Yes"));
        assert!(md.contains("**FSF Libre:** Yes"));
        assert!(md.contains("- https://mit-license.org/"));
        assert!(md.contains("- https://opensource.org/licenses/MIT"));
        assert!(md.contains("## License Text\n\n```\nPermission is hereby granted...\n```"));
    }

    #[test]
    fn cross_references_dedupe_and_order() {
        let license = json!({
            "crossRef": [
                { "url": "https://b.example", "order": 2 },
                { "url": "https://a.example", "order": 1 }
            ],
            "seeAlso": ["https://a.example", "https://c.example"]
        });
        assert_eq!(
            cross_references(&license),
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string(),
                "https://c.example".to_string()
            ]
        );
    }
}
