// RAWG handler: renders a video game from the RAWG games API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::time::Duration;

pub struct RawgHandler;

static GAME_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/games/([^/?#]+)").expect("static rawg regex"));

fn parse_slug(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "rawg.io" && host != "www.rawg.io" {
        return None;
    }
    let raw = &GAME_PATH.captures(parsed.path())?[1];
    let slug = super::util::percent_decode(raw).trim().to_string();
    (!slug.is_empty()).then_some(slug)
}

/// True when the API rejected an anonymous request and asked for a key.
fn requires_api_key(game: &Value) -> bool {
    let detail = format!(
        "{} {}",
        game.get("detail").and_then(Value::as_str).unwrap_or(""),
        game.get("error").and_then(Value::as_str).unwrap_or("")
    )
    .to_lowercase();
    detail.contains("api key") || detail.contains("key is required") || detail.contains("apikey")
}

/// Deduplicated, order-stable names pulled from a `[{... name}]` list via `path`.
fn collect_names(game: &Value, key: &str, nested: Option<&str>) -> Vec<String> {
    let Some(entries) = game.get(key).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for entry in entries {
        let name = match nested {
            Some(inner) => entry.get(inner).and_then(|n| n.get("name")),
            None => entry.get("name"),
        }
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
        if let Some(name) = name {
            if seen.insert(name.to_string()) {
                out.push(name.to_string());
            }
        }
    }
    out
}

fn description(game: &Value) -> Option<String> {
    game.get("description_raw")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn render(game: &Value, slug: &str) -> String {
    let title = game
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(slug);
    let mut md = format!("# {title}\n\n");

    if let Some(released) = game
        .get("released")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let _ = writeln!(md, "**Released:** {released}");
    }
    if let Some(rating) = game.get("rating").and_then(Value::as_f64) {
        let _ = writeln!(md, "**Rating:** {rating:.2} / 5");
    }
    let platforms = collect_names(game, "platforms", Some("platform"));
    if !platforms.is_empty() {
        let _ = writeln!(md, "**Platforms:** {}", platforms.join(", "));
    }
    let genres = collect_names(game, "genres", None);
    if !genres.is_empty() {
        let _ = writeln!(md, "**Genres:** {}", genres.join(", "));
    }
    let _ = writeln!(
        md,
        "**RAWG:** https://rawg.io/games/{}",
        super::util::percent_encode_component(slug)
    );
    md.push('\n');

    if let Some(desc) = description(game) {
        let _ = write!(md, "## Description\n\n{desc}\n");
    }
    md
}

#[async_trait]
impl SpecialHandler for RawgHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let slug = parse_slug(url)?;
        let api_url = format!(
            "https://api.rawg.io/api/games/{}",
            super::util::percent_encode_component(&slug)
        );
        let result = load_page(
            &api_url,
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
        let game: Value = serde_json::from_str(&result.content).ok()?;
        if requires_api_key(&game) {
            return None;
        }
        let md = render(&game, &slug);
        Some(build_result(
            &md,
            url,
            "rawg",
            vec!["Fetched via RAWG API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_slug_reads_game_path() {
        assert_eq!(
            parse_slug("https://rawg.io/games/the-witcher-3-wild-hunt"),
            Some("the-witcher-3-wild-hunt".to_string())
        );
        assert_eq!(parse_slug("https://example.com/games/x"), None);
    }

    #[test]
    fn requires_api_key_detects_the_error() {
        assert!(requires_api_key(
            &json!({ "error": "An API Key is required." })
        ));
        assert!(!requires_api_key(&json!({ "name": "Game" })));
    }

    #[test]
    fn render_lays_out_metadata_and_description() {
        let game = json!({
            "name": "The Witcher 3",
            "released": "2015-05-18",
            "rating": 4.66,
            "platforms": [{ "platform": { "name": "PC" } }, { "platform": { "name": "PC" } }],
            "genres": [{ "name": "RPG" }, { "name": "Action" }],
            "description_raw": "An open-world RPG."
        });
        let md = render(&game, "the-witcher-3");
        assert!(md.contains("# The Witcher 3"));
        assert!(md.contains("**Released:** 2015-05-18"));
        assert!(md.contains("**Rating:** 4.66 / 5"));
        assert!(md.contains("**Platforms:** PC"));
        assert!(md.contains("**Genres:** RPG, Action"));
        assert!(md.contains("## Description\n\nAn open-world RPG."));
    }
}
