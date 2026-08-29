// JetBrains Marketplace handler: renders a plugin from the plugins.jetbrains.com API.

use super::util::{
    build_result, format_number, html_to_markdown, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct JetBrainsMarketplaceHandler;

static PLUGIN_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/plugin/(\d+)(?:-[^/]+)?(?:/|$)").expect("static jetbrains plugin regex")
});

fn parse_plugin_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "plugins.jetbrains.com" {
        return None;
    }
    let caps = PLUGIN_PATH.captures(parsed.path())?;
    Some(caps[1].to_string())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Rating value and vote count, tolerating a scalar or nested-object shape.
fn extract_rating(plugin: &Value) -> (Option<f64>, Option<u64>) {
    let rating_count = plugin.get("ratingCount").and_then(Value::as_u64);
    match plugin.get("rating") {
        Some(Value::Number(n)) => (n.as_f64(), rating_count),
        Some(obj @ Value::Object(_)) => {
            let value = ["rating", "value", "score"]
                .iter()
                .find_map(|k| obj.get(*k).and_then(Value::as_f64));
            let votes = ["votes", "totalVotes", "count"]
                .iter()
                .find_map(|k| obj.get(*k).and_then(Value::as_u64))
                .or(rating_count);
            (value, votes)
        }
        _ => (None, rating_count),
    }
}

/// Compact build-range string from the update's since/until fields.
fn format_build_compatibility(update: &Value) -> Option<String> {
    if let Some(su) = str_field(update, "sinceUntil") {
        return Some(su.to_string());
    }
    let since = str_field(update, "since");
    let until = str_field(update, "until");
    match (since, until) {
        (Some(since), Some(until)) => Some(format!("{since} - {until}")),
        (Some(since), None) => Some(format!("{since}+")),
        _ => None,
    }
}

fn append_header(md: &mut String, plugin: &Value, plugin_id: &str) {
    let name = str_field(plugin, "name").unwrap_or(plugin_id);
    let _ = write!(md, "# {name}\n\n");
    let source = str_field(plugin, "description").or_else(|| str_field(plugin, "preview"));
    if let Some(source) = source {
        let text = html_to_markdown(source);
        if !text.is_empty() {
            let _ = write!(md, "{text}\n\n");
        }
    }
    let _ = writeln!(md, "**Plugin ID:** {plugin_id}");
    if let Some(vendor) = plugin
        .get("vendor")
        .and_then(|v| str_field(v, "name").or_else(|| str_field(v, "publicName")))
    {
        let _ = writeln!(md, "**Vendor:** {vendor}");
    }
    if let Some(downloads) = plugin.get("downloads").and_then(Value::as_u64) {
        let _ = writeln!(md, "**Downloads:** {}", format_number(downloads));
    }
    let (value, votes) = extract_rating(plugin);
    if let Some(value) = value {
        let _ = write!(md, "**Rating:** {value:.2}");
        if let Some(votes) = votes {
            let _ = write!(md, " ({} votes)", format_number(votes));
        }
        md.push('\n');
    }
    let tags: Vec<&str> = plugin
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| str_field(t, "name"))
        .collect();
    if !tags.is_empty() {
        let _ = writeln!(md, "**Tags:** {}", tags.join(", "));
    }
}

fn append_release(md: &mut String, update: &Value) {
    md.push_str("\n## Latest Release\n\n");
    if let Some(version) = str_field(update, "version") {
        let _ = writeln!(md, "**Version:** {version}");
    }
    if let Some(channel) = str_field(update, "channel") {
        let _ = writeln!(md, "**Channel:** {channel}");
    }
    if let Some(build) = format_build_compatibility(update) {
        let _ = writeln!(md, "**Build Compatibility:** {build}");
    }
    if let Some(downloads) = update.get("downloads").and_then(Value::as_u64) {
        let _ = writeln!(md, "**Release Downloads:** {}", format_number(downloads));
    }
}

fn append_compatibility(md: &mut String, update: &Value) {
    let Some(map) = update.get("compatibleVersions").and_then(Value::as_object) else {
        return;
    };
    let mut entries: Vec<(&String, &str)> = map
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s)))
        .collect();
    if entries.is_empty() {
        return;
    }
    entries.sort_by(|a, b| a.0.cmp(b.0));
    md.push_str("\n## IDE Compatibility\n\n");
    for (product, version) in entries {
        let _ = writeln!(md, "- {product}: {version}");
    }
}

fn render(plugin: &Value, update: Option<&Value>, plugin_id: &str) -> String {
    let mut md = String::new();
    append_header(&mut md, plugin, plugin_id);
    if let Some(update) = update {
        append_release(&mut md, update);
        append_compatibility(&mut md, update);
    }
    md
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

#[async_trait]
impl SpecialHandler for JetBrainsMarketplaceHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let plugin_id = parse_plugin_id(url)?;
        let plugin_url = format!("https://plugins.jetbrains.com/api/plugins/{plugin_id}");
        let updates_url =
            format!("https://plugins.jetbrains.com/api/plugins/{plugin_id}/updates?size=1");
        let (plugin, updates) = tokio::join!(
            fetch_json(&plugin_url, timeout),
            fetch_json(&updates_url, timeout)
        );
        let plugin = plugin?;
        str_field(&plugin, "name")?;
        let updates = updates?;
        let update = updates.as_array().and_then(|a| a.first());
        let md = render(&plugin, update, &plugin_id);
        Some(build_result(
            &md,
            url,
            "jetbrains-marketplace",
            vec!["Fetched via JetBrains Marketplace API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_plugin_id_reads_numeric_segment() {
        assert_eq!(
            parse_plugin_id("https://plugins.jetbrains.com/plugin/6954-kotlin"),
            Some("6954".to_string())
        );
        assert_eq!(parse_plugin_id("https://example.com/plugin/1"), None);
    }

    #[test]
    fn rating_reads_object_shape() {
        let plugin = json!({ "rating": { "value": 4.5, "votes": 320 } });
        assert_eq!(extract_rating(&plugin), (Some(4.5), Some(320)));
        let scalar = json!({ "rating": 4.0, "ratingCount": 12 });
        assert_eq!(extract_rating(&scalar), (Some(4.0), Some(12)));
    }

    #[test]
    fn render_lays_out_plugin_and_release() {
        let plugin = json!({
            "name": "Kotlin",
            "description": "<p>The <strong>Kotlin</strong> plugin.</p>",
            "vendor": { "name": "JetBrains" },
            "downloads": 5000000,
            "rating": { "value": 4.75, "votes": 1200 },
            "tags": [{ "name": "Languages" }]
        });
        let update = json!({
            "version": "241.1",
            "channel": "stable",
            "since": "241.0",
            "until": "241.9999",
            "downloads": 12345,
            "compatibleVersions": { "IDEA_U": "2024.1", "IDEA_C": "2024.1" }
        });
        let md = render(&plugin, Some(&update), "6954");
        assert!(md.contains("# Kotlin"));
        assert!(md.contains("The **Kotlin** plugin."));
        assert!(md.contains("**Plugin ID:** 6954"));
        assert!(md.contains("**Vendor:** JetBrains"));
        assert!(md.contains("**Downloads:** 5,000,000"));
        assert!(md.contains("**Rating:** 4.75 (1,200 votes)"));
        assert!(md.contains("**Tags:** Languages"));
        assert!(md.contains("**Version:** 241.1"));
        assert!(md.contains("**Build Compatibility:** 241.0 - 241.9999"));
        assert!(md.contains("- IDEA_C: 2024.1\n- IDEA_U: 2024.1"));
    }
}
