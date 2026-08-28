// NVD handler: renders a CVE from the NVD REST API (CVSS, CWE, CPE, references).

use super::util::{build_result, format_iso_date, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt::Write;
use std::time::Duration;

pub struct NvdHandler;

static CVE_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)/vuln/detail/(CVE-\d{4}-\d+)").expect("static nvd path regex"));

fn cve_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("nvd.nist.gov") {
        return None;
    }
    CVE_PATH
        .captures(parsed.path())
        .map(|m| m[1].to_uppercase())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The English description, if present.
fn description_en(vuln: &Value) -> Option<String> {
    vuln.get("descriptions")?
        .as_array()?
        .iter()
        .find(|d| d.get("lang").and_then(Value::as_str) == Some("en"))
        .and_then(|d| str_field(d, "value"))
        .map(str::to_string)
}

fn metric<'a>(vuln: &'a Value, key: &str) -> Option<&'a Value> {
    vuln.get("metrics")?.get(key)?.as_array()?.first()
}

fn append_cvss_v3(md: &mut String, heading: &str, metric: &Value) {
    let Some(data) = metric.get("cvssData") else {
        return;
    };
    let score = data
        .get("baseScore")
        .map(|s| s.to_string())
        .unwrap_or_default();
    let severity = str_field(data, "baseSeverity").unwrap_or("");
    let _ = write!(
        md,
        "### {heading}\n\n- **Base Score:** {score} ({severity})\n"
    );
    if let Some(vector) = str_field(data, "vectorString") {
        let _ = writeln!(md, "- **Vector:** `{vector}`");
    }
    if let Some(e) = metric.get("exploitabilityScore") {
        let _ = writeln!(md, "- **Exploitability:** {e}");
    }
    if let Some(i) = metric.get("impactScore") {
        let _ = writeln!(md, "- **Impact:** {i}");
    }
    md.push('\n');
}

fn append_cvss(md: &mut String, vuln: &Value) {
    let v31 = metric(vuln, "cvssMetricV31");
    let v30 = metric(vuln, "cvssMetricV30");
    let v2 = metric(vuln, "cvssMetricV2");
    if v31.is_none() && v30.is_none() && v2.is_none() {
        return;
    }
    md.push_str("## CVSS Scores\n\n");
    if let Some(m) = v31 {
        append_cvss_v3(md, "CVSS 3.1", m);
    } else if let Some(m) = v30 {
        append_cvss_v3(md, "CVSS 3.0", m);
    }
    if let Some(m) = v2 {
        if let Some(data) = m.get("cvssData") {
            let score = data
                .get("baseScore")
                .map(|s| s.to_string())
                .unwrap_or_default();
            let _ = write!(md, "### CVSS 2.0\n\n- **Base Score:** {score}");
            if let Some(sev) = str_field(data, "severity") {
                let _ = write!(md, " ({sev})");
            }
            if let Some(vector) = str_field(data, "vectorString") {
                let _ = write!(md, "\n- **Vector:** `{vector}`");
            }
            md.push_str("\n\n");
        }
    }
}

fn append_weaknesses(md: &mut String, vuln: &Value) {
    let Some(weaknesses) = vuln.get("weaknesses").and_then(Value::as_array) else {
        return;
    };
    let mut cwes: Vec<&str> = Vec::new();
    for w in weaknesses {
        let Some(descs) = w.get("description").and_then(Value::as_array) else {
            continue;
        };
        for d in descs {
            if d.get("lang").and_then(Value::as_str) != Some("en") {
                continue;
            }
            if let Some(v) = str_field(d, "value") {
                if v != "NVD-CWE-Other" && v != "NVD-CWE-noinfo" {
                    cwes.push(v);
                }
            }
        }
    }
    if cwes.is_empty() {
        return;
    }
    md.push_str("## Weaknesses\n\n");
    for cwe in cwes {
        let _ = writeln!(md, "- {cwe}");
    }
    md.push('\n');
}

/// Distinct vulnerable CPE criteria from the configuration tree.
fn extract_cpes(vuln: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut cpes = Vec::new();
    let Some(configs) = vuln.get("configurations").and_then(Value::as_array) else {
        return cpes;
    };
    for config in configs {
        let Some(nodes) = config.get("nodes").and_then(Value::as_array) else {
            continue;
        };
        for node in nodes {
            let Some(matches) = node.get("cpeMatch").and_then(Value::as_array) else {
                continue;
            };
            for m in matches {
                if m.get("vulnerable").and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                if let Some(criteria) = str_field(m, "criteria") {
                    if seen.insert(criteria.to_string()) {
                        cpes.push(criteria.to_string());
                    }
                }
            }
        }
    }
    cpes
}

fn append_cpes(md: &mut String, vuln: &Value) {
    let cpes = extract_cpes(vuln);
    if cpes.is_empty() {
        return;
    }
    md.push_str("## Affected Products\n\n");
    for cpe in cpes.iter().take(20) {
        let _ = writeln!(md, "- `{cpe}`");
    }
    if cpes.len() > 20 {
        let _ = write!(md, "\n[…{} CPEs elided…]\n", cpes.len() - 20);
    }
    md.push('\n');
}

fn append_references(md: &mut String, vuln: &Value) {
    let Some(refs) = vuln.get("references").and_then(Value::as_array) else {
        return;
    };
    if refs.is_empty() {
        return;
    }
    md.push_str("## References\n\n");
    for r in refs.iter().take(15) {
        let Some(url) = str_field(r, "url") else {
            continue;
        };
        let tags = r
            .get("tags")
            .and_then(Value::as_array)
            .map(|t| t.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .filter(|t| !t.is_empty())
            .map(|t| format!(" ({})", t.join(", ")))
            .unwrap_or_default();
        let _ = writeln!(md, "- {url}{tags}");
    }
    if refs.len() > 15 {
        let _ = write!(md, "\n[…{} references elided…]\n", refs.len() - 15);
    }
}

fn render(vuln: &Value) -> String {
    let id = str_field(vuln, "id").unwrap_or("(CVE)");
    let mut md = format!("# {id}\n\n");
    if let Some(status) = str_field(vuln, "vulnStatus") {
        let _ = writeln!(md, "**Status:** {status}");
    }
    let published = str_field(vuln, "published")
        .map(format_iso_date)
        .unwrap_or_default();
    let modified = str_field(vuln, "lastModified")
        .map(format_iso_date)
        .unwrap_or_default();
    let _ = write!(
        md,
        "**Published:** {published} · **Modified:** {modified}\n\n"
    );
    if let Some(desc) = description_en(vuln) {
        let _ = write!(md, "## Description\n\n{desc}\n\n");
    }
    append_cvss(&mut md, vuln);
    append_weaknesses(&mut md, vuln);
    append_cpes(&mut md, vuln);
    append_references(&mut md, vuln);
    md
}

#[async_trait]
impl SpecialHandler for NvdHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let cve = cve_id(url)?;
        let api_url = format!("https://services.nvd.nist.gov/rest/json/cves/2.0?cveId={cve}");
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
        let vuln = data
            .get("vulnerabilities")
            .and_then(Value::as_array)?
            .first()?
            .get("cve")?
            .clone();
        let md = render(&vuln);
        Some(build_result(
            &md,
            url,
            "nvd",
            vec!["Fetched via NVD API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cve_id_requires_the_nvd_host_and_detail_path() {
        assert_eq!(
            cve_id("https://nvd.nist.gov/vuln/detail/CVE-2021-44228").as_deref(),
            Some("CVE-2021-44228")
        );
        assert!(cve_id("https://nvd.nist.gov/vuln/search").is_none());
        assert!(cve_id("https://example.com/vuln/detail/CVE-2021-44228").is_none());
    }

    #[test]
    fn render_lays_out_cvss_cwe_and_products() {
        let vuln = json!({
            "id": "CVE-2021-44228",
            "vulnStatus": "Analyzed",
            "published": "2021-12-10T10:15:00Z",
            "lastModified": "2023-04-03T20:15:00Z",
            "descriptions": [{ "lang": "en", "value": "Log4j JNDI RCE." }],
            "metrics": {
                "cvssMetricV31": [{
                    "cvssData": { "baseScore": 10.0, "baseSeverity": "CRITICAL", "vectorString": "AV:N/..." },
                    "exploitabilityScore": 3.9, "impactScore": 6.0
                }]
            },
            "weaknesses": [{ "description": [{ "lang": "en", "value": "CWE-502" }] }],
            "configurations": [{ "nodes": [{ "cpeMatch": [
                { "vulnerable": true, "criteria": "cpe:2.3:a:apache:log4j:*" }
            ] }] }],
            "references": [{ "url": "https://logging.apache.org", "tags": ["Vendor Advisory"] }]
        });
        let md = render(&vuln);
        assert!(md.contains("# CVE-2021-44228"));
        assert!(md.contains("**Status:** Analyzed"));
        assert!(md.contains("## Description\n\nLog4j JNDI RCE."));
        assert!(md.contains("### CVSS 3.1"));
        assert!(md.contains("- **Base Score:** 10.0 (CRITICAL)"));
        assert!(md.contains("- CWE-502"));
        assert!(md.contains("- `cpe:2.3:a:apache:log4j:*`"));
        assert!(md.contains("- https://logging.apache.org (Vendor Advisory)"));
    }

    #[test]
    fn cpe_extraction_dedupes_and_skips_non_vulnerable() {
        let vuln = json!({ "configurations": [{ "nodes": [{ "cpeMatch": [
            { "vulnerable": true, "criteria": "cpe:a" },
            { "vulnerable": true, "criteria": "cpe:a" },
            { "vulnerable": false, "criteria": "cpe:b" }
        ] }] }] });
        assert_eq!(extract_cpes(&vuln), vec!["cpe:a"]);
    }
}
