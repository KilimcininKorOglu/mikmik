// pub.dev handler: renders a Dart/Flutter package via the JSON API.
//
// The optional HTML README fetch that omp performs needs a DOM parser and is
// deferred to the HTML-parse scraper phase; the JSON path below carries the
// description, version, metrics, links and dependencies.

use super::util::{
    build_result, format_number, load_page, percent_encode_component, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct PubDevHandler;

fn package_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "pub.dev" && host != "www.pub.dev" {
        return None;
    }
    let rest = parsed.path().strip_prefix("/packages/")?;
    let name = rest.split('/').next().filter(|s| !s.is_empty())?;
    Some(super::util::percent_decode(name))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn append_metrics(md: &mut String, data: &Value) {
    let Some(score) = data.get("metrics").and_then(|m| m.get("score")) else {
        return;
    };
    let mut wrote = false;
    if let Some(likes) = score.get("likeCount").and_then(Value::as_u64) {
        let _ = write!(md, "**Likes:** {}", format_number(likes));
        wrote = true;
    }
    let points = score.get("grantedPoints").and_then(Value::as_u64);
    let max = score.get("maxPoints").and_then(Value::as_u64);
    if let (Some(p), Some(m)) = (points, max) {
        let _ = write!(md, " · **Pub Points:** {p}/{m}");
        wrote = true;
    }
    if let Some(pop) = score.get("popularityScore").and_then(Value::as_f64) {
        let _ = write!(md, " · **Popularity:** {}%", (pop * 100.0).round() as i64);
        wrote = true;
    }
    if wrote {
        md.push('\n');
    }
}

fn append_dependencies(md: &mut String, pubspec: &Value) {
    let Some(deps) = pubspec.get("dependencies").and_then(Value::as_object) else {
        return;
    };
    if deps.is_empty() {
        return;
    }
    let _ = write!(md, "## Dependencies ({})\n\n", deps.len());
    for (dep, constraint) in deps.iter().take(20) {
        let text = match constraint {
            Value::String(s) => s.clone(),
            Value::Object(_) | Value::Array(_) => "complex".to_string(),
            _ => String::new(),
        };
        if text.is_empty() {
            let _ = writeln!(md, "- {dep}");
        } else {
            let _ = writeln!(md, "- {dep}: {text}");
        }
    }
    if deps.len() > 20 {
        let _ = write!(md, "\n[…{} dependencies elided…]\n", deps.len() - 20);
    }
    md.push('\n');
}

fn render(data: &Value) -> String {
    let name = str_field(data, "name").unwrap_or("(package)");
    let latest = data.get("latest").cloned().unwrap_or(Value::Null);
    let pubspec = latest.get("pubspec").cloned().unwrap_or(Value::Null);

    let mut md = format!("# {name}\n\n");
    if let Some(desc) = str_field(&pubspec, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    let version = str_field(&latest, "version").unwrap_or("unknown");
    let _ = write!(md, "**Latest:** {version}");
    if let Some(publisher) = str_field(data, "publisherId") {
        let _ = write!(md, " · **Publisher:** {publisher}");
    }
    md.push('\n');
    append_metrics(&mut md, data);
    md.push('\n');

    for (label, key) in [
        ("Homepage", "homepage"),
        ("Repository", "repository"),
        ("Documentation", "documentation"),
    ] {
        if let Some(value) = str_field(&pubspec, key) {
            let _ = writeln!(md, "**{label}:** {value}");
        }
    }
    if let Some(env) = pubspec.get("environment").and_then(Value::as_object) {
        let constraints: Vec<String> = env
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| format!("{k}: {s}")))
            .collect();
        if !constraints.is_empty() {
            let _ = writeln!(md, "**SDK:** {}", constraints.join(", "));
        }
    }
    md.push('\n');
    append_dependencies(&mut md, &pubspec);
    md
}

#[async_trait]
impl SpecialHandler for PubDevHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let name = package_name(url)?;
        let api_url = format!(
            "https://pub.dev/api/packages/{}",
            percent_encode_component(&name)
        );
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&data);
        Some(build_result(
            &md,
            url,
            "pub.dev",
            vec!["Fetched via pub.dev API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn package_name_parses_and_rejects_other_hosts() {
        assert_eq!(
            package_name("https://pub.dev/packages/provider").as_deref(),
            Some("provider")
        );
        assert!(package_name("https://example.com/packages/x").is_none());
    }

    #[test]
    fn render_lays_out_package_metrics_and_deps() {
        let data = json!({
            "name": "provider",
            "publisherId": "dash-overflow.net",
            "latest": {
                "version": "6.1.1",
                "pubspec": {
                    "description": "State management",
                    "repository": "https://github.com/rrousselGit/provider",
                    "environment": { "sdk": ">=3.0.0 <4.0.0" },
                    "dependencies": { "flutter": "sdk", "collection": "^1.17.0" }
                }
            },
            "metrics": { "score": { "likeCount": 5000, "grantedPoints": 140, "maxPoints": 140, "popularityScore": 0.99 } }
        });
        let md = render(&data);
        assert!(md.contains("# provider"));
        assert!(md.contains("**Latest:** 6.1.1 · **Publisher:** dash-overflow.net"));
        assert!(md.contains("**Likes:** 5,000 · **Pub Points:** 140/140 · **Popularity:** 99%"));
        assert!(md.contains("**Repository:** https://github.com/rrousselGit/provider"));
        assert!(md.contains("**SDK:** sdk: >=3.0.0 <4.0.0"));
        assert!(md.contains("## Dependencies (2)"));
        assert!(md.contains("- collection: ^1.17.0"));
    }
}
