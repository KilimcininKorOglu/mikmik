// tldr handler: serves a tldr-pages command page as raw markdown.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use std::time::Duration;

pub struct TldrHandler;

const TLDR_BASE: &str = "https://raw.githubusercontent.com/tldr-pages/tldr/main/pages";
const PLATFORMS: [&str; 3] = ["common", "linux", "osx"];

fn parse_command(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "tldr.sh" && host != "tldr.ostera.io" {
        return None;
    }
    let command = parsed
        .path()
        .trim_start_matches('/')
        .trim_end_matches(".md");
    if command.is_empty() || command.contains('/') {
        return None;
    }
    Some(command.to_string())
}

#[async_trait]
impl SpecialHandler for TldrHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let command = parse_command(url)?;
        for platform in PLATFORMS {
            let raw_url = format!("{TLDR_BASE}/{platform}/{command}.md");
            let result = load_page(
                &raw_url,
                LoadOptions {
                    timeout,
                    ..Default::default()
                },
            )
            .await;
            if result.ok && !result.content.trim().is_empty() {
                return Some(build_result(
                    &result.content,
                    url,
                    "tldr",
                    vec![format!("Fetched from tldr-pages ({platform})")],
                ));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_strips_slash_and_extension() {
        assert_eq!(
            parse_command("https://tldr.sh/tar"),
            Some("tar".to_string())
        );
        assert_eq!(
            parse_command("https://tldr.ostera.io/git.md"),
            Some("git".to_string())
        );
        // A nested path is not a single command page.
        assert_eq!(parse_command("https://tldr.sh/common/tar"), None);
        assert_eq!(parse_command("https://example.com/tar"), None);
    }
}
