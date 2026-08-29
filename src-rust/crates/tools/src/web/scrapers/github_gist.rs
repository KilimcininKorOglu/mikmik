// GitHub Gist handler: renders a gist and its files from the GitHub API.

use super::github::fetch_github_api;
use super::util::{build_result, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct GitHubGistHandler;

static HEX_ID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-fA-F0-9]+$").expect("static gist id"));

fn parse_gist_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "gist.github.com" {
        return None;
    }
    let last = parsed.path().split('/').rfind(|s| !s.is_empty())?;
    HEX_ID.is_match(last).then(|| last.to_string())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn render(gist: &Value) -> String {
    let owner = gist
        .get("owner")
        .and_then(|o| str_field(o, "login"))
        .unwrap_or("anonymous");
    let mut md = format!("# Gist by {owner}\n\n");
    if let Some(description) = str_field(gist, "description") {
        let _ = write!(md, "{description}\n\n");
    }
    let created = str_field(gist, "created_at").unwrap_or("");
    let updated = str_field(gist, "updated_at").unwrap_or("");
    let _ = writeln!(md, "**Created:** {created} · **Updated:** {updated}");

    let files: Vec<&Value> = gist
        .get("files")
        .and_then(Value::as_object)
        .map(|m| m.values().collect())
        .unwrap_or_default();
    let _ = write!(md, "**Files:** {}\n\n", files.len());
    for file in files {
        let filename = str_field(file, "filename").unwrap_or("file");
        let lang = str_field(file, "language").unwrap_or("").to_lowercase();
        let content = str_field(file, "content").unwrap_or("");
        let _ = write!(md, "---\n\n## {filename}\n\n```{lang}\n{content}\n```\n\n");
    }
    md
}

#[async_trait]
impl SpecialHandler for GitHubGistHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let gist_id = parse_gist_id(url)?;
        let gist = fetch_github_api(&format!("/gists/{gist_id}"), timeout).await?;
        let md = render(&gist);
        Some(build_result(
            &md,
            url,
            "github-gist",
            vec!["Fetched via GitHub API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_gist_id_reads_last_hex_segment() {
        assert_eq!(
            parse_gist_id("https://gist.github.com/octocat/aa5a315d61ae9438b18d"),
            Some("aa5a315d61ae9438b18d".to_string())
        );
        assert_eq!(
            parse_gist_id("https://gist.github.com/aa5a315d61ae9438b18d"),
            Some("aa5a315d61ae9438b18d".to_string())
        );
        assert_eq!(
            parse_gist_id("https://gist.github.com/octocat/not-hex"),
            None
        );
        assert_eq!(parse_gist_id("https://example.com/abc123"), None);
    }

    #[test]
    fn render_lays_out_gist_and_files() {
        let gist = json!({
            "description": "A useful snippet",
            "owner": { "login": "octocat" },
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-02T00:00:00Z",
            "files": {
                "hello.rs": { "filename": "hello.rs", "language": "Rust", "content": "fn main() {}" }
            }
        });
        let md = render(&gist);
        assert!(md.contains("# Gist by octocat"));
        assert!(md.contains("A useful snippet"));
        assert!(
            md.contains("**Created:** 2024-01-01T00:00:00Z · **Updated:** 2024-01-02T00:00:00Z")
        );
        assert!(md.contains("**Files:** 1"));
        assert!(md.contains("## hello.rs\n\n```rust\nfn main() {}\n```"));
    }
}
