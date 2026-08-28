// AUR handler: renders an Arch User Repository package via the RPC API.

use super::util::{
    build_result, format_epoch_millis, format_number, load_page, percent_encode_component,
    LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct AurHandler;

fn package_name(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "aur.archlinux.org" {
        return None;
    }
    let rest = parsed.path().strip_prefix("/packages/")?;
    let name = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|s| !s.is_empty())?;
    Some(super::util::percent_decode(name))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn str_list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// A `## Heading\n\n- item` block, with an optional count in the heading.
fn append_list(md: &mut String, heading: &str, items: &[&str], with_count: bool) {
    if items.is_empty() {
        return;
    }
    if with_count {
        let _ = write!(md, "\n## {heading} ({})\n\n", items.len());
    } else {
        let _ = write!(md, "\n## {heading}\n\n");
    }
    for item in items {
        let _ = writeln!(md, "- {item}");
    }
}

fn epoch_date(pkg: &Value, key: &str) -> String {
    pkg.get(key)
        .and_then(Value::as_i64)
        .map(|s| format_epoch_millis(s * 1000))
        .unwrap_or_default()
}

fn append_header(md: &mut String, pkg: &Value) {
    let version = str_field(pkg, "Version").unwrap_or("unknown");
    let _ = write!(md, "**Version:** {version}");
    if let Some(ood) = pkg.get("OutOfDate").and_then(Value::as_i64) {
        let _ = write!(
            md,
            " (flagged out-of-date: {})",
            format_epoch_millis(ood * 1000)
        );
    }
    md.push('\n');
    match str_field(pkg, "Maintainer") {
        Some(m) => {
            let _ = writeln!(
                md,
                "**Maintainer:** [{m}](https://aur.archlinux.org/account/{m})"
            );
        }
        None => md.push_str("**Maintainer:** Orphaned\n"),
    }
    let votes = pkg.get("NumVotes").and_then(Value::as_u64).unwrap_or(0);
    let pop = pkg.get("Popularity").and_then(Value::as_f64).unwrap_or(0.0);
    let _ = writeln!(
        md,
        "**Votes:** {} · **Popularity:** {pop:.2}",
        format_number(votes)
    );
    let _ = writeln!(
        md,
        "**Last Updated:** {} · **First Submitted:** {}",
        epoch_date(pkg, "LastModified"),
        epoch_date(pkg, "FirstSubmitted")
    );
    let licenses = str_list(pkg, "License");
    if !licenses.is_empty() {
        let _ = writeln!(md, "**License:** {}", licenses.join(", "));
    }
    if let Some(upstream) = str_field(pkg, "URL") {
        let _ = writeln!(md, "**Upstream:** {upstream}");
    }
    let keywords = str_list(pkg, "Keywords");
    if !keywords.is_empty() {
        let _ = writeln!(md, "**Keywords:** {}", keywords.join(", "));
    }
}

fn render(pkg: &Value) -> String {
    let name = str_field(pkg, "Name").unwrap_or("(package)");
    let mut md = format!("# {name}\n\n");
    if let Some(desc) = str_field(pkg, "Description") {
        let _ = write!(md, "{desc}\n\n");
    }
    append_header(&mut md, pkg);
    append_list(&mut md, "Dependencies", &str_list(pkg, "Depends"), true);
    append_list(
        &mut md,
        "Make Dependencies",
        &str_list(pkg, "MakeDepends"),
        true,
    );
    append_list(
        &mut md,
        "Optional Dependencies",
        &str_list(pkg, "OptDepends"),
        false,
    );
    append_list(
        &mut md,
        "Check Dependencies",
        &str_list(pkg, "CheckDepends"),
        false,
    );
    append_list(&mut md, "Provides", &str_list(pkg, "Provides"), false);
    append_list(&mut md, "Conflicts", &str_list(pkg, "Conflicts"), false);
    append_list(&mut md, "Replaces", &str_list(pkg, "Replaces"), false);

    let base = str_field(pkg, "PackageBase").unwrap_or(name);
    let _ = write!(
        md,
        "\n---\n\n## Installation\n\n```bash\n# Using an AUR helper (e.g., yay, paru)\nyay -S {name}\n\n# Manual installation\ngit clone https://aur.archlinux.org/{base}.git\ncd {base}\nmakepkg -si\n```\n"
    );
    md
}

#[async_trait]
impl SpecialHandler for AurHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let name = package_name(url)?;
        let api_url = format!(
            "https://aur.archlinux.org/rpc/?v=5&type=info&arg={}",
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
        let pkg = data
            .get("results")
            .and_then(Value::as_array)?
            .first()?
            .clone();
        let md = render(&pkg);
        Some(build_result(
            &md,
            url,
            "aur",
            vec!["Fetched via AUR RPC API".to_string()],
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
            package_name("https://aur.archlinux.org/packages/yay").as_deref(),
            Some("yay")
        );
        assert_eq!(
            package_name("https://aur.archlinux.org/packages/yay?comments=all").as_deref(),
            Some("yay")
        );
        assert!(package_name("https://example.com/packages/yay").is_none());
    }

    #[test]
    fn render_lays_out_metadata_deps_and_install() {
        let pkg = json!({
            "Name": "yay",
            "Description": "Yet another yogurt",
            "Version": "12.0.0-1",
            "Maintainer": "Jguer",
            "NumVotes": 2000,
            "Popularity": 15.678,
            "LastModified": 1_609_459_200_i64,
            "FirstSubmitted": 1_500_000_000_i64,
            "License": ["GPL3"],
            "URL": "https://github.com/Jguer/yay",
            "Depends": ["pacman", "git"],
            "OptDepends": ["sudo"],
            "PackageBase": "yay"
        });
        let md = render(&pkg);
        assert!(md.contains("# yay"));
        assert!(md.contains("**Version:** 12.0.0-1"));
        assert!(md.contains("**Maintainer:** [Jguer](https://aur.archlinux.org/account/Jguer)"));
        assert!(md.contains("**Votes:** 2,000 · **Popularity:** 15.68"));
        assert!(md.contains("**Last Updated:** 2021-01-01"));
        assert!(md.contains("## Dependencies (2)\n\n- pacman\n- git"));
        assert!(md.contains("## Optional Dependencies\n\n- sudo"));
        assert!(md.contains("yay -S yay"));
        assert!(md.contains("git clone https://aur.archlinux.org/yay.git"));
    }

    #[test]
    fn an_orphaned_package_says_so() {
        let pkg = json!({ "Name": "x", "Version": "1", "PackageBase": "x" });
        let md = render(&pkg);
        assert!(md.contains("**Maintainer:** Orphaned"));
    }
}
