// Vimeo handler: renders a video from the Vimeo oEmbed API plus the player
// config for quality details.

use super::util::{build_result, format_media_duration, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct VimeoHandler;

static PLAYER_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/video/(\d+)").expect("static vimeo player regex"));
static NUMERIC: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d+$").expect("static vimeo numeric"));

fn extract_video_id(parsed: &url::Url) -> Option<String> {
    let host = parsed.host_str()?;
    if host == "player.vimeo.com" {
        return PLAYER_PATH
            .captures(parsed.path())
            .map(|c| c[1].to_string());
    }
    if host == "vimeo.com" || host == "www.vimeo.com" {
        let last = parsed.path().split('/').rfind(|s| !s.is_empty())?;
        if NUMERIC.is_match(last) {
            return Some(last.to_string());
        }
    }
    None
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn render(oembed: &Value, video_id: &str) -> String {
    let mut md = format!("# {}\n\n", str_field(oembed, "title").unwrap_or("(video)"));
    let author = str_field(oembed, "author_name").unwrap_or("");
    let author_url = str_field(oembed, "author_url").unwrap_or("");
    let _ = writeln!(md, "**Author:** [{author}]({author_url})");
    if let Some(duration) = oembed.get("duration").and_then(Value::as_u64) {
        let _ = writeln!(md, "**Duration:** {}", format_media_duration(duration));
    }
    if let Some(uploaded) = str_field(oembed, "upload_date") {
        let _ = writeln!(md, "**Uploaded:** {uploaded}");
    }
    let _ = write!(md, "**Video ID:** {video_id}\n\n");
    if let Some(desc) = str_field(oembed, "description") {
        let _ = write!(md, "---\n\n## Description\n\n{desc}\n\n");
    }
    let _ = write!(
        md,
        "---\n\n**Thumbnail:** {}\n",
        str_field(oembed, "thumbnail_url").unwrap_or("")
    );
    md
}

/// Append up to five progressive qualities from the player config.
fn append_qualities(md: &mut String, config: &Value) {
    let progressive = config
        .get("request")
        .and_then(|r| r.get("files"))
        .and_then(|f| f.get("progressive"))
        .and_then(Value::as_array);
    let Some(list) = progressive.filter(|p| !p.is_empty()) else {
        return;
    };
    md.push_str("\n**Available Qualities:**\n");
    for quality in list.iter().take(5) {
        let name = str_field(quality, "quality").unwrap_or("");
        let width = quality.get("width").and_then(Value::as_i64).unwrap_or(0);
        let height = quality.get("height").and_then(Value::as_i64).unwrap_or(0);
        let fps = quality.get("fps").and_then(Value::as_i64).unwrap_or(0);
        let _ = writeln!(md, "- {name}: {width}x{height} @ {fps}fps");
    }
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}

#[async_trait]
impl SpecialHandler for VimeoHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        if !parsed.host_str()?.contains("vimeo.com") {
            return None;
        }
        let video_id = extract_video_id(&parsed)?;

        let canonical = format!("https://vimeo.com/{video_id}");
        let oembed_url = format!(
            "https://vimeo.com/api/oembed.json?url={}",
            super::util::percent_encode_component(&canonical)
        );
        let oembed = fetch_json(&oembed_url, timeout).await?;
        let mut md = render(&oembed, &video_id);

        let config_url = format!("https://player.vimeo.com/video/{video_id}/config");
        if let Some(config) = fetch_json(&config_url, timeout.min(Duration::from_secs(5))).await {
            append_qualities(&mut md, &config);
        }

        Some(build_result(
            &md,
            url,
            "vimeo",
            vec!["Fetched via Vimeo oEmbed API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id_of(url: &str) -> Option<String> {
        extract_video_id(&url::Url::parse(url).expect("url"))
    }

    #[test]
    fn video_id_reads_all_url_shapes() {
        assert_eq!(
            id_of("https://vimeo.com/123456789"),
            Some("123456789".to_string())
        );
        assert_eq!(
            id_of("https://vimeo.com/user/987654321"),
            Some("987654321".to_string())
        );
        assert_eq!(
            id_of("https://player.vimeo.com/video/555"),
            Some("555".to_string())
        );
        assert_eq!(id_of("https://vimeo.com/channels/staffpicks"), None);
    }

    #[test]
    fn render_lays_out_video_metadata() {
        let oembed = json!({
            "title": "My Film",
            "author_name": "Director",
            "author_url": "https://vimeo.com/director",
            "duration": 185,
            "upload_date": "2024-01-01 12:00:00",
            "description": "A short film.",
            "thumbnail_url": "https://i.vimeocdn.com/x.jpg"
        });
        let md = render(&oembed, "123456789");
        assert!(md.contains("# My Film"));
        assert!(md.contains("**Author:** [Director](https://vimeo.com/director)"));
        assert!(md.contains("**Duration:** 3:05"));
        assert!(md.contains("**Video ID:** 123456789"));
        assert!(md.contains("## Description\n\nA short film."));
    }

    #[test]
    fn qualities_appended_from_config() {
        let config = json!({
            "request": { "files": { "progressive": [
                { "quality": "1080p", "width": 1920, "height": 1080, "fps": 30 }
            ] } }
        });
        let mut md = String::new();
        append_qualities(&mut md, &config);
        assert!(md.contains("- 1080p: 1920x1080 @ 30fps"));
    }
}
