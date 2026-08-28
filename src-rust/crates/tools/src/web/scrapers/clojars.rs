// Clojars handler: renders a Clojure/Java artifact via the Clojars API.

use super::util::{
    build_result, format_number, load_page, percent_encode_component, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct ClojarsHandler;

/// The API URL for a `/group/artifact` or `/artifact` path.
fn api_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "clojars.org" && host != "www.clojars.org" {
        return None;
    }
    let segments: Vec<String> = parsed
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(super::util::percent_decode)
        .collect();
    match segments.as_slice() {
        [artifact] => Some(format!(
            "https://clojars.org/api/artifacts/{}",
            percent_encode_component(artifact)
        )),
        [group, artifact] => Some(format!(
            "https://clojars.org/api/artifacts/{}/{}",
            percent_encode_component(group),
            percent_encode_component(artifact)
        )),
        _ => None,
    }
}

fn as_str(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn first_str(data: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| data.get(k).and_then(as_str))
}

fn first_num(data: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|k| data.get(k).and_then(Value::as_u64))
}

fn format_licenses(value: &Value) -> Vec<String> {
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|license| {
            if let Some(s) = as_str(license) {
                return Some(s);
            }
            let name = license.get("name").and_then(as_str);
            let url = license.get("url").and_then(as_str);
            match (name, url) {
                (Some(n), Some(u)) => Some(format!("{n} ({u})")),
                (Some(n), None) => Some(n),
                (None, Some(u)) => Some(u),
                (None, None) => None,
            }
        })
        .collect()
}

fn dep_from_object(dep: &Value) -> Option<String> {
    let name = first_str(dep, &["name", "artifact", "jar_name"])?;
    match dep.get("version").and_then(as_str) {
        Some(v) => Some(format!("{name}: {v}")),
        None => Some(name),
    }
}

fn format_dependencies(value: &Value) -> Vec<String> {
    if let Some(arr) = value.as_array() {
        return arr
            .iter()
            .filter_map(|dep| {
                if let Some(s) = as_str(dep) {
                    return Some(s);
                }
                if let Some(pair) = dep.as_array() {
                    let name = pair.first().and_then(as_str)?;
                    return match pair.get(1).and_then(as_str) {
                        Some(v) => Some(format!("{name}: {v}")),
                        None => Some(name),
                    };
                }
                dep_from_object(dep)
            })
            .collect();
    }
    if let Some(map) = value.as_object() {
        return map
            .iter()
            .map(|(name, version)| match as_str(version) {
                Some(v) => format!("{name}: {v}"),
                None => name.clone(),
            })
            .collect();
    }
    Vec::new()
}

fn display_name(group: Option<&str>, artifact: Option<&str>) -> String {
    match (group, artifact) {
        (Some(g), Some(a)) if g != a => format!("{g}/{a}"),
        (_, Some(a)) => a.to_string(),
        (Some(g), None) => g.to_string(),
        (None, None) => "Clojars artifact".to_string(),
    }
}

fn render(data: &Value) -> String {
    let group = first_str(data, &["group_name", "group"]);
    let artifact = first_str(data, &["jar_name", "artifact", "name"]);
    let version = first_str(data, &["latest_version", "version"]);
    let description = first_str(data, &["description", "summary"]);
    let downloads = first_num(data, &["downloads", "downloads_total", "total_downloads"]);
    let homepage = first_str(data, &["homepage", "url"]);
    let licenses = format_licenses(data.get("licenses").unwrap_or(&Value::Null));
    let deps = format_dependencies(
        data.get("dependencies")
            .or_else(|| data.get("deps"))
            .unwrap_or(&Value::Null),
    );

    let mut md = format!(
        "# {}\n\n",
        display_name(group.as_deref(), artifact.as_deref())
    );
    if let Some(desc) = &description {
        let _ = write!(md, "{desc}\n\n");
    }
    if let Some(g) = &group {
        let _ = writeln!(md, "**Group:** {g}");
    }
    if let Some(a) = &artifact {
        let _ = writeln!(md, "**Artifact:** {a}");
    }
    if let Some(v) = &version {
        let _ = writeln!(md, "**Latest:** {v}");
    }
    if let Some(d) = downloads {
        let _ = writeln!(md, "**Downloads:** {}", format_number(d));
    }
    if let Some(h) = &homepage {
        let _ = writeln!(md, "**Homepage:** {h}");
    }
    if !licenses.is_empty() {
        let _ = writeln!(md, "**Licenses:** {}", licenses.join(", "));
    }
    if !deps.is_empty() {
        md.push_str("\n## Dependencies\n\n");
        for dep in deps {
            let _ = writeln!(md, "- {dep}");
        }
    }
    md
}

#[async_trait]
impl SpecialHandler for ClojarsHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let api = api_url(url)?;
        let result = load_page(
            &api,
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
        let payload: Value = serde_json::from_str(&result.content).ok()?;
        let data = match &payload {
            Value::Array(items) => items.first()?.clone(),
            other => other.clone(),
        };
        if !data.is_object() {
            return None;
        }
        let md = render(&data);
        Some(build_result(
            &md,
            url,
            "clojars",
            vec!["Fetched via Clojars API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_url_maps_one_and_two_segment_paths() {
        assert_eq!(
            api_url("https://clojars.org/ring").as_deref(),
            Some("https://clojars.org/api/artifacts/ring")
        );
        assert_eq!(
            api_url("https://clojars.org/metosin/reitit").as_deref(),
            Some("https://clojars.org/api/artifacts/metosin/reitit")
        );
        assert!(api_url("https://example.com/ring").is_none());
    }

    #[test]
    fn dependencies_accept_array_pair_object_and_map() {
        assert_eq!(
            format_dependencies(&json!(["a", ["b", "1.0"], { "name": "c", "version": "2.0" }])),
            vec!["a", "b: 1.0", "c: 2.0"]
        );
        assert_eq!(
            format_dependencies(&json!({ "d": "3.0", "e": "4.0" })),
            vec!["d: 3.0", "e: 4.0"]
        );
    }

    #[test]
    fn render_lays_out_artifact() {
        let data = json!({
            "group_name": "metosin",
            "jar_name": "reitit",
            "latest_version": "0.7.0",
            "description": "A fast router",
            "downloads": 1_500_000,
            "homepage": "https://github.com/metosin/reitit",
            "licenses": [{ "name": "EPL", "url": "https://epl.org" }],
            "dependencies": [["org.clojure/clojure", "1.11.0"]]
        });
        let md = render(&data);
        assert!(md.contains("# metosin/reitit"));
        assert!(md.contains("**Latest:** 0.7.0"));
        assert!(md.contains("**Downloads:** 1,500,000"));
        assert!(md.contains("**Licenses:** EPL (https://epl.org)"));
        assert!(md.contains("- org.clojure/clojure: 1.11.0"));
    }
}
