// RubyGems handler: renders a rubygems.org gem page from the API.

use super::util::{
    build_result, format_number, load_page, percent_decode, percent_encode_component, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct RubyGemsHandler;

fn gem_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "rubygems.org" && host != "www.rubygems.org" {
        return None;
    }
    let rest = parsed.path().strip_prefix("/gems/")?;
    let name = rest.split('/').next().filter(|s| !s.is_empty())?;
    Some(percent_decode(name))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn append_deps(md: &mut String, gem: &Value, kind: &str, heading: &str) {
    let deps = gem
        .get("dependencies")
        .and_then(|d| d.get(kind))
        .and_then(Value::as_array);
    let Some(deps) = deps.filter(|d| !d.is_empty()) else {
        return;
    };
    let _ = write!(md, "\n## {heading}\n\n");
    for dep in deps {
        let name = str_field(dep, "name").unwrap_or("?");
        let req = str_field(dep, "requirements").unwrap_or("");
        let _ = writeln!(md, "- {name} {req}");
    }
}

fn render(gem: &Value) -> String {
    let name = str_field(gem, "name").unwrap_or("(gem)");
    let mut md = format!("# {name}\n\n");
    if let Some(info) = str_field(gem, "info") {
        let _ = write!(md, "{info}\n\n");
    }
    let version = str_field(gem, "version").unwrap_or("unknown");
    let _ = write!(md, "**Version:** {version}");
    let licenses = str_list(gem, "licenses");
    if !licenses.is_empty() {
        let _ = write!(md, " · **License:** {}", licenses.join(", "));
    }
    md.push('\n');
    let downloads = gem.get("downloads").and_then(Value::as_u64).unwrap_or(0);
    let _ = write!(md, "**Total Downloads:** {}", format_number(downloads));
    if let Some(vd) = gem.get("version_downloads").and_then(Value::as_u64) {
        let _ = write!(md, " · **Version Downloads:** {}", format_number(vd));
    }
    md.push_str("\n\n");
    for (label, key) in [
        ("Homepage", "homepage_uri"),
        ("Source Code", "source_code_uri"),
        ("Documentation", "documentation_uri"),
        ("Authors", "authors"),
    ] {
        if let Some(value) = str_field(gem, key) {
            let _ = writeln!(md, "**{label}:** {value}");
        }
    }
    append_deps(&mut md, gem, "runtime", "Runtime Dependencies");
    append_deps(&mut md, gem, "development", "Development Dependencies");
    md
}

#[async_trait]
impl SpecialHandler for RubyGemsHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let name = gem_name(url)?;
        let api_url = format!(
            "https://rubygems.org/api/v1/gems/{}.json",
            percent_encode_component(&name)
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
        let gem: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&gem);
        Some(build_result(
            &md,
            url,
            "rubygems",
            vec!["Fetched via RubyGems API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gem_name_parses_and_rejects_other_hosts() {
        assert_eq!(
            gem_name("https://rubygems.org/gems/rails").as_deref(),
            Some("rails")
        );
        assert!(gem_name("https://example.com/gems/rails").is_none());
    }

    #[test]
    fn render_lays_out_gem_and_dependencies() {
        let gem = json!({
            "name": "rails",
            "info": "Web framework",
            "version": "7.1.0",
            "licenses": ["MIT"],
            "downloads": 500_000_000,
            "version_downloads": 1_000_000,
            "homepage_uri": "https://rubyonrails.org",
            "dependencies": { "runtime": [{ "name": "actionpack", "requirements": "= 7.1.0" }] }
        });
        let md = render(&gem);
        assert!(md.contains("# rails"));
        assert!(md.contains("**Version:** 7.1.0 · **License:** MIT"));
        assert!(md.contains("**Total Downloads:** 500,000,000 · **Version Downloads:** 1,000,000"));
        assert!(md.contains("**Homepage:** https://rubyonrails.org"));
        assert!(md.contains("## Runtime Dependencies\n\n- actionpack = 7.1.0"));
    }
}
