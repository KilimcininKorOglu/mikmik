// Hex.pm handler: renders an Elixir package page from the Hex API.

use super::util::{
    build_result, format_iso_date, format_number, load_page, percent_decode, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct HexHandler;

fn package_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "hex.pm" && host != "www.hex.pm" {
        return None;
    }
    let rest = parsed.path().strip_prefix("/packages/")?;
    let name = rest.split('/').next().filter(|s| !s.is_empty())?;
    Some(percent_decode(name))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn latest_version(data: &Value) -> String {
    str_field(data, "latest_stable_version")
        .or_else(|| str_field(data, "latest_version"))
        .unwrap_or("unknown")
        .to_string()
}

fn append_links(md: &mut String, meta: &Value) {
    let Some(links) = meta.get("links").and_then(Value::as_object) else {
        return;
    };
    if links.is_empty() {
        return;
    }
    md.push_str("## Links\n\n");
    for (key, value) in links {
        if let Some(value) = value.as_str() {
            let _ = writeln!(md, "- **{key}:** {value}");
        }
    }
    md.push('\n');
}

fn append_releases(md: &mut String, data: &Value) {
    let Some(releases) = data.get("releases").and_then(Value::as_array) else {
        return;
    };
    if releases.is_empty() {
        return;
    }
    md.push_str("## Recent Releases\n\n");
    for release in releases.iter().take(10) {
        let version = str_field(release, "version").unwrap_or("?");
        let date = str_field(release, "inserted_at")
            .map(format_iso_date)
            .unwrap_or_default();
        let _ = writeln!(md, "- **{version}** ({date})");
    }
}

fn render(data: &Value) -> String {
    let name = str_field(data, "name").unwrap_or("(package)");
    let mut md = format!("# {name}\n\n");
    let meta = data.get("meta").cloned().unwrap_or(Value::Null);
    if let Some(desc) = str_field(&meta, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    let version = latest_version(data);
    let _ = write!(md, "**Latest:** {version}");
    if let Some(licenses) = meta.get("licenses").and_then(Value::as_array) {
        let ls: Vec<&str> = licenses.iter().filter_map(Value::as_str).collect();
        if !ls.is_empty() {
            let _ = write!(md, " · **License:** {}", ls.join(", "));
        }
    }
    md.push('\n');
    if let Some(all) = data
        .get("downloads")
        .and_then(|d| d.get("all"))
        .and_then(Value::as_u64)
    {
        let _ = write!(md, "**Total Downloads:** {}", format_number(all));
        if let Some(week) = data
            .get("downloads")
            .and_then(|d| d.get("week"))
            .and_then(Value::as_u64)
        {
            let _ = write!(md, " · **This Week:** {}", format_number(week));
        }
        md.push('\n');
    }
    md.push('\n');
    append_links(&mut md, &meta);
    append_releases(&mut md, data);
    md
}

#[async_trait]
impl SpecialHandler for HexHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let name = package_name(url)?;
        let api_url = format!("https://hex.pm/api/packages/{name}");
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
        let md = render(&data);
        Some(build_result(
            &md,
            url,
            "hex",
            vec!["Fetched via Hex.pm API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn package_name_parses_and_rejects_other_hosts() {
        assert_eq!(
            package_name("https://hex.pm/packages/phoenix").as_deref(),
            Some("phoenix")
        );
        assert!(package_name("https://example.com/packages/x").is_none());
    }

    #[test]
    fn render_lays_out_package_links_and_releases() {
        let data = json!({
            "name": "phoenix",
            "meta": {
                "description": "Web framework",
                "licenses": ["MIT"],
                "links": { "GitHub": "https://github.com/phoenixframework/phoenix" }
            },
            "downloads": { "all": 10_000_000, "week": 50_000 },
            "latest_stable_version": "1.7.0",
            "releases": [{ "version": "1.7.0", "inserted_at": "2024-01-01T00:00:00Z" }]
        });
        let md = render(&data);
        assert!(md.contains("# phoenix"));
        assert!(md.contains("**Latest:** 1.7.0 · **License:** MIT"));
        assert!(md.contains("**Total Downloads:** 10,000,000 · **This Week:** 50,000"));
        assert!(md.contains("- **GitHub:** https://github.com/phoenixframework/phoenix"));
        assert!(md.contains("- **1.7.0** (2024-01-01)"));
    }
}
