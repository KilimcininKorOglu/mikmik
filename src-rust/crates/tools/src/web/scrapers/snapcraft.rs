// Snapcraft handler: renders a snap from the Snap Store info API.

use super::util::{build_result, format_number, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::time::Duration;

pub struct SnapcraftHandler;

static INSTALL_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/install/([^/]+)/?$").expect("static snapcraft install regex"));
static DIRECT_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/([^/]+)/?$").expect("static snapcraft direct regex"));

fn parse_snap_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "snapcraft.io" && host != "www.snapcraft.io" {
        return None;
    }
    let path = parsed.path();
    let raw = INSTALL_PATH
        .captures(path)
        .or_else(|| DIRECT_PATH.captures(path))?;
    Some(super::util::percent_decode(&raw[1]))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Publisher display name, appending `(@username)` when it differs.
fn format_publisher(publisher: &Value) -> Option<String> {
    let display = str_field(publisher, "display-name")
        .or_else(|| str_field(publisher, "username"))
        .or_else(|| str_field(publisher, "id"))?;
    let username = str_field(publisher, "username");
    match username {
        Some(u) if u != display => Some(format!("{display} (@{u})")),
        _ => Some(display.to_string()),
    }
}

/// Build a `track/risk[/branch]` channel name from a channel object.
fn format_channel_name(channel: &Value) -> Option<String> {
    if let Some(name) = str_field(channel, "name") {
        if name.contains('/') {
            return Some(name.to_string());
        }
    }
    if let (Some(track), Some(risk)) = (str_field(channel, "track"), str_field(channel, "risk")) {
        let branch = str_field(channel, "branch")
            .map(|b| format!("/{b}"))
            .unwrap_or_default();
        return Some(format!("{track}/{risk}{branch}"));
    }
    str_field(channel, "name").map(str::to_string)
}

/// First stable version in the channel map, else the first with a version.
fn pick_version_from_channels(entries: &[Value]) -> Option<String> {
    let stable = entries.iter().find(|e| {
        e.get("channel").and_then(|c| str_field(c, "risk")) == Some("stable")
            && str_field(e, "version").is_some()
    });
    stable
        .or_else(|| entries.iter().find(|e| str_field(e, "version").is_some()))
        .and_then(|e| str_field(e, "version"))
        .map(str::to_string)
}

fn extract_downloads(snap_info: &Value, data: &Value) -> Option<u64> {
    for source in [snap_info, data] {
        for key in ["downloads", "download"] {
            if let Some(n) = source.get(key).and_then(Value::as_u64) {
                return Some(n);
            }
        }
    }
    None
}

#[derive(Default)]
struct ChannelInfo {
    version: Option<String>,
    architectures: BTreeSet<String>,
}

/// Collapse the flat channel map into one row per channel name.
fn collect_channels(channel_map: &[Value]) -> BTreeMap<String, ChannelInfo> {
    let mut channels: BTreeMap<String, ChannelInfo> = BTreeMap::new();
    for entry in channel_map {
        let channel = entry.get("channel").cloned().unwrap_or(Value::Null);
        let Some(name) = format_channel_name(&channel) else {
            continue;
        };
        let info = channels.entry(name).or_default();
        if info.version.is_none() {
            info.version = str_field(entry, "version").map(str::to_string);
        }
        if let Some(arch) = str_field(&channel, "architecture") {
            info.architectures.insert(arch.to_string());
        }
    }
    channels
}

fn append_channels(md: &mut String, channels: &BTreeMap<String, ChannelInfo>) {
    if channels.is_empty() {
        return;
    }
    md.push_str("## Channels\n\n");
    for (name, info) in channels {
        let version = info
            .version
            .as_ref()
            .map(|v| format!(": {v}"))
            .unwrap_or_default();
        let arches = if info.architectures.is_empty() {
            String::new()
        } else {
            let list: Vec<&str> = info.architectures.iter().map(String::as_str).collect();
            format!(" ({})", list.join(", "))
        };
        let _ = writeln!(md, "- {name}{version}{arches}");
    }
    md.push('\n');
}

fn render(data: &Value, snap_name: &str) -> String {
    let snap_info = data.get("snap").cloned().unwrap_or_else(|| data.clone());
    let name = str_field(&snap_info, "title")
        .or_else(|| str_field(&snap_info, "name"))
        .or_else(|| str_field(data, "name"))
        .unwrap_or(snap_name);
    let summary = str_field(&snap_info, "summary").or_else(|| str_field(data, "summary"));

    let channel_map = data
        .get("channel-map")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let version = str_field(&snap_info, "version")
        .or_else(|| str_field(data, "version"))
        .map(str::to_string)
        .or_else(|| pick_version_from_channels(&channel_map));

    let mut md = format!("# {name}\n\n");
    if let Some(summary) = summary {
        let _ = write!(md, "{summary}\n\n");
    }
    let _ = write!(
        md,
        "**Version:** {}",
        version.as_deref().unwrap_or("unknown")
    );
    if let Some(confinement) =
        str_field(&snap_info, "confinement").or_else(|| str_field(data, "confinement"))
    {
        let _ = write!(md, " · **Confinement:** {confinement}");
    }
    if let Some(base) = str_field(&snap_info, "base").or_else(|| str_field(data, "base")) {
        let _ = write!(md, " · **Base:** {base}");
    }
    md.push('\n');
    if let Some(publisher) = snap_info
        .get("publisher")
        .or_else(|| data.get("publisher"))
        .and_then(format_publisher)
    {
        let _ = writeln!(md, "**Publisher:** {publisher}");
    }
    if let Some(downloads) = extract_downloads(&snap_info, data) {
        let _ = writeln!(md, "**Downloads:** {}", format_number(downloads));
    }
    md.push('\n');

    append_channels(&mut md, &collect_channels(&channel_map));

    let description =
        str_field(&snap_info, "description").or_else(|| str_field(data, "description"));
    if let Some(description) = description.or(summary) {
        let _ = write!(md, "## Description\n\n{description}\n");
    }
    md
}

#[async_trait]
impl SpecialHandler for SnapcraftHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let snap_name = parse_snap_name(url)?;
        let api_url = format!(
            "https://api.snapcraft.io/v2/snaps/info/{}",
            super::util::percent_encode_component(&snap_name)
        );
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                headers: vec![
                    ("Accept".to_string(), "application/json".to_string()),
                    ("Snap-Device-Series".to_string(), "16".to_string()),
                ],
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&data, &snap_name);
        Some(build_result(
            &md,
            url,
            "snapcraft",
            vec!["Fetched via Snapcraft API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_snap_name_reads_install_and_direct() {
        assert_eq!(
            parse_snap_name("https://snapcraft.io/install/firefox"),
            Some("firefox".to_string())
        );
        assert_eq!(
            parse_snap_name("https://snapcraft.io/signal-desktop"),
            Some("signal-desktop".to_string())
        );
        assert_eq!(parse_snap_name("https://example.com/firefox"), None);
    }

    #[test]
    fn publisher_appends_username_when_it_differs() {
        assert_eq!(
            format_publisher(&json!({ "display-name": "Mozilla", "username": "mozilla" })),
            Some("Mozilla (@mozilla)".to_string())
        );
        assert_eq!(
            format_publisher(&json!({ "display-name": "canonical", "username": "canonical" })),
            Some("canonical".to_string())
        );
    }

    #[test]
    fn render_lays_out_meta_and_channels() {
        let data = json!({
            "snap": {
                "title": "Firefox",
                "summary": "Mozilla Firefox web browser",
                "publisher": { "display-name": "Mozilla", "username": "mozilla" },
                "confinement": "strict",
                "base": "core22"
            },
            "channel-map": [
                {
                    "channel": { "track": "latest", "risk": "stable", "architecture": "amd64" },
                    "version": "120.0"
                },
                {
                    "channel": { "track": "latest", "risk": "stable", "architecture": "arm64" },
                    "version": "120.0"
                }
            ]
        });
        let md = render(&data, "firefox");
        assert!(md.contains("# Firefox"));
        assert!(md.contains("Mozilla Firefox web browser"));
        assert!(md.contains("**Version:** 120.0 · **Confinement:** strict · **Base:** core22"));
        assert!(md.contains("**Publisher:** Mozilla (@mozilla)"));
        assert!(md.contains("- latest/stable: 120.0 (amd64, arm64)"));
    }
}
