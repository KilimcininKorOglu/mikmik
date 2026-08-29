// VS Code Marketplace handler: renders an extension via the gallery extension
// query API.

use super::util::{build_result, format_number, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write;
use std::time::Duration;

pub struct VscodeMarketplaceHandler;

fn is_marketplace_host(host: &str) -> bool {
    host == "marketplace.visualstudio.com" || host == "www.marketplace.visualstudio.com"
}

/// The `publisher.extension` identifier from an `/items?itemName=` URL.
fn get_item_name(parsed: &url::Url) -> Option<String> {
    if !parsed.path().starts_with("/items") {
        return None;
    }
    let item = parsed
        .query_pairs()
        .find(|(k, _)| k == "itemName")
        .map(|(_, v)| v.into_owned())?;
    item.contains('.').then_some(item)
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn str_list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Map lower-cased statistic names to their values.
fn stat_map(extension: &Value) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    if let Some(stats) = extension.get("statistics").and_then(Value::as_array) {
        for stat in stats {
            let name = str_field(stat, "statisticName");
            let value = stat.get("value").and_then(Value::as_f64);
            if let (Some(name), Some(value)) = (name, value) {
                map.insert(name.trim().to_lowercase(), value);
            }
        }
    }
    map
}

/// `4.50` → `4.5`, `4.00` → `4`, kept to two decimals otherwise.
fn trim_rating(value: f64) -> String {
    let text = format!("{value:.2}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

fn format_rating(stats: &HashMap<String, f64>) -> Option<String> {
    let average = stats.get("averagerating");
    let count = stats.get("ratingcount").map(|c| *c as u64);
    match (average, count) {
        (Some(avg), Some(count)) => Some(format!(
            "{} ({} ratings)",
            trim_rating(*avg),
            format_number(count)
        )),
        (Some(avg), None) => Some(trim_rating(*avg)),
        (None, Some(count)) => Some(format!("{} ratings", format_number(count))),
        (None, None) => None,
    }
}

/// First property whose http value belongs to a source/repository key.
fn extract_repo_link(properties: &Value) -> Option<String> {
    let props = properties.as_array()?;
    let http_pairs: Vec<(String, &str)> = props
        .iter()
        .filter_map(|p| {
            let key = str_field(p, "key")?.trim().to_lowercase();
            let value = str_field(p, "value")?.trim();
            value.starts_with("http").then_some((key, value))
        })
        .collect();
    for (key, value) in &http_pairs {
        if key.contains("links.source") || key.contains("repository") {
            return Some(value.to_string());
        }
    }
    for (key, value) in &http_pairs {
        if key == "source" || key.ends_with(".source") {
            return Some(value.to_string());
        }
    }
    None
}

fn publisher_label(extension: &Value, publisher_from_url: &str) -> Option<String> {
    let publisher = extension.get("publisher");
    let name = publisher
        .and_then(|p| str_field(p, "publisherName"))
        .unwrap_or(publisher_from_url);
    let display = publisher.and_then(|p| str_field(p, "displayName"));
    match display {
        Some(display) if display != name => Some(format!("{display} ({name})")),
        Some(display) => Some(display.to_string()),
        None if !name.is_empty() => Some(name.to_string()),
        None => None,
    }
}

fn render(extension: &Value, item_name: &str) -> String {
    let (publisher_from_url, extension_from_url) =
        item_name.split_once('.').unwrap_or((item_name, ""));
    let extension_name = str_field(extension, "extensionName").unwrap_or(extension_from_url);
    let display_name = str_field(extension, "displayName")
        .or(Some(extension_name).filter(|s| !s.is_empty()))
        .unwrap_or(item_name);
    let description =
        str_field(extension, "shortDescription").or_else(|| str_field(extension, "description"));

    let first_version = extension
        .get("versions")
        .and_then(Value::as_array)
        .and_then(|v| v.first());
    let version = first_version.and_then(|v| str_field(v, "version"));
    let stats = stat_map(extension);
    let installs = stats
        .get("install")
        .or_else(|| stats.get("installs"))
        .map(|v| *v as u64);

    let publisher_name = extension
        .get("publisher")
        .and_then(|p| str_field(p, "publisherName"))
        .unwrap_or(publisher_from_url);
    let identifier = if !publisher_name.is_empty() && !extension_name.is_empty() {
        format!("{publisher_name}.{extension_name}")
    } else {
        item_name.to_string()
    };

    let mut md = format!("# {display_name}\n\n");
    if let Some(desc) = description {
        let _ = write!(md, "{desc}\n\n");
    }
    let _ = writeln!(md, "**Identifier:** {identifier}");
    if let Some(label) = publisher_label(extension, publisher_from_url) {
        let _ = writeln!(md, "**Publisher:** {label}");
    }
    if let Some(version) = version {
        let _ = writeln!(md, "**Version:** {version}");
    }
    if let Some(installs) = installs {
        let _ = writeln!(md, "**Installs:** {}", format_number(installs));
    }
    if let Some(rating) = format_rating(&stats) {
        let _ = writeln!(md, "**Rating:** {rating}");
    }
    let categories = str_list(extension, "categories");
    if !categories.is_empty() {
        let _ = writeln!(md, "**Categories:** {}", categories.join(", "));
    }
    let tags = str_list(extension, "tags");
    if !tags.is_empty() {
        let _ = writeln!(md, "**Tags:** {}", tags.join(", "));
    }
    let repo = first_version
        .map(|v| v.get("properties").cloned().unwrap_or(Value::Null))
        .and_then(|p| extract_repo_link(&p))
        .or_else(|| extension.get("properties").and_then(extract_repo_link));
    if let Some(repo) = repo {
        let _ = writeln!(md, "**Repository:** {repo}");
    }
    md
}

#[async_trait]
impl SpecialHandler for VscodeMarketplaceHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        if !is_marketplace_host(parsed.host_str()?) {
            return None;
        }
        let item_name = get_item_name(&parsed)?;

        let payload = serde_json::json!({
            "filters": [{ "criteria": [{ "filterType": 7, "value": item_name }] }],
            "flags": 950
        })
        .to_string();
        let result = load_page(
            "https://marketplace.visualstudio.com/_apis/public/gallery/extensionquery",
            LoadOptions {
                timeout,
                method: reqwest::Method::POST,
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    (
                        "Accept".to_string(),
                        "application/json;api-version=7.2-preview.1".to_string(),
                    ),
                ],
                body: Some(payload),
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let extension = data
            .get("results")?
            .as_array()?
            .first()?
            .get("extensions")?
            .as_array()?
            .first()?;

        let md = render(extension, &item_name);
        Some(build_result(
            &md,
            url,
            "vscode-marketplace",
            vec!["Fetched via VS Code Marketplace API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item_of(url: &str) -> Option<String> {
        get_item_name(&url::Url::parse(url).expect("url"))
    }

    #[test]
    fn item_name_requires_a_dotted_identifier() {
        assert_eq!(
            item_of("https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer"),
            Some("rust-lang.rust-analyzer".to_string())
        );
        assert_eq!(
            item_of("https://marketplace.visualstudio.com/items?itemName=single"),
            None
        );
        assert_eq!(
            item_of("https://marketplace.visualstudio.com/search?term=x"),
            None
        );
    }

    #[test]
    fn rating_trims_trailing_zeros() {
        assert_eq!(trim_rating(4.50), "4.5");
        assert_eq!(trim_rating(4.00), "4");
        assert_eq!(trim_rating(4.25), "4.25");
    }

    #[test]
    fn render_lays_out_extension() {
        let extension = json!({
            "extensionName": "rust-analyzer",
            "displayName": "rust-analyzer",
            "shortDescription": "Rust support",
            "publisher": { "publisherName": "rust-lang", "displayName": "The Rust Programming Language" },
            "versions": [{
                "version": "0.4.0",
                "properties": [{ "key": "Microsoft.VisualStudio.Services.Links.Source", "value": "https://github.com/rust-lang/rust-analyzer" }]
            }],
            "statistics": [
                { "statisticName": "install", "value": 5000000 },
                { "statisticName": "averagerating", "value": 4.5 },
                { "statisticName": "ratingcount", "value": 320 }
            ],
            "categories": ["Programming Languages"]
        });
        let md = render(&extension, "rust-lang.rust-analyzer");
        assert!(md.contains("# rust-analyzer"));
        assert!(md.contains("**Identifier:** rust-lang.rust-analyzer"));
        assert!(md.contains("**Publisher:** The Rust Programming Language (rust-lang)"));
        assert!(md.contains("**Version:** 0.4.0"));
        assert!(md.contains("**Installs:** 5,000,000"));
        assert!(md.contains("**Rating:** 4.5 (320 ratings)"));
        assert!(md.contains("**Repository:** https://github.com/rust-lang/rust-analyzer"));
    }
}
