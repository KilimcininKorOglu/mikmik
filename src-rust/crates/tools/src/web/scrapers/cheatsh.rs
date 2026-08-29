// cheat.sh handler: renders a command cheatsheet from the cheat.sh plain-text
// API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use std::time::Duration;

pub struct CheatShHandler;

static CODE_KEYWORD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*(if|for|while|def|func|fn|let|const|var)\b").expect("static regex")
});

fn parse_topic(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "cheat.sh" && host != "cht.sh" {
        return None;
    }
    let topic = parsed.path().trim_start_matches('/');
    (!topic.is_empty()).then(|| super::util::percent_decode(topic))
}

/// A line hints at code when it is a shell prompt, comment, call, or keyword.
fn looks_like_code(content: &str) -> bool {
    content.lines().any(|line| {
        line.starts_with('$')
            || line.starts_with('#')
            || line.contains("()")
            || line.contains("=>")
            || CODE_KEYWORD.is_match(line)
    })
}

fn render(content: &str, topic: &str) -> String {
    let mut md = format!("# cheat.sh/{topic}\n\n");
    let fence = if looks_like_code(content) || topic.contains('/') {
        topic
            .split('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("bash")
    } else {
        ""
    };
    md.push_str("```");
    md.push_str(fence);
    md.push('\n');
    md.push_str(content);
    md.push_str("\n```\n");
    md
}

#[async_trait]
impl SpecialHandler for CheatShHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let topic = parse_topic(url)?;
        let api_url = format!(
            "https://cheat.sh/{}?T",
            super::util::percent_encode_component(&topic)
        );
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                headers: vec![("Accept".to_string(), "text/plain".to_string())],
                ..Default::default()
            },
        )
        .await;
        if !result.ok || result.content.trim().is_empty() {
            return None;
        }
        let md = render(result.content.trim(), &topic);
        Some(build_result(
            &md,
            url,
            "cheat.sh",
            vec!["Fetched via cheat.sh".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_topic_reads_the_path() {
        assert_eq!(parse_topic("https://cheat.sh/tar"), Some("tar".to_string()));
        assert_eq!(
            parse_topic("https://cht.sh/python/list"),
            Some("python/list".to_string())
        );
        assert_eq!(parse_topic("https://cheat.sh/"), None);
        assert_eq!(parse_topic("https://example.com/tar"), None);
    }

    #[test]
    fn render_uses_language_fence_for_code() {
        let md = render("def foo():\n    pass", "python");
        assert!(md.contains("# cheat.sh/python"));
        assert!(md.contains("```python\ndef foo():"));
    }

    #[test]
    fn render_uses_bare_fence_for_prose() {
        let md = render("A plain description with no code markers.", "tar");
        assert!(md.contains("```\nA plain description"));
    }

    #[test]
    fn render_derives_language_from_topic_path() {
        let md = render("some list ops", "go/slice");
        assert!(md.contains("```go\n"));
    }
}
