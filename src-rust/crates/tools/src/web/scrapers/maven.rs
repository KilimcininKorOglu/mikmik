// Maven Central handler: renders an artifact via the Solr search API.
// Recognizes search.maven.org and mvnrepository.com artifact URLs.

use super::util::{
    build_result, format_epoch_millis, format_number, load_page, percent_encode_component,
    LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct MavenHandler;

/// The `(groupId, artifactId, version?)` from a recognized Maven URL.
struct Coords {
    group: String,
    artifact: String,
    version: Option<String>,
}

fn parse_coords(url: &str) -> Option<Coords> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let known = matches!(
        host,
        "search.maven.org" | "mvnrepository.com" | "www.mvnrepository.com"
    );
    if !known {
        return None;
    }
    let rest = parsed.path().strip_prefix("/artifact/")?;
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let group = parts.next()?.to_string();
    let artifact = parts.next()?.to_string();
    let version = parts.next().map(str::to_string);
    Some(Coords {
        group,
        artifact,
        version,
    })
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn append_snippets(md: &mut String, group: &str, artifact: &str, version: &str) {
    let _ = write!(
        md,
        "\n## Maven Dependency\n\n```xml\n<dependency>\n    <groupId>{group}</groupId>\n    <artifactId>{artifact}</artifactId>\n    <version>{version}</version>\n</dependency>\n```\n"
    );
    let _ = write!(
        md,
        "\n## Gradle Dependency\n\n```groovy\nimplementation '{group}:{artifact}:{version}'\n```\n"
    );
    let _ = write!(
        md,
        "\n## Gradle (Kotlin DSL)\n\n```kotlin\nimplementation(\"{group}:{artifact}:{version}\")\n```\n"
    );
}

fn append_extensions(md: &mut String, doc: &Value) {
    let Some(ec) = doc.get("ec").and_then(Value::as_array) else {
        return;
    };
    let extensions: Vec<&str> = ec
        .iter()
        .filter_map(Value::as_str)
        .filter(|e| !e.is_empty() && *e != "-")
        .collect();
    if extensions.is_empty() {
        return;
    }
    md.push_str("\n## Available Extensions\n\n");
    for e in extensions {
        let _ = writeln!(md, "- {e}");
    }
}

fn render(doc: &Value, requested: Option<&str>) -> String {
    let group = str_field(doc, "g").unwrap_or("");
    let artifact = str_field(doc, "a").unwrap_or("");
    let latest = str_field(doc, "latestVersion").unwrap_or("unknown");
    let display_version = requested.unwrap_or(latest);

    let mut md = format!("# {group}:{artifact}\n\n");
    let _ = writeln!(md, "**Group ID:** {group}");
    let _ = writeln!(md, "**Artifact ID:** {artifact}");
    let _ = write!(md, "**Latest Version:** {latest}");
    if let Some(v) = requested {
        if v != latest {
            let _ = write!(md, " (viewing {v})");
        }
    }
    md.push('\n');
    if let Some(packaging) = str_field(doc, "p") {
        let _ = writeln!(md, "**Packaging:** {packaging}");
    }
    if let Some(count) = doc.get("versionCount").and_then(Value::as_u64) {
        let _ = writeln!(md, "**Versions:** {}", format_number(count));
    }
    if let Some(ts) = doc.get("timestamp").and_then(Value::as_i64) {
        let _ = writeln!(md, "**Last Updated:** {}", format_epoch_millis(ts));
    }
    append_snippets(&mut md, group, artifact, display_version);
    append_extensions(&mut md, doc);
    let _ = write!(
        md,
        "\n## Links\n\n- [Maven Central](https://search.maven.org/artifact/{group}/{artifact}/{display_version}/jar)\n- [MVN Repository](https://mvnrepository.com/artifact/{group}/{artifact}/{display_version})\n"
    );
    md
}

#[async_trait]
impl SpecialHandler for MavenHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let coords = parse_coords(url)?;
        let api_url = format!(
            "https://search.maven.org/solrsearch/select?q=g:{}+AND+a:{}&wt=json&rows=1",
            percent_encode_component(&coords.group),
            percent_encode_component(&coords.artifact)
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
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let doc = data
            .get("response")?
            .get("docs")
            .and_then(Value::as_array)?
            .first()?;
        let md = render(doc, coords.version.as_deref());
        Some(build_result(
            &md,
            url,
            "maven",
            vec!["Fetched via Maven Central API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_coords_reads_group_artifact_version() {
        let c =
            parse_coords("https://mvnrepository.com/artifact/com.google.guava/guava/33.0.0-jre")
                .expect("coords");
        assert_eq!(c.group, "com.google.guava");
        assert_eq!(c.artifact, "guava");
        assert_eq!(c.version.as_deref(), Some("33.0.0-jre"));
        assert!(parse_coords("https://example.com/artifact/a/b").is_none());
        assert!(parse_coords("https://search.maven.org/search").is_none());
    }

    #[test]
    fn render_lays_out_coordinates_and_snippets() {
        let doc = json!({
            "g": "com.google.guava",
            "a": "guava",
            "latestVersion": "33.0.0-jre",
            "p": "jar",
            "versionCount": 50,
            "timestamp": 1_609_459_200_000_i64,
            "ec": ["-sources.jar", "-javadoc.jar", "-"]
        });
        let md = render(&doc, None);
        assert!(md.contains("# com.google.guava:guava"));
        assert!(md.contains("**Latest Version:** 33.0.0-jre"));
        assert!(md.contains("**Versions:** 50"));
        assert!(md.contains("**Last Updated:** 2021-01-01"));
        assert!(md.contains("implementation 'com.google.guava:guava:33.0.0-jre'"));
        assert!(md.contains("- -sources.jar"));
        assert!(!md.contains("- -\n"));
    }

    #[test]
    fn render_marks_a_requested_older_version() {
        let doc = json!({ "g": "g", "a": "a", "latestVersion": "2.0" });
        let md = render(&doc, Some("1.0"));
        assert!(md.contains("**Latest Version:** 2.0 (viewing 1.0)"));
        assert!(md.contains("implementation 'g:a:1.0'"));
    }
}
