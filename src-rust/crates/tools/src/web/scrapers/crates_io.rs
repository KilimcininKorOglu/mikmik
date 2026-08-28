// crates.io handler: renders a crate page from the crates.io API.

use super::util::{
    build_result, format_iso_date, format_number, load_page, percent_decode, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct CratesIoHandler;

/// crates.io rejects requests without a descriptive User-Agent.
const CRATES_UA: &str = "mikmik (github.com/KilimcininKorOglu/mikmik)";

fn crate_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "crates.io" && host != "www.crates.io" {
        return None;
    }
    let rest = parsed.path().strip_prefix("/crates/")?;
    let name = rest.split('/').next().filter(|s| !s.is_empty())?;
    Some(percent_decode(name))
}

async fn fetch(url: &str, timeout: Duration) -> super::util::LoadPageResult {
    load_page(
        url,
        LoadOptions {
            timeout,
            headers: vec![("User-Agent".to_string(), CRATES_UA.to_string())],
            ..Default::default()
        },
    )
    .await
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn render_markdown(krate: &Value, versions: &[Value]) -> String {
    let name = str_field(krate, "name").unwrap_or("(crate)");
    let mut md = format!("# {name}\n\n");
    if let Some(desc) = str_field(krate, "description") {
        let _ = writeln!(md, "{desc}\n");
    }
    let latest = versions.first();
    let max_version = str_field(krate, "max_version").unwrap_or("unknown");
    let _ = write!(md, "**Latest:** {max_version}");
    if let Some(license) = latest.and_then(|v| str_field(v, "license")) {
        let _ = write!(md, " · **License:** {license}");
    }
    if let Some(msrv) = latest.and_then(|v| str_field(v, "rust_version")) {
        let _ = write!(md, " · **MSRV:** {msrv}");
    }
    md.push('\n');
    let total = krate.get("downloads").and_then(Value::as_u64).unwrap_or(0);
    let recent = krate
        .get("recent_downloads")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let _ = writeln!(
        md,
        "**Downloads:** {} total · {} recent\n",
        format_number(total),
        format_number(recent)
    );
    append_links(&mut md, krate);
    append_versions(&mut md, versions);
    md
}

fn append_links(md: &mut String, krate: &Value) {
    let repo = str_field(krate, "repository");
    if let Some(repo) = repo {
        let _ = writeln!(md, "**Repository:** {repo}");
    }
    if let Some(home) = str_field(krate, "homepage").filter(|h| Some(*h) != repo) {
        let _ = writeln!(md, "**Homepage:** {home}");
    }
    if let Some(docs) = str_field(krate, "documentation") {
        let _ = writeln!(md, "**Docs:** {docs}");
    }
    if let Some(keywords) = krate.get("keywords").and_then(Value::as_array) {
        let ks: Vec<&str> = keywords.iter().filter_map(Value::as_str).collect();
        if !ks.is_empty() {
            let _ = writeln!(md, "**Keywords:** {}", ks.join(", "));
        }
    }
    if let Some(categories) = krate.get("categories").and_then(Value::as_array) {
        let cs: Vec<&str> = categories.iter().filter_map(Value::as_str).collect();
        if !cs.is_empty() {
            let _ = writeln!(md, "**Categories:** {}", cs.join(", "));
        }
    }
}

fn append_versions(md: &mut String, versions: &[Value]) {
    if versions.is_empty() {
        return;
    }
    md.push_str("\n## Recent Versions\n\n");
    for ver in versions.iter().take(5) {
        let num = str_field(ver, "num").unwrap_or("?");
        let date = str_field(ver, "created_at")
            .map(format_iso_date)
            .unwrap_or_default();
        let downloads = ver.get("downloads").and_then(Value::as_u64).unwrap_or(0);
        let _ = writeln!(
            md,
            "- **{num}** ({date}) - {} downloads",
            format_number(downloads)
        );
    }
}

#[async_trait]
impl SpecialHandler for CratesIoHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let name = crate_name(url)?;
        let api_url = format!("https://crates.io/api/v1/crates/{name}");
        let result = fetch(&api_url, timeout).await;
        if !result.ok {
            return None;
        }
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let krate = data.get("crate")?.clone();
        let versions: Vec<Value> = data
            .get("versions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let md = render_markdown(&krate, &versions);
        Some(build_result(
            &md,
            url,
            "crates.io",
            vec!["Fetched via crates.io API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn crate_name_parses_and_rejects_other_hosts() {
        assert_eq!(
            crate_name("https://crates.io/crates/serde").as_deref(),
            Some("serde")
        );
        assert_eq!(
            crate_name("https://crates.io/crates/tokio/1.0.0").as_deref(),
            Some("tokio")
        );
        assert!(crate_name("https://example.com/crates/x").is_none());
    }

    #[test]
    fn markdown_renders_crate_and_versions() {
        let krate = json!({
            "name": "serde",
            "description": "Serialization framework",
            "max_version": "1.0.0",
            "downloads": 1_000_000,
            "recent_downloads": 50_000,
            "repository": "https://github.com/serde-rs/serde",
            "keywords": ["serde", "serialization"]
        });
        let versions = vec![json!({
            "num": "1.0.0", "downloads": 500_000, "created_at": "2024-01-01T00:00:00Z",
            "license": "MIT OR Apache-2.0", "rust_version": "1.56"
        })];
        let md = render_markdown(&krate, &versions);
        assert!(md.contains("# serde"));
        assert!(md.contains("**Latest:** 1.0.0 · **License:** MIT OR Apache-2.0 · **MSRV:** 1.56"));
        assert!(md.contains("**Downloads:** 1,000,000 total · 50,000 recent"));
        assert!(md.contains("**Repository:** https://github.com/serde-rs/serde"));
        assert!(md.contains("- **1.0.0** (2024-01-01) - 500,000 downloads"));
    }
}
