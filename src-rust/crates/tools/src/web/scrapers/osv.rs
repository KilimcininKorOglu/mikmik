// OSV handler: renders an osv.dev vulnerability from the OSV API.

use super::util::{build_result, format_iso_date, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct OsvHandler;

static VULN_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/vulnerability/([A-Za-z0-9-]+)$").expect("static osv path regex"));

fn vuln_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "osv.dev" {
        return None;
    }
    VULN_PATH.captures(parsed.path()).map(|m| m[1].to_string())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Every severity entry, from the top level and per-affected package.
fn severities(vuln: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |list: &Value| {
        if let Some(arr) = list.as_array() {
            for s in arr {
                if let (Some(t), Some(score)) = (str_field(s, "type"), str_field(s, "score")) {
                    out.push(format!("{t}: {score}"));
                }
            }
        }
    };
    if let Some(list) = vuln.get("severity") {
        push(list);
    }
    if let Some(affected) = vuln.get("affected").and_then(Value::as_array) {
        for a in affected {
            if let Some(list) = a.get("severity") {
                push(list);
            }
        }
    }
    out
}

fn append_metadata(md: &mut String, vuln: &Value) {
    md.push_str("## Metadata\n\n");
    if let Some(aliases) = vuln.get("aliases").and_then(Value::as_array) {
        let a: Vec<&str> = aliases.iter().filter_map(Value::as_str).collect();
        if !a.is_empty() {
            let _ = writeln!(md, "**Aliases:** {}", a.join(", "));
        }
    }
    for (label, key) in [
        ("Published", "published"),
        ("Modified", "modified"),
        ("Withdrawn", "withdrawn"),
    ] {
        if let Some(value) = str_field(vuln, key) {
            let _ = writeln!(md, "**{label}:** {}", format_iso_date(value));
        }
    }
    let sev = severities(vuln);
    if !sev.is_empty() {
        let _ = writeln!(md, "**Severity:** {}", sev.join(", "));
    }
    md.push('\n');
}

fn append_affected(md: &mut String, vuln: &Value) {
    let Some(affected) = vuln.get("affected").and_then(Value::as_array) else {
        return;
    };
    if affected.is_empty() {
        return;
    }
    md.push_str("## Affected Packages\n\n");
    for a in affected {
        let Some(pkg) = a.get("package") else {
            continue;
        };
        let (Some(eco), Some(name)) = (str_field(pkg, "ecosystem"), str_field(pkg, "name")) else {
            continue;
        };
        let _ = write!(md, "### {eco}: {name}\n\n");
        append_ranges(md, a);
        append_versions(md, a);
        md.push('\n');
    }
}

fn append_ranges(md: &mut String, affected: &Value) {
    let Some(ranges) = affected.get("ranges").and_then(Value::as_array) else {
        return;
    };
    for range in ranges {
        let Some(events) = range.get("events").and_then(Value::as_array) else {
            continue;
        };
        let mut parts: Vec<String> = Vec::new();
        for event in events {
            for key in ["introduced", "fixed", "last_affected", "limit"] {
                if let Some(v) = str_field(event, key) {
                    parts.push(format!("{key}: {v}"));
                }
            }
        }
        if !parts.is_empty() {
            let rtype = str_field(range, "type").unwrap_or("RANGE");
            let _ = writeln!(md, "- **{rtype}:** {}", parts.join(" → "));
        }
    }
}

fn append_versions(md: &mut String, affected: &Value) {
    let Some(versions) = affected.get("versions").and_then(Value::as_array) else {
        return;
    };
    let vs: Vec<&str> = versions.iter().filter_map(Value::as_str).collect();
    if vs.is_empty() {
        return;
    }
    let shown = if vs.len() > 10 {
        format!("{}… ({} total)", vs[..10].join(", "), vs.len())
    } else {
        vs.join(", ")
    };
    let _ = writeln!(md, "- **Versions:** {shown}");
}

fn append_references(md: &mut String, vuln: &Value) {
    let Some(refs) = vuln.get("references").and_then(Value::as_array) else {
        return;
    };
    if refs.is_empty() {
        return;
    }
    md.push_str("## References\n\n");
    for r in refs {
        if let Some(url) = str_field(r, "url") {
            let rtype = str_field(r, "type").unwrap_or("WEB");
            let _ = writeln!(md, "- [{rtype}]({url})");
        }
    }
    md.push('\n');
}

fn render(vuln: &Value) -> String {
    let id = str_field(vuln, "id").unwrap_or("(vuln)");
    let mut md = format!("# {id}\n\n");
    if let Some(summary) = str_field(vuln, "summary") {
        let _ = write!(md, "{summary}\n\n");
    }
    append_metadata(&mut md, vuln);
    if let Some(details) = str_field(vuln, "details") {
        let _ = write!(md, "## Details\n\n{details}\n\n");
    }
    append_affected(&mut md, vuln);
    append_references(&mut md, vuln);
    md
}

#[async_trait]
impl SpecialHandler for OsvHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let id = vuln_id(url)?;
        let api_url = format!("https://api.osv.dev/v1/vulns/{id}");
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
        let vuln: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&vuln);
        Some(build_result(
            &md,
            url,
            "osv",
            vec!["Fetched via OSV API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vuln_id_parses_and_rejects_other_hosts() {
        assert_eq!(
            vuln_id("https://osv.dev/vulnerability/GHSA-jfh8-c2jp-5v3q").as_deref(),
            Some("GHSA-jfh8-c2jp-5v3q")
        );
        assert!(vuln_id("https://osv.dev/list").is_none());
        assert!(vuln_id("https://example.com/vulnerability/x").is_none());
    }

    #[test]
    fn render_lays_out_severity_affected_and_references() {
        let vuln = json!({
            "id": "GHSA-xxxx",
            "summary": "A flaw",
            "aliases": ["CVE-2024-0001"],
            "published": "2024-01-01T00:00:00Z",
            "severity": [{ "type": "CVSS_V3", "score": "9.8" }],
            "details": "Long details.",
            "affected": [{
                "package": { "ecosystem": "npm", "name": "left-pad" },
                "ranges": [{ "type": "SEMVER", "events": [{ "introduced": "0" }, { "fixed": "1.3.1" }] }],
                "versions": ["1.0.0", "1.3.0"]
            }],
            "references": [{ "type": "WEB", "url": "https://example.com/adv" }]
        });
        let md = render(&vuln);
        assert!(md.contains("# GHSA-xxxx"));
        assert!(md.contains("**Aliases:** CVE-2024-0001"));
        assert!(md.contains("**Severity:** CVSS_V3: 9.8"));
        assert!(md.contains("### npm: left-pad"));
        assert!(md.contains("- **SEMVER:** introduced: 0 → fixed: 1.3.1"));
        assert!(md.contains("- **Versions:** 1.0.0, 1.3.0"));
        assert!(md.contains("- [WEB](https://example.com/adv)"));
    }
}
