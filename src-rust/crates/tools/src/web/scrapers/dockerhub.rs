// Docker Hub handler: renders a repository and its recent tags via the v2 API.

use super::util::{
    build_result, format_bytes, format_iso_date, format_number, load_page, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct DockerHubHandler;

/// The `(namespace, repository)` from an official (`/_/img`) or regular
/// (`/r/ns/repo`) Docker Hub path.
fn parse_repo(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("hub.docker.com") {
        return None;
    }
    let path = parsed.path();
    if let Some(rest) = path.strip_prefix("/_/") {
        let repo = rest.split('/').next().filter(|s| !s.is_empty())?;
        return Some(("library".to_string(), repo.to_string()));
    }
    let rest = path.strip_prefix("/r/")?;
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let namespace = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    Some((namespace, repo))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
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

fn append_stats(md: &mut String, repo: &Value) {
    let mut stats: Vec<String> = Vec::new();
    if let Some(pulls) = repo.get("pull_count").and_then(Value::as_u64) {
        stats.push(format!("**Pulls:** {}", format_number(pulls)));
    }
    if let Some(stars) = repo.get("star_count").and_then(Value::as_u64) {
        stats.push(format!("**Stars:** {}", format_number(stars)));
    }
    if repo.get("is_official").and_then(Value::as_bool) == Some(true) {
        stats.push("**Official Image**".to_string());
    }
    if repo.get("is_automated").and_then(Value::as_bool) == Some(true) {
        stats.push("**Automated Build**".to_string());
    }
    if !stats.is_empty() {
        let _ = writeln!(md, "{}", stats.join(" · "));
    }
}

fn tag_architectures(tag: &Value) -> String {
    let archs: Vec<&str> = tag
        .get("images")
        .and_then(Value::as_array)
        .map(|imgs| {
            imgs.iter()
                .filter_map(|i| str_field(i, "architecture"))
                .collect()
        })
        .unwrap_or_default();
    if archs.is_empty() {
        "-".to_string()
    } else {
        archs.join(", ")
    }
}

fn append_tags(md: &mut String, tags: &[Value]) {
    if tags.is_empty() {
        return;
    }
    md.push_str("## Recent Tags\n\n");
    md.push_str("| Tag | Size | Architectures | Updated |\n");
    md.push_str("|-----|------|---------------|--------|\n");
    for tag in tags {
        let name = str_field(tag, "name").unwrap_or("-");
        let size = tag
            .get("full_size")
            .and_then(Value::as_u64)
            .map(format_bytes)
            .unwrap_or_else(|| "-".to_string());
        let archs = tag_architectures(tag);
        let updated = str_field(tag, "last_updated")
            .map(format_iso_date)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(md, "| `{name}` | {size} | {archs} | {updated} |");
    }
    md.push('\n');
}

fn render(namespace: &str, repo: &Value, tags: &[Value]) -> String {
    let name = str_field(repo, "name").unwrap_or("(image)");
    let full_name = if namespace == "library" {
        name.to_string()
    } else {
        format!("{namespace}/{name}")
    };
    let mut md = format!("# {full_name}\n\n");
    if let Some(desc) = str_field(repo, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    append_stats(&mut md, repo);
    if let Some(updated) = str_field(repo, "last_updated") {
        let date = format_iso_date(updated);
        if !date.is_empty() {
            let _ = writeln!(md, "**Last Updated:** {date}");
        }
    }
    let _ = write!(
        md,
        "\n## Quick Start\n\n```bash\ndocker pull {full_name}\n```\n\n"
    );
    append_tags(&mut md, tags);
    md
}

#[async_trait]
impl SpecialHandler for DockerHubHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let (namespace, repository) = parse_repo(url)?;
        let repo_url = format!("https://hub.docker.com/v2/repositories/{namespace}/{repository}/");
        let tags_url = format!(
            "https://hub.docker.com/v2/repositories/{namespace}/{repository}/tags/?page_size=10"
        );
        let (repo, tags_data) = tokio::join!(
            fetch_json(&repo_url, timeout),
            fetch_json(&tags_url, timeout.min(Duration::from_secs(10)))
        );
        let repo = repo?;
        let tags = tags_data
            .as_ref()
            .and_then(|d| d.get("results"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let md = render(&namespace, &repo, &tags);
        Some(build_result(
            &md,
            url,
            "dockerhub",
            vec!["Fetched via Docker Hub API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_repo_reads_official_and_regular_paths() {
        assert_eq!(
            parse_repo("https://hub.docker.com/_/nginx"),
            Some(("library".to_string(), "nginx".to_string()))
        );
        assert_eq!(
            parse_repo("https://hub.docker.com/r/grafana/grafana"),
            Some(("grafana".to_string(), "grafana".to_string()))
        );
        assert!(parse_repo("https://example.com/_/nginx").is_none());
    }

    #[test]
    fn render_lays_out_official_image_and_tags() {
        let repo = json!({
            "name": "nginx",
            "description": "Official build of Nginx",
            "pull_count": 5_000_000_000_u64,
            "star_count": 20000,
            "is_official": true,
            "last_updated": "2024-01-01T00:00:00Z"
        });
        let tags = vec![json!({
            "name": "latest",
            "full_size": 5_242_880,
            "last_updated": "2024-01-01T00:00:00Z",
            "images": [{ "architecture": "amd64" }, { "architecture": "arm64" }]
        })];
        let md = render("library", &repo, &tags);
        assert!(md.contains("# nginx"));
        assert!(md.contains("**Pulls:** 5,000,000,000 · **Stars:** 20,000 · **Official Image**"));
        assert!(md.contains("docker pull nginx"));
        assert!(md.contains("| `latest` | 5.0MB | amd64, arm64 | 2024-01-01 |"));
    }

    #[test]
    fn a_namespaced_image_prefixes_the_owner() {
        let repo = json!({ "name": "grafana" });
        let md = render("grafana", &repo, &[]);
        assert!(md.contains("# grafana/grafana"));
        assert!(md.contains("docker pull grafana/grafana"));
    }
}
