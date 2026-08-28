// MetaCPAN handler: renders a Perl module or release via fastapi.metacpan.org.

use super::util::{
    build_result, format_epoch_millis, load_page, percent_decode, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct MetaCpanHandler;

/// Which MetaCPAN resource a URL names.
enum Target {
    /// `/pod/Module::Name`
    Module(String),
    /// `/release/AUTHOR/Distribution` or `/release/Distribution`
    Release(String),
}

fn parse_target(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "metacpan.org" && host != "www.metacpan.org" {
        return None;
    }
    let path = parsed.path();
    if let Some(rest) = path.strip_prefix("/pod/") {
        let name = rest.split('/').next().filter(|s| !s.is_empty())?;
        return Some(Target::Module(percent_decode(name)));
    }
    if let Some(rest) = path.strip_prefix("/release/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        let dist = match parts.as_slice() {
            [dist] => *dist,
            [_author, dist, ..] => *dist,
            _ => return None,
        };
        return Some(Target::Release(percent_decode(dist)));
    }
    None
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn licenses(release: &Value) -> Vec<String> {
    release
        .get("license")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn append_resources(md: &mut String, release: &Value) {
    let resources = release
        .get("metadata")
        .and_then(|m| m.get("resources"))
        .cloned()
        .unwrap_or(Value::Null);
    let repo = resources
        .get("repository")
        .and_then(|r| str_field(r, "web").or_else(|| str_field(r, "url")));
    if let Some(repo) = repo {
        let _ = writeln!(md, "**Repository:** {repo}");
    }
    if let Some(home) = str_field(&resources, "homepage") {
        let _ = writeln!(md, "**Homepage:** {home}");
    }
    if let Some(bug) = resources
        .get("bugtracker")
        .and_then(|b| str_field(b, "web"))
    {
        let _ = writeln!(md, "**Issues:** {bug}");
    }
}

fn append_dependencies(md: &mut String, release: &Value) {
    let Some(deps) = release.get("dependency").and_then(Value::as_array) else {
        return;
    };
    let runtime: Vec<&Value> = deps
        .iter()
        .filter(|d| {
            d.get("phase").and_then(Value::as_str) == Some("runtime")
                && d.get("relationship").and_then(Value::as_str) == Some("requires")
                && d.get("module").and_then(Value::as_str) != Some("perl")
        })
        .collect();
    if runtime.is_empty() {
        return;
    }
    md.push_str("\n## Dependencies\n\n");
    for dep in runtime.iter().take(20) {
        let module = str_field(dep, "module").unwrap_or("?");
        let _ = write!(md, "- **{module}**");
        if let Some(v) = str_field(dep, "version") {
            if v != "0" {
                let _ = write!(md, " >= {v}");
            }
        }
        md.push('\n');
    }
    if runtime.len() > 20 {
        let _ = write!(md, "\n[…{} dependencies elided…]\n", runtime.len() - 20);
    }
}

fn render_module(module: &Value, release: Option<&Value>) -> String {
    let name = str_field(module, "name").unwrap_or("(module)");
    let mut md = format!("# {name}\n\n");
    if let Some(abstract_) = str_field(module, "abstract") {
        let _ = write!(md, "{abstract_}\n\n");
    }
    let version = str_field(module, "version").unwrap_or("unknown");
    let dist = str_field(module, "distribution").unwrap_or("");
    let author = str_field(module, "author").unwrap_or("");
    let _ = writeln!(
        md,
        "**Version:** {version} · **Distribution:** {dist} · **Author:** [{author}](https://metacpan.org/author/{author})"
    );
    if let Some(release) = release {
        let ls = licenses(release);
        if !ls.is_empty() {
            let _ = writeln!(md, "**License:** {}", ls.join(", "));
        }
        append_resources(&mut md, release);
        append_dependencies(&mut md, release);
    }
    let _ = write!(md, "\n## Installation\n\n```bash\ncpanm {name}\n```\n");
    md
}

fn render_release(release: &Value) -> String {
    let dist = str_field(release, "distribution").unwrap_or("(distribution)");
    let mut md = format!("# {dist}\n\n");
    if let Some(abstract_) = str_field(release, "abstract") {
        let _ = write!(md, "{abstract_}\n\n");
    }
    let version = str_field(release, "version").unwrap_or("unknown");
    let author = str_field(release, "author").unwrap_or("");
    let _ = writeln!(
        md,
        "**Version:** {version} · **Author:** [{author}](https://metacpan.org/author/{author})"
    );
    let ls = licenses(release);
    if !ls.is_empty() {
        let _ = writeln!(md, "**License:** {}", ls.join(", "));
    }
    if let Some(mtime) = release
        .get("stat")
        .and_then(|s| s.get("mtime"))
        .and_then(Value::as_i64)
    {
        let _ = writeln!(md, "**Released:** {}", format_epoch_millis(mtime * 1000));
    }
    append_resources(&mut md, release);
    append_dependencies(&mut md, release);
    let _ = write!(md, "\n## Installation\n\n```bash\ncpanm {dist}\n```\n");
    md
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
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
impl SpecialHandler for MetaCpanHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let md = match parse_target(url)? {
            Target::Module(name) => {
                let module = fetch_json(
                    &format!("https://fastapi.metacpan.org/v1/module/{name}"),
                    timeout,
                )
                .await?;
                let dist = str_field(&module, "distribution").map(str::to_string);
                let release = match dist {
                    Some(dist) => {
                        fetch_json(
                            &format!("https://fastapi.metacpan.org/v1/release/{dist}"),
                            timeout.min(Duration::from_secs(5)),
                        )
                        .await
                    }
                    None => None,
                };
                render_module(&module, release.as_ref())
            }
            Target::Release(dist) => {
                let release = fetch_json(
                    &format!("https://fastapi.metacpan.org/v1/release/{dist}"),
                    timeout,
                )
                .await?;
                render_release(&release)
            }
        };
        Some(build_result(
            &md,
            url,
            "metacpan",
            vec!["Fetched via MetaCPAN API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_reads_pod_and_release_shapes() {
        assert!(matches!(
            parse_target("https://metacpan.org/pod/Moose"),
            Some(Target::Module(m)) if m == "Moose"
        ));
        assert!(matches!(
            parse_target("https://metacpan.org/release/ETHER/Moose-2.2"),
            Some(Target::Release(d)) if d == "Moose-2.2"
        ));
        assert!(matches!(
            parse_target("https://metacpan.org/release/Moose"),
            Some(Target::Release(d)) if d == "Moose"
        ));
        assert!(parse_target("https://example.com/pod/Moose").is_none());
    }

    #[test]
    fn render_module_lays_out_deps_and_install() {
        let module = json!({
            "name": "Moose",
            "abstract": "Postmodern OO",
            "version": "2.2015",
            "distribution": "Moose",
            "author": "ETHER"
        });
        let release = json!({
            "license": ["perl_5"],
            "metadata": { "resources": { "repository": { "web": "https://github.com/moose/Moose" } } },
            "dependency": [
                { "phase": "runtime", "relationship": "requires", "module": "Class::Load", "version": "0.09" },
                { "phase": "runtime", "relationship": "requires", "module": "perl", "version": "5.008" }
            ]
        });
        let md = render_module(&module, Some(&release));
        assert!(md.contains("# Moose"));
        assert!(md.contains("**Version:** 2.2015 · **Distribution:** Moose · **Author:** [ETHER]"));
        assert!(md.contains("**License:** perl_5"));
        assert!(md.contains("**Repository:** https://github.com/moose/Moose"));
        assert!(md.contains("- **Class::Load** >= 0.09"));
        assert!(!md.contains("perl** >="), "perl is filtered out");
        assert!(md.contains("cpanm Moose"));
    }

    #[test]
    fn render_release_formats_the_mtime() {
        let release = json!({
            "distribution": "Moose",
            "version": "2.2015",
            "author": "ETHER",
            "stat": { "mtime": 1_609_459_200_i64 }
        });
        let md = render_release(&release);
        assert!(md.contains("# Moose"));
        assert!(md.contains("**Released:** 2021-01-01"));
    }
}
