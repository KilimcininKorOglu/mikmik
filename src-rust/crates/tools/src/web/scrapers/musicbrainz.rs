// MusicBrainz handler: renders an artist, a release, or a recording via the
// ws/2 JSON API.

use super::util::{build_result, format_media_duration, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct MusicBrainzHandler;

const MAX_TRACKS: usize = 50;

/// Which MusicBrainz entity a URL names, and the API path to fetch.
enum Entity {
    Artist(String),
    Release(String),
    Recording(String),
}

fn is_mbid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn parse_entity(url: &str) -> Option<Entity> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "musicbrainz.org" && host != "www.musicbrainz.org" {
        return None;
    }
    let mut parts = parsed.path().split('/').filter(|s| !s.is_empty());
    let kind = parts.next()?;
    let mbid = parts.next().filter(|m| is_mbid(m))?.to_string();
    match kind {
        "artist" => Some(Entity::Artist(mbid)),
        "release" => Some(Entity::Release(mbid)),
        "recording" => Some(Entity::Recording(mbid)),
        _ => None,
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn duration_ms(ms: Option<i64>) -> Option<String> {
    ms.filter(|m| *m > 0)
        .map(|m| format_media_duration((m as f64 / 1000.0).round() as u64))
}

fn format_life_span(life: &Value) -> Option<String> {
    let begin = str_field(life, "begin");
    let end = str_field(life, "end");
    let ended = life.get("ended").and_then(Value::as_bool);
    match (begin, end) {
        (Some(b), Some(e)) => Some(format!("{b} - {e}")),
        (Some(b), None) => Some(format!(
            "{b} - {}",
            if ended == Some(true) {
                "ended"
            } else {
                "present"
            }
        )),
        (None, Some(e)) => Some(format!("? - {e}")),
        (None, None) => ended.map(|e| if e { "ended" } else { "present" }.to_string()),
    }
}

fn format_artist_credits(recording: &Value) -> Option<String> {
    let credits = recording.get("artist-credit").and_then(Value::as_array)?;
    let names: Vec<&str> = credits
        .iter()
        .filter_map(|c| {
            str_field(c, "name").or_else(|| c.get("artist").and_then(|a| str_field(a, "name")))
        })
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

fn build_artist(artist: &Value) -> String {
    let mut md = format!("# {}\n\n", str_field(artist, "name").unwrap_or("(artist)"));
    let mut meta: Vec<String> = Vec::new();
    if let Some(kind) = str_field(artist, "type") {
        meta.push(format!("**Type**: {kind}"));
    }
    if let Some(country) = str_field(artist, "country") {
        meta.push(format!("**Country**: {country}"));
    }
    if let Some(life) = artist.get("life-span").and_then(format_life_span) {
        meta.push(format!("**Life Span**: {life}"));
    }
    if !meta.is_empty() {
        let _ = writeln!(md, "{}", meta.join("\n"));
    }
    md
}

fn format_track(track: &Value) -> String {
    let recording = track.get("recording").cloned().unwrap_or(Value::Null);
    let title = str_field(track, "title")
        .or_else(|| str_field(&recording, "title"))
        .unwrap_or("Untitled");
    let length = track
        .get("length")
        .and_then(Value::as_i64)
        .or_else(|| recording.get("length").and_then(Value::as_i64));
    let number = str_field(track, "number").map(str::to_string).or_else(|| {
        track
            .get("position")
            .and_then(Value::as_i64)
            .map(|p| p.to_string())
    });
    let prefix = match &number {
        Some(n) => format!("{n}. "),
        None => "- ".to_string(),
    };
    let mut line = format!("{prefix}{title}");
    if let Some(dur) = duration_ms(length) {
        let _ = write!(line, " ({dur})");
    }
    line
}

fn medium_label(medium: &Value, include_position: bool) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if include_position {
        if let Some(pos) = medium.get("position").and_then(Value::as_i64) {
            parts.push(format!("Disc {pos}"));
        }
    }
    if let Some(format) = str_field(medium, "format") {
        parts.push(format.to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" - "))
}

fn total_tracks(release: &Value, media: &[Value]) -> i64 {
    if let Some(count) = release.get("track-count").and_then(Value::as_i64) {
        return count;
    }
    media
        .iter()
        .map(|m| {
            m.get("track-count")
                .and_then(Value::as_i64)
                .or_else(|| {
                    m.get("tracks")
                        .and_then(Value::as_array)
                        .map(|t| t.len() as i64)
                })
                .unwrap_or(0)
        })
        .sum()
}

fn append_medium(md: &mut String, medium: &Value, include_position: bool) {
    if let Some(label) = medium_label(medium, include_position) {
        let _ = write!(md, "### {label}\n\n");
    }
    let tracks = medium.get("tracks").and_then(Value::as_array);
    match tracks.filter(|t| !t.is_empty()) {
        Some(tracks) => {
            let lines: Vec<String> = tracks.iter().take(MAX_TRACKS).map(format_track).collect();
            let _ = write!(md, "{}\n\n", lines.join("\n"));
            if tracks.len() > MAX_TRACKS {
                let _ = write!(
                    md,
                    "_Showing first {MAX_TRACKS} of {} tracks._\n\n",
                    tracks.len()
                );
            }
        }
        None => {
            if let Some(count) = medium.get("track-count").and_then(Value::as_i64) {
                let _ = write!(md, "- {count} tracks (details unavailable)\n\n");
            }
        }
    }
}

fn build_release(release: &Value) -> String {
    let mut md = format!(
        "# {}\n\n",
        str_field(release, "title").unwrap_or("(release)")
    );
    let media: Vec<Value> = release
        .get("media")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let count = total_tracks(release, &media);
    if count > 0 {
        let _ = write!(md, "**Tracks**: {count}\n\n");
    }
    if !media.is_empty() {
        md.push_str("## Tracks\n\n");
        let include_position = media.len() > 1;
        for medium in &media {
            append_medium(&mut md, medium, include_position);
        }
    }
    md
}

fn build_recording(recording: &Value) -> String {
    let mut md = format!(
        "# {}\n\n",
        str_field(recording, "title").unwrap_or("(recording)")
    );
    let mut meta: Vec<String> = Vec::new();
    if let Some(artists) = format_artist_credits(recording) {
        meta.push(format!("**Artists**: {artists}"));
    }
    if let Some(length) = duration_ms(recording.get("length").and_then(Value::as_i64)) {
        meta.push(format!("**Length**: {length}"));
    }
    if !meta.is_empty() {
        let _ = writeln!(md, "{}", meta.join("\n"));
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
    if !result.ok {
        return None;
    }
    serde_json::from_str(&result.content).ok()
}

#[async_trait]
impl SpecialHandler for MusicBrainzHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let base = "https://musicbrainz.org/ws/2";
        let md = match parse_entity(url)? {
            Entity::Artist(mbid) => {
                let data = fetch_json(
                    &format!("{base}/artist/{mbid}?fmt=json&inc=url-rels"),
                    timeout,
                )
                .await?;
                build_artist(&data)
            }
            Entity::Release(mbid) => {
                let data = fetch_json(
                    &format!("{base}/release/{mbid}?fmt=json&inc=recordings"),
                    timeout,
                )
                .await?;
                build_release(&data)
            }
            Entity::Recording(mbid) => {
                let data =
                    fetch_json(&format!("{base}/recording/{mbid}?fmt=json"), timeout).await?;
                build_recording(&data)
            }
        };
        Some(build_result(
            &md,
            url,
            "musicbrainz-api",
            vec!["Fetched via MusicBrainz API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MBID: &str = "5b11f4ce-a62d-471e-81fc-a69a8278c7da";

    #[test]
    fn parse_entity_needs_a_kind_and_a_valid_mbid() {
        assert!(matches!(
            parse_entity(&format!("https://musicbrainz.org/artist/{MBID}")),
            Some(Entity::Artist(m)) if m == MBID
        ));
        assert!(matches!(
            parse_entity(&format!("https://musicbrainz.org/release/{MBID}")),
            Some(Entity::Release(_))
        ));
        assert!(parse_entity("https://musicbrainz.org/artist/not-a-uuid").is_none());
        assert!(parse_entity(&format!("https://example.com/artist/{MBID}")).is_none());
    }

    #[test]
    fn build_artist_lays_out_metadata() {
        let artist = json!({
            "name": "Radiohead",
            "type": "Group",
            "country": "GB",
            "life-span": { "begin": "1985", "ended": false }
        });
        let md = build_artist(&artist);
        assert!(md.contains("# Radiohead"));
        assert!(md.contains("**Type**: Group"));
        assert!(md.contains("**Country**: GB"));
        assert!(md.contains("**Life Span**: 1985 - present"));
    }

    #[test]
    fn build_release_lays_out_tracks_with_durations() {
        let release = json!({
            "title": "OK Computer",
            "media": [{
                "position": 1, "format": "CD",
                "tracks": [
                    { "number": "1", "title": "Airbag", "length": 284000 },
                    { "position": 2, "recording": { "title": "Paranoid Android", "length": 383000 } }
                ]
            }]
        });
        let md = build_release(&release);
        assert!(md.contains("# OK Computer"));
        assert!(md.contains("**Tracks**: 2"));
        assert!(md.contains("1. Airbag (4:44)"));
        assert!(md.contains("2. Paranoid Android (6:23)"));
    }

    #[test]
    fn build_recording_shows_artists_and_length() {
        let recording = json!({
            "title": "Karma Police",
            "artist-credit": [{ "name": "Radiohead" }],
            "length": 261000
        });
        let md = build_recording(&recording);
        assert!(md.contains("# Karma Police"));
        assert!(md.contains("**Artists**: Radiohead"));
        assert!(md.contains("**Length**: 4:21"));
    }
}
