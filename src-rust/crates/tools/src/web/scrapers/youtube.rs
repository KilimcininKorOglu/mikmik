// YouTube handler: renders a video's metadata and transcript via `yt-dlp`.
// When `yt-dlp` is not on PATH the handler reports that instead of failing, so
// the URL still yields a useful message.

use super::util::{build_result, format_media_duration, format_number, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::path::PathBuf;
use std::time::Duration;

pub struct YouTubeHandler;

const DESCRIPTION_LIMIT: usize = 1000;
const VIDEO_ID: &str = r"[a-zA-Z0-9_-]{11}";

static PATH_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(&format!(r"^/(v|embed)/({VIDEO_ID})")).expect("static yt path regex"));
static BARE_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(&format!(r"^{VIDEO_ID}$")).expect("static yt id regex"));
static TIMESTAMP_LINE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\d{2}:\d{2}").expect("static vtt ts regex"));
static UUID_CUE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-f0-9-]{36}$").expect("static vtt uuid regex"));
static NUMERIC_CUE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d+$").expect("static vtt num regex"));
static INLINE_TS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<\d{2}:\d{2}:\d{2}\.\d{3}>").expect("static vtt inline ts regex"));
static VTT_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"</?[^>]+>").expect("static vtt tag regex"));

fn video_id_from_host_path(host: &str, path: &str) -> Option<String> {
    if let Some(caps) = PATH_ID.captures(path) {
        return Some(caps[2].to_string());
    }
    if host == "youtube.com" {
        if let Some(id) = path
            .strip_prefix("/shorts/")
            .and_then(|p| p.split('/').next())
        {
            if BARE_ID.is_match(id) {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn parse_video_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed
        .host_str()?
        .strip_prefix("www.")
        .unwrap_or(parsed.host_str()?);
    if host == "youtu.be" {
        let id = parsed.path().trim_start_matches('/').split('/').next()?;
        return BARE_ID.is_match(id).then(|| id.to_string());
    }
    let is_youtube = host == "youtube.com" || host == "m.youtube.com";
    if is_youtube && parsed.path() == "/watch" {
        if let Some(id) = parsed
            .query_pairs()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.to_string())
        {
            return Some(id);
        }
    }
    if is_youtube {
        return video_id_from_host_path(host, parsed.path());
    }
    None
}

/// Reduce WebVTT subtitle text to a single line of de-duplicated captions.
fn clean_vtt_to_text(vtt: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut last = String::new();
    for line in vtt.lines() {
        if is_vtt_noise(line) {
            continue;
        }
        let without_ts = INLINE_TS.replace_all(line, "");
        let stripped = VTT_TAG.replace_all(&without_ts, "");
        let cleaned = stripped.trim().to_string();
        if !cleaned.is_empty() && cleaned != last {
            last.clone_from(&cleaned);
            out.push(cleaned);
        }
    }
    out.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_vtt_noise(line: &str) -> bool {
    line.starts_with("WEBVTT")
        || line.starts_with("Kind:")
        || line.starts_with("Language:")
        || TIMESTAMP_LINE.is_match(line)
        || UUID_CUE.is_match(line)
        || NUMERIC_CUE.is_match(line)
        || line.contains("-->")
        || line.trim().is_empty()
}

async fn run_yt_dlp(bin: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn format_upload_date(raw: &str) -> Option<String> {
    (raw.len() == 8 && raw.chars().all(|c| c.is_ascii_digit()))
        .then(|| format!("{}-{}-{}", &raw[0..4], &raw[4..6], &raw[6..8]))
}

fn append_metadata(md: &mut String, meta: &Value, video_id: &str) {
    if let Some(channel) = meta
        .get("channel")
        .and_then(Value::as_str)
        .or_else(|| meta.get("uploader").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
    {
        let _ = writeln!(md, "**Channel:** {channel}");
    }
    if let Some(date) = meta
        .get("upload_date")
        .and_then(Value::as_str)
        .and_then(format_upload_date)
    {
        let _ = writeln!(md, "**Uploaded:** {date}");
    }
    let duration = meta.get("duration").and_then(Value::as_u64).unwrap_or(0);
    if duration > 0 {
        let _ = writeln!(md, "**Duration:** {}", format_media_duration(duration));
    }
    let views = meta.get("view_count").and_then(Value::as_u64).unwrap_or(0);
    if views > 0 {
        let _ = writeln!(md, "**Views:** {}", format_number(views));
    }
    let _ = write!(md, "**Video ID:** {video_id}\n\n");
}

fn append_description(md: &mut String, meta: &Value) {
    let Some(description) = meta
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let preview = if description.chars().count() > DESCRIPTION_LIMIT {
        let truncated: String = description.chars().take(DESCRIPTION_LIMIT).collect();
        format!("{truncated}…")
    } else {
        description.to_string()
    };
    let _ = write!(md, "---\n\n## Description\n\n{preview}\n\n");
}

fn temp_base(video_id: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("yt-{video_id}-{}-{nanos}", std::process::id()))
}

async fn download_transcript(bin: &str, flag: &str, base: &str, video_url: &str) -> Option<String> {
    run_yt_dlp(
        bin,
        &[
            flag,
            "--sub-lang",
            "en,en-US,en-GB",
            "--sub-format",
            "vtt",
            "--skip-download",
            "--no-warnings",
            "--no-playlist",
            "-o",
            base,
            video_url,
        ],
    )
    .await?;
    read_vtt(base)
}

fn read_vtt(base: &str) -> Option<String> {
    let pattern = format!("{base}*.vtt");
    let path = glob::glob(&pattern).ok()?.flatten().next()?;
    let content = std::fs::read_to_string(&path).ok();
    cleanup(base);
    content
        .map(|c| clean_vtt_to_text(&c))
        .filter(|t| !t.is_empty())
}

fn cleanup(base: &str) {
    if let Ok(paths) = glob::glob(&format!("{base}*")) {
        for path in paths.flatten() {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn fetch_transcript(
    bin: &str,
    video_id: &str,
    video_url: &str,
) -> (Option<String>, &'static str) {
    let list = run_yt_dlp(
        bin,
        &[
            "--list-subs",
            "--no-warnings",
            "--no-playlist",
            "--skip-download",
            video_url,
        ],
    )
    .await
    .unwrap_or_default();
    let base = temp_base(video_id);
    let base = base.to_string_lossy().to_string();

    if list.contains("[info] Available subtitles") {
        if let Some(text) = download_transcript(bin, "--write-sub", &base, video_url).await {
            return (Some(text), "manual");
        }
    }
    if list.contains("[info] Available automatic captions") {
        if let Some(text) = download_transcript(bin, "--write-auto-sub", &base, video_url).await {
            return (Some(text), "auto-generated");
        }
    }
    (None, "")
}

#[async_trait]
impl SpecialHandler for YouTubeHandler {
    async fn handle(&self, url: &str, _timeout: Duration) -> Option<RenderResult> {
        let video_id = parse_video_id(url)?;
        let video_url = format!("https://www.youtube.com/watch?v={video_id}");
        let Ok(bin) = which::which("yt-dlp") else {
            return Some(build_result(
                "YouTube video detected but yt-dlp could not be installed.",
                url,
                "youtube-no-ytdlp",
                vec!["yt-dlp installation failed".to_string()],
            ));
        };
        let bin = bin.to_string_lossy().to_string();

        let meta_json = run_yt_dlp(
            &bin,
            &[
                "--dump-json",
                "--no-warnings",
                "--no-playlist",
                "--skip-download",
                &video_url,
            ],
        )
        .await
        .unwrap_or_default();
        let meta: Value = serde_json::from_str(meta_json.trim()).unwrap_or(Value::Null);
        let title = meta
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("YouTube Video");

        let mut md = format!("# {title}\n\n");
        append_metadata(&mut md, &meta, &video_id);
        append_description(&mut md, &meta);

        let mut notes = Vec::new();
        let (transcript, source) = fetch_transcript(&bin, &video_id, &video_url).await;
        match transcript {
            Some(text) => {
                notes.push(format!("Using {source} subtitles"));
                let _ = write!(md, "---\n\n## Transcript ({source})\n\n{text}\n");
            }
            None => {
                notes.push("No subtitles/captions available".to_string());
                md.push_str("---\n\n*No transcript available for this video.*\n");
            }
        }
        Some(build_result(&md, url, "youtube", notes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_video_id_from_url_shapes() {
        assert_eq!(
            parse_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            parse_video_id("https://youtu.be/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            parse_video_id("https://youtube.com/shorts/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            parse_video_id("https://www.youtube.com/embed/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(parse_video_id("https://example.com/watch?v=x"), None);
    }

    #[test]
    fn clean_vtt_strips_noise_and_dedups() {
        let vtt = "WEBVTT\nKind: captions\nLanguage: en\n\n00:00:01.000 --> 00:00:02.000\n<00:00:01.520><c>Hello</c> world\n00:00:02.000 --> 00:00:03.000\nHello world\nnext line\n";
        assert_eq!(clean_vtt_to_text(vtt), "Hello world next line");
    }

    #[test]
    fn upload_date_formats_yyyymmdd() {
        assert_eq!(
            format_upload_date("20240115").as_deref(),
            Some("2024-01-15")
        );
        assert_eq!(format_upload_date("2024"), None);
    }

    #[test]
    fn metadata_lays_out_fields() {
        let meta = serde_json::json!({
            "channel": "Rick Astley",
            "upload_date": "20091025",
            "duration": 213,
            "view_count": 1600000000u64
        });
        let mut md = String::new();
        append_metadata(&mut md, &meta, "dQw4w9WgXcQ");
        assert!(md.contains("**Channel:** Rick Astley"));
        assert!(md.contains("**Uploaded:** 2009-10-25"));
        assert!(md.contains("**Duration:** 3:33"));
        assert!(md.contains("**Views:** 1,600,000,000"));
        assert!(md.contains("**Video ID:** dQw4w9WgXcQ"));
    }
}
