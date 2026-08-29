// W3C handler: renders a Technical Report specification from the W3C API.

use super::util::{build_result, html_to_markdown, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct W3cHandler;

static YEAR: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d{4}$").expect("static w3c year regex"));
static DATED_VERSION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z]+-(.+)-\d{8}$").expect("static w3c version regex"));

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Recover the spec shortname from a `/TR/...` path.
fn extract_shortname(pathname: &str) -> Option<String> {
    let segments: Vec<&str> = pathname
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if segments.len() < 2 || segments[0] != "TR" {
        return None;
    }
    if segments.len() == 2 {
        let shortname = segments[1];
        if YEAR.is_match(shortname) {
            return None;
        }
        return Some(super::util::percent_decode(shortname));
    }
    if YEAR.is_match(segments[1]) {
        let caps = DATED_VERSION.captures(segments[2])?;
        return Some(super::util::percent_decode(&caps[1]));
    }
    None
}

/// Map a status label to its W3C maturity code.
fn normalize_status(status: &str) -> (Option<&'static str>, &str) {
    let lower = status.to_lowercase();
    let code = if lower.contains("working draft") {
        Some("WD")
    } else if lower.contains("candidate recommendation") {
        Some("CR")
    } else if lower.contains("proposed recommendation") {
        Some("PR")
    } else if lower.contains("recommendation") {
        Some("REC")
    } else {
        None
    };
    (code, status)
}

fn extract_editors(payload: &Value) -> Vec<String> {
    payload
        .get("_links")
        .and_then(|l| l.get("editors"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| str_field(entry, "title").map(str::to_string))
        .collect()
}

fn link_href<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get("_links")
        .and_then(|l| l.get(key))
        .and_then(|entry| str_field(entry, "href"))
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            headers: vec![("Accept".to_string(), "application/json".to_string())],
            ..Default::default()
        },
    )
    .await;
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}

fn append_status(md: &mut String, latest: &Value) {
    let Some(status) = str_field(latest, "status") else {
        return;
    };
    let (code, label) = normalize_status(status);
    match code {
        Some(code) => {
            let _ = writeln!(md, "**Status:** {code} ({label})");
        }
        None => {
            let _ = writeln!(md, "**Status:** {label}");
        }
    }
}

fn render(spec: &Value, latest: &Value, editors: &[String], shortname: &str) -> String {
    let title = str_field(spec, "title").unwrap_or(shortname);
    let mut md = format!("# {title}\n\n");
    let description = str_field(spec, "description").or_else(|| str_field(spec, "abstract"));
    if let Some(description) = description {
        let abstract_md = html_to_markdown(description);
        if !abstract_md.is_empty() {
            let _ = write!(md, "## Abstract\n\n{abstract_md}\n\n");
        }
    }
    md.push_str("## Metadata\n\n");
    let shortname_value = str_field(spec, "shortname").unwrap_or(shortname);
    let _ = writeln!(md, "**Shortname:** {shortname_value}");
    append_status(&mut md, latest);
    if !editors.is_empty() {
        let _ = writeln!(md, "**Editors:** {}", editors.join(", "));
    }
    let latest_url = str_field(latest, "uri")
        .or_else(|| str_field(latest, "shortlink"))
        .or_else(|| str_field(spec, "shortlink"));
    if let Some(latest_url) = latest_url {
        let _ = writeln!(md, "**Latest Version:** {latest_url}");
    }
    if let Some(history) = link_href(spec, "version-history") {
        let _ = writeln!(md, "**History:** {history}");
    }
    md
}

#[async_trait]
impl SpecialHandler for W3cHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host_str()?;
        if host != "www.w3.org" && host != "w3.org" {
            return None;
        }
        let shortname = extract_shortname(parsed.path())?;
        let encoded = super::util::percent_encode_component(&shortname);
        let spec_url = format!("https://api.w3.org/specifications/{encoded}");
        let latest_url = format!("https://api.w3.org/specifications/{encoded}/versions/latest");
        let (spec, latest) = tokio::join!(
            fetch_json(&spec_url, timeout),
            fetch_json(&latest_url, timeout)
        );
        let spec = spec?;
        let latest = latest?;

        let editors = match link_href(&latest, "editors") {
            Some(editors_url) => fetch_json(editors_url, timeout.min(Duration::from_secs(10)))
                .await
                .map(|p| extract_editors(&p))
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let md = render(&spec, &latest, &editors, &shortname);
        Some(build_result(
            &md,
            url,
            "w3c-api",
            vec!["Fetched via W3C API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shortname_from_bare_tr_path() {
        assert_eq!(
            extract_shortname("/TR/css-flexbox-1/"),
            Some("css-flexbox-1".to_string())
        );
        assert_eq!(extract_shortname("/TR/2020"), None);
        assert_eq!(extract_shortname("/other/x"), None);
    }

    #[test]
    fn shortname_from_dated_version_path() {
        assert_eq!(
            extract_shortname("/TR/2018/REC-css-color-3-20180619/"),
            Some("css-color-3".to_string())
        );
    }

    #[test]
    fn status_maps_to_code() {
        assert_eq!(normalize_status("Working Draft").0, Some("WD"));
        assert_eq!(normalize_status("Recommendation").0, Some("REC"));
        assert_eq!(normalize_status("Note").0, None);
    }

    #[test]
    fn render_lays_out_spec() {
        let spec = json!({
            "title": "CSS Flexible Box Layout Module Level 1",
            "shortname": "css-flexbox-1",
            "description": "<p>A <strong>flex</strong> layout.</p>",
            "_links": { "version-history": { "href": "https://x.test/history" } }
        });
        let latest = json!({
            "uri": "https://www.w3.org/TR/css-flexbox-1/",
            "status": "Candidate Recommendation"
        });
        let editors = vec!["Tab Atkins".to_string()];
        let md = render(&spec, &latest, &editors, "css-flexbox-1");
        assert!(md.contains("# CSS Flexible Box Layout Module Level 1"));
        assert!(md.contains("## Abstract\n\nA **flex** layout."));
        assert!(md.contains("**Shortname:** css-flexbox-1"));
        assert!(md.contains("**Status:** CR (Candidate Recommendation)"));
        assert!(md.contains("**Editors:** Tab Atkins"));
        assert!(md.contains("**Latest Version:** https://www.w3.org/TR/css-flexbox-1/"));
        assert!(md.contains("**History:** https://x.test/history"));
    }
}
