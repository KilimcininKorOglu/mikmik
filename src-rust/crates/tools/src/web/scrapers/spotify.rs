// Spotify handler: renders a track, album, playlist, or podcast from the
// oEmbed API plus Open Graph metadata scraped from the page.

use super::util::{build_result, format_media_duration, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write;
use std::time::Duration;

pub struct SpotifyHandler;

static META_TAG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)<meta\s+(?:property|name)="([^"]+)"\s+content="([^"]*)"[^>]*>"#)
        .expect("static spotify meta regex")
});

fn content_type(url: &str) -> Option<&'static str> {
    for (needle, label) in [
        ("/episode/", "podcast-episode"),
        ("/show/", "podcast-show"),
        ("/track/", "track"),
        ("/album/", "album"),
        ("/playlist/", "playlist"),
    ] {
        if url.contains(needle) {
            return Some(label);
        }
    }
    None
}

/// Collect the Open Graph / music meta tags relevant to a Spotify page.
fn parse_open_graph(html: &str) -> HashMap<String, String> {
    let mut og: HashMap<String, String> = HashMap::new();
    for caps in META_TAG.captures_iter(html) {
        let property = &caps[1];
        let content = caps[2].to_string();
        let key = match property {
            "og:title" => "title",
            "og:description" => "description",
            "og:image" => "image",
            "music:duration" => "duration",
            "music:album" => "album",
            "music:musician" => "musician",
            "music:release_date" => "release_date",
            "twitter:audio:artist_name" => "artist",
            _ => continue,
        };
        og.entry(key.to_string()).or_insert(content);
    }
    og
}

fn format_duration(seconds: &str) -> Option<String> {
    seconds.parse::<u64>().ok().map(format_media_duration)
}

fn append_media_fields(md: &mut String, og: &HashMap<String, String>) {
    if let Some(artist) = og.get("artist").or_else(|| og.get("musician")) {
        let _ = write!(md, "**Artist**: {artist}\n\n");
    }
    if let Some(album) = og.get("album") {
        let _ = write!(md, "**Album**: {album}\n\n");
    }
    if let Some(duration) = og.get("duration").and_then(|d| format_duration(d)) {
        let _ = write!(md, "**Duration**: {duration}\n\n");
    }
}

fn note_for(content_type: &str) -> Option<&'static str> {
    match content_type {
        "playlist" => Some(
            "**Note**: Playlist details (tracks, creator, follower count) require authentication. \
             Only basic metadata is available without Spotify API credentials.",
        ),
        "album" => Some(
            "**Note**: Track listing and detailed album information require authentication. \
             Only basic metadata is available without Spotify API credentials.",
        ),
        "podcast-show" => Some(
            "**Note**: Episode listing and detailed show information require authentication. \
             Only basic metadata is available without Spotify API credentials.",
        ),
        _ => None,
    }
}

fn render(content_type: &str, oembed: &Value, og: &HashMap<String, String>, url: &str) -> String {
    let title = og
        .get("title")
        .map(String::as_str)
        .or_else(|| oembed.get("title").and_then(Value::as_str))
        .unwrap_or("Unknown");
    let mut md = format!("# {title}\n\n**Type**: {content_type}\n\n");
    if let Some(desc) = og.get("description") {
        let _ = write!(md, "**Description**: {desc}\n\n");
    }
    if content_type == "track" || content_type == "podcast-episode" {
        append_media_fields(&mut md, og);
    }
    if content_type == "album" {
        if let Some(release) = og.get("release_date") {
            let _ = write!(md, "**Release Date**: {release}\n\n");
        }
    }
    md.push_str("\n---\n\n");
    if let Some(note) = note_for(content_type) {
        let _ = write!(md, "{note}\n\n");
    }
    let _ = write!(md, "**URL**: {url}\n\n");
    if let Some(thumb) = oembed.get("thumbnail_url").and_then(Value::as_str) {
        let _ = writeln!(md, "**Thumbnail**: {thumb}");
    } else if let Some(image) = og.get("image") {
        let _ = writeln!(md, "**Image**: {image}");
    }
    md
}

async fn fetch(url: &str, timeout: Duration) -> Option<String> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    result.ok.then_some(result.content)
}

#[async_trait]
impl SpecialHandler for SpotifyHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        if !url.contains("open.spotify.com/") {
            return None;
        }
        let content_type = content_type(url)?;

        let oembed_url = format!(
            "https://open.spotify.com/oembed?url={}",
            super::util::percent_encode_component(url)
        );
        let oembed = fetch(&oembed_url, timeout)
            .await
            .and_then(|body| serde_json::from_str(&body).ok())
            .unwrap_or(Value::Null);
        let og = fetch(url, timeout)
            .await
            .map(|html| parse_open_graph(&html))
            .unwrap_or_default();

        let md = render(content_type, &oembed, &og, url);
        Some(build_result(
            &md,
            url,
            "spotify",
            vec!["Fetched via Spotify oEmbed API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn content_type_reads_the_path() {
        assert_eq!(
            content_type("https://open.spotify.com/track/abc"),
            Some("track")
        );
        assert_eq!(
            content_type("https://open.spotify.com/playlist/xyz"),
            Some("playlist")
        );
        assert_eq!(content_type("https://open.spotify.com/user/me"), None);
    }

    #[test]
    fn open_graph_parses_meta_tags() {
        let html = r#"<meta property="og:title" content="Song Name"><meta property="music:musician" content="Artist"><meta property="music:duration" content="215">"#;
        let og = parse_open_graph(html);
        assert_eq!(og.get("title").map(String::as_str), Some("Song Name"));
        assert_eq!(og.get("musician").map(String::as_str), Some("Artist"));
        assert_eq!(og.get("duration").map(String::as_str), Some("215"));
    }

    #[test]
    fn render_lays_out_track_metadata() {
        let oembed = json!({ "thumbnail_url": "https://i.scdn.co/x.jpg" });
        let mut og = HashMap::new();
        og.insert("title".to_string(), "Song Name".to_string());
        og.insert("musician".to_string(), "Artist".to_string());
        og.insert("album".to_string(), "The Album".to_string());
        og.insert("duration".to_string(), "215".to_string());
        let md = render("track", &oembed, &og, "https://open.spotify.com/track/abc");
        assert!(md.contains("# Song Name"));
        assert!(md.contains("**Type**: track"));
        assert!(md.contains("**Artist**: Artist"));
        assert!(md.contains("**Album**: The Album"));
        assert!(md.contains("**Duration**: 3:35"));
        assert!(md.contains("**Thumbnail**: https://i.scdn.co/x.jpg"));
    }

    #[test]
    fn album_shows_auth_note() {
        let md = render(
            "album",
            &Value::Null,
            &HashMap::new(),
            "https://open.spotify.com/album/x",
        );
        assert!(md.contains("Track listing and detailed album information require authentication"));
    }
}
