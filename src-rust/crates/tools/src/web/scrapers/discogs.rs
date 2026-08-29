// Discogs handler: renders a release or a master via the Discogs API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::Duration;

pub struct DiscogsHandler;

static RELEASE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/release/(\d+)").expect("static discogs release regex"));
static MASTER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/master/(\d+)").expect("static discogs master regex"));

enum Target {
    Release(String),
    Master(String),
}

fn parse_target(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("discogs.com") {
        return None;
    }
    let path = parsed.path();
    if let Some(m) = RELEASE.captures(path) {
        return Some(Target::Release(m[1].to_string()));
    }
    MASTER
        .captures(path)
        .map(|m| Target::Master(m[1].to_string()))
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

/// Join artist names, honouring each entry's `join` phrase (`&`, `,`, …).
fn format_artists(artists: &Value) -> String {
    let Some(list) = artists.as_array().filter(|a| !a.is_empty()) else {
        return "Unknown Artist".to_string();
    };
    let mut out = String::new();
    for a in list {
        let name = str_field(a, "anv")
            .or_else(|| str_field(a, "name"))
            .unwrap_or("");
        out.push_str(name);
        if let Some(join) = str_field(a, "join") {
            let _ = write!(out, " {join} ");
        }
    }
    out.trim_end_matches([',', '&', ' ']).trim().to_string()
}

fn format_track(track: &Value) -> String {
    let position = str_field(track, "position");
    let prefix = match position {
        Some(p) => format!("{p}. "),
        None => "- ".to_string(),
    };
    let mut line = format!("{prefix}{}", str_field(track, "title").unwrap_or(""));
    if let Some(duration) = str_field(track, "duration") {
        let _ = write!(line, " ({duration})");
    }
    if let Some(artists) = track
        .get("artists")
        .filter(|a| a.as_array().is_some_and(|l| !l.is_empty()))
    {
        let _ = write!(line, " - {}", format_artists(artists));
    }
    line
}

fn format_formats(release: &Value) -> Option<String> {
    let formats = release.get("formats").and_then(Value::as_array)?;
    let rendered: Vec<String> = formats
        .iter()
        .map(|f| {
            let mut parts: Vec<String> = Vec::new();
            if let Some(qty) = str_field(f, "qty")
                .and_then(|q| q.parse::<u32>().ok())
                .filter(|q| *q > 1)
            {
                parts.push(format!("{qty}×"));
            }
            if let Some(name) = str_field(f, "name") {
                parts.push(name.to_string());
            }
            let descriptions = str_list(f, "descriptions");
            if !descriptions.is_empty() {
                parts.push(descriptions.join(", "));
            }
            parts.join(" ")
        })
        .collect();
    (!rendered.is_empty()).then(|| rendered.join(" + "))
}

fn format_labels(release: &Value) -> Option<String> {
    let labels = release.get("labels").and_then(Value::as_array)?;
    let rendered: Vec<String> = labels
        .iter()
        .filter_map(|l| {
            let name = str_field(l, "name")?;
            Some(match str_field(l, "catno").filter(|c| *c != "none") {
                Some(catno) => format!("{name} ({catno})"),
                None => name.to_string(),
            })
        })
        .collect();
    (!rendered.is_empty()).then(|| rendered.join(", "))
}

fn format_credits(release: &Value) -> Option<String> {
    let extra = release.get("extraartists").and_then(Value::as_array)?;
    if extra.is_empty() {
        return None;
    }
    let mut by_role: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for a in extra {
        let role = str_field(a, "role").unwrap_or("Other").to_string();
        let name = str_field(a, "anv")
            .or_else(|| str_field(a, "name"))
            .unwrap_or("");
        by_role.entry(role).or_default().push(name.to_string());
    }
    let lines: Vec<String> = by_role
        .iter()
        .map(|(role, names)| format!("- **{role}**: {}", names.join(", ")))
        .collect();
    Some(lines.join("\n"))
}

fn append_tracklist(md: &mut String, item: &Value) {
    let Some(tracks) = item
        .get("tracklist")
        .and_then(Value::as_array)
        .filter(|t| !t.is_empty())
    else {
        return;
    };
    md.push_str("## Tracklist\n\n");
    let lines: Vec<String> = tracks.iter().map(format_track).collect();
    let _ = write!(md, "{}\n\n", lines.join("\n"));
}

fn append_notes(md: &mut String, item: &Value) {
    if let Some(notes) = str_field(item, "notes") {
        let _ = write!(md, "## Notes\n\n{notes}\n");
    }
}

fn build_release(release: &Value) -> String {
    let artist = format_artists(release.get("artists").unwrap_or(&Value::Null));
    let title = str_field(release, "title").unwrap_or("");
    let mut md = format!("# {artist} - {title}\n\n");
    let mut meta: Vec<String> = Vec::new();
    if let Some(year) = release.get("year").and_then(Value::as_i64) {
        meta.push(format!("**Year**: {year}"));
    }
    if let Some(country) = str_field(release, "country") {
        meta.push(format!("**Country**: {country}"));
    }
    if let Some(format) = format_formats(release) {
        meta.push(format!("**Format**: {format}"));
    }
    if let Some(labels) = format_labels(release) {
        meta.push(format!("**Label**: {labels}"));
    }
    let genres = str_list(release, "genres");
    if !genres.is_empty() {
        meta.push(format!("**Genre**: {}", genres.join(", ")));
    }
    let styles = str_list(release, "styles");
    if !styles.is_empty() {
        meta.push(format!("**Style**: {}", styles.join(", ")));
    }
    if let Some(master) = release.get("master_id").and_then(Value::as_i64) {
        meta.push(format!(
            "**Master Release**: [{master}](https://www.discogs.com/master/{master})"
        ));
    }
    if !meta.is_empty() {
        let _ = write!(md, "{}\n\n", meta.join("\n"));
    }
    append_tracklist(&mut md, release);
    if let Some(credits) = format_credits(release) {
        let _ = write!(md, "## Credits\n\n{credits}\n\n");
    }
    append_notes(&mut md, release);
    md
}

fn build_master(master: &Value) -> String {
    let artist = format_artists(master.get("artists").unwrap_or(&Value::Null));
    let title = str_field(master, "title").unwrap_or("");
    let mut md = format!("# {artist} - {title}\n\n*Master Release*\n\n");
    let mut meta: Vec<String> = Vec::new();
    if let Some(year) = master.get("year").and_then(Value::as_i64) {
        meta.push(format!("**Year**: {year}"));
    }
    let genres = str_list(master, "genres");
    if !genres.is_empty() {
        meta.push(format!("**Genre**: {}", genres.join(", ")));
    }
    let styles = str_list(master, "styles");
    if !styles.is_empty() {
        meta.push(format!("**Style**: {}", styles.join(", ")));
    }
    if let Some(main) = master.get("main_release").and_then(Value::as_i64) {
        meta.push(format!(
            "**Main Release**: [{main}](https://www.discogs.com/release/{main})"
        ));
    }
    if let Some(for_sale) = master
        .get("num_for_sale")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
    {
        meta.push(format!("**For Sale**: {for_sale} copies"));
        if let Some(price) = master.get("lowest_price").and_then(Value::as_f64) {
            meta.push(format!("**Lowest Price**: ${price:.2}"));
        }
    }
    if !meta.is_empty() {
        let _ = write!(md, "{}\n\n", meta.join("\n"));
    }
    append_tracklist(&mut md, master);
    append_notes(&mut md, master);
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
    if !result.ok {
        return None;
    }
    serde_json::from_str(&result.content).ok()
}

#[async_trait]
impl SpecialHandler for DiscogsHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let (md, note) = match parse_target(url)? {
            Target::Release(id) => {
                let data =
                    fetch_json(&format!("https://api.discogs.com/releases/{id}"), timeout).await?;
                (build_release(&data), "Fetched via Discogs API (release)")
            }
            Target::Master(id) => {
                let data =
                    fetch_json(&format!("https://api.discogs.com/masters/{id}"), timeout).await?;
                (build_master(&data), "Fetched via Discogs API (master)")
            }
        };
        Some(build_result(&md, url, "discogs", vec![note.to_string()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_reads_release_and_master_ids() {
        assert!(
            matches!(parse_target("https://www.discogs.com/release/249504-Rick-Astley"), Some(Target::Release(i)) if i == "249504")
        );
        assert!(
            matches!(parse_target("https://www.discogs.com/master/96559"), Some(Target::Master(i)) if i == "96559")
        );
        assert!(parse_target("https://example.com/release/1").is_none());
    }

    #[test]
    fn artists_join_with_their_phrase() {
        let artists = json!([{ "name": "A", "join": "&" }, { "name": "B" }]);
        assert_eq!(format_artists(&artists), "A & B");
        assert_eq!(format_artists(&json!([])), "Unknown Artist");
    }

    #[test]
    fn build_release_lays_out_meta_and_tracks() {
        let release = json!({
            "title": "Whenever You Need Somebody",
            "artists": [{ "name": "Rick Astley" }],
            "year": 1987,
            "country": "UK",
            "formats": [{ "name": "Vinyl", "qty": "1", "descriptions": ["LP", "Album"] }],
            "labels": [{ "name": "RCA", "catno": "PL 71529" }],
            "genres": ["Electronic", "Pop"],
            "master_id": 96559,
            "tracklist": [{ "position": "A1", "title": "Never Gonna Give You Up", "duration": "3:32" }]
        });
        let md = build_release(&release);
        assert!(md.contains("# Rick Astley - Whenever You Need Somebody"));
        assert!(md.contains("**Year**: 1987"));
        assert!(md.contains("**Format**: Vinyl LP, Album"));
        assert!(md.contains("**Label**: RCA (PL 71529)"));
        assert!(md.contains("**Genre**: Electronic, Pop"));
        assert!(md.contains("**Master Release**: [96559](https://www.discogs.com/master/96559)"));
        assert!(md.contains("A1. Never Gonna Give You Up (3:32)"));
    }
}
