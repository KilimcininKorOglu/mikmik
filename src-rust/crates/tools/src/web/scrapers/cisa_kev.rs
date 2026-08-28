// CISA KEV handler: looks a CVE up in the Known Exploited Vulnerabilities feed.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct CisaKevHandler;

const KEV_FEED_URL: &str =
    "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json";

static CVE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)CVE-\d{4}-\d{4,7}").expect("static CVE regex"));

/// The CVE id addressed by a CISA KEV URL, or `None` when it is not one.
fn cve_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_lowercase();
    if !host.ends_with("cisa.gov") {
        return None;
    }
    if !parsed
        .path()
        .to_lowercase()
        .contains("known-exploited-vulnerabilities")
    {
        return None;
    }
    let haystack = format!("{}?{}", parsed.path(), parsed.query().unwrap_or(""));
    CVE_PATTERN
        .find(&haystack)
        .map(|m| m.as_str().to_uppercase())
}

fn field<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn render(entry: &Value) -> String {
    let id = field(entry, "cveID").unwrap_or("(CVE)");
    let mut md = format!("# {id}\n\n");
    if let Some(name) = field(entry, "vulnerabilityName") {
        let _ = writeln!(md, "{name}\n");
    }
    md.push_str("## Metadata\n\n");
    for (label, key) in [
        ("Vendor", "vendorProject"),
        ("Product", "product"),
        ("Date Added", "dateAdded"),
        ("Due Date", "dueDate"),
    ] {
        if let Some(value) = field(entry, key) {
            let _ = writeln!(md, "**{label}:** {value}");
        }
    }
    md.push('\n');
    if let Some(desc) = field(entry, "shortDescription") {
        let _ = write!(md, "## Description\n\n{desc}\n\n");
    }
    if let Some(action) = field(entry, "requiredAction") {
        let _ = write!(md, "## Required Action\n\n{action}\n\n");
    }
    md
}

#[async_trait]
impl SpecialHandler for CisaKevHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let cve = cve_id(url)?;
        let result = load_page(
            KEV_FEED_URL,
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
        let entry = data
            .get("vulnerabilities")
            .and_then(Value::as_array)?
            .iter()
            .find(|item| field(item, "cveID").map(str::to_uppercase) == Some(cve.clone()))?;

        let md = render(entry);
        Some(build_result(
            &md,
            url,
            "cisa-kev",
            vec!["Fetched via CISA KEV feed".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cve_id_requires_the_kev_path_and_a_cve() {
        assert_eq!(
            cve_id(
                "https://www.cisa.gov/known-exploited-vulnerabilities-catalog?cve=CVE-2021-44228"
            )
            .as_deref(),
            Some("CVE-2021-44228")
        );
        assert!(cve_id("https://www.cisa.gov/known-exploited-vulnerabilities-catalog").is_none());
        assert!(cve_id("https://example.com/CVE-2021-44228").is_none());
    }

    #[test]
    fn render_lays_out_the_entry() {
        let entry = json!({
            "cveID": "CVE-2021-44228",
            "vulnerabilityName": "Log4Shell",
            "vendorProject": "Apache",
            "product": "Log4j",
            "dateAdded": "2021-12-10",
            "shortDescription": "RCE via JNDI.",
            "requiredAction": "Patch immediately."
        });
        let md = render(&entry);
        assert!(md.contains("# CVE-2021-44228"));
        assert!(md.contains("Log4Shell"));
        assert!(md.contains("**Vendor:** Apache"));
        assert!(md.contains("## Description\n\nRCE via JNDI."));
        assert!(md.contains("## Required Action\n\nPatch immediately."));
    }
}
