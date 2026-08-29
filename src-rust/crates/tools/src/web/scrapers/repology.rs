// Repology handler: renders a project's packaging status across distributions
// via the Repology API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::time::Duration;

pub struct RepologyHandler;

static PROJECT_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/project/([^/]+)").expect("static repology regex"));

/// Status slugs in the order Repology ranks them, best first.
const STATUS_ORDER: [&str; 10] = [
    "newest",
    "unique",
    "devel",
    "rolling",
    "outdated",
    "legacy",
    "noscheme",
    "incorrect",
    "untrusted",
    "ignored",
];

fn parse_project(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "repology.org" && host != "www.repology.org" {
        return None;
    }
    let raw = &PROJECT_PATH.captures(parsed.path())?[1];
    Some(super::util::percent_decode(raw))
}

fn status_indicator(status: &str) -> &'static str {
    match status {
        "newest" => "✅",
        "devel" => "🚧",
        "unique" => "🔵",
        "outdated" => "🔴",
        "legacy" => "⚠\u{fe0f}",
        "rolling" => "🔄",
        _ => "➖",
    }
}

fn status_priority(status: &str) -> usize {
    STATUS_ORDER
        .iter()
        .position(|s| *s == status)
        .unwrap_or(STATUS_ORDER.len())
}

/// Map a Repology repo slug to a friendly name, else title-case it.
fn prettify_repo(repo: &str) -> String {
    const MAPPING: [(&str, &str); 29] = [
        ("arch", "Arch Linux"),
        ("aur", "AUR"),
        ("debian_unstable", "Debian Unstable"),
        ("debian_stable", "Debian Stable"),
        ("ubuntu_24_04", "Ubuntu 24.04"),
        ("ubuntu_22_04", "Ubuntu 22.04"),
        ("fedora_rawhide", "Fedora Rawhide"),
        ("fedora_40", "Fedora 40"),
        ("gentoo", "Gentoo"),
        ("nix_unstable", "Nixpkgs Unstable"),
        ("nix_stable", "Nixpkgs Stable"),
        ("homebrew", "Homebrew"),
        ("macports", "MacPorts"),
        ("alpine_edge", "Alpine Edge"),
        ("freebsd", "FreeBSD"),
        ("openbsd", "OpenBSD"),
        ("void_x86_64", "Void Linux"),
        ("opensuse_tumbleweed", "openSUSE Tumbleweed"),
        ("msys2_mingw", "MSYS2"),
        ("chocolatey", "Chocolatey"),
        ("winget", "Winget"),
        ("scoop", "Scoop"),
        ("conda_main", "Conda"),
        ("pypi", "PyPI"),
        ("crates_io", "Crates.io"),
        ("npm", "npm"),
        ("rubygems", "RubyGems"),
        ("cpan", "CPAN"),
        ("hackage", "Hackage"),
    ];
    if let Some((_, name)) = MAPPING.iter().find(|(key, _)| *key == repo) {
        return name.to_string();
    }
    if let Some((_, name)) = MAPPING.iter().find(|(key, _)| repo.starts_with(key)) {
        return name.to_string();
    }
    repo.split('_')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Fields aggregated from the flat package list before rendering.
#[derive(Default)]
struct Summary {
    newest_versions: BTreeSet<String>,
    summary: Option<String>,
    licenses: Vec<String>,
    categories: BTreeSet<String>,
    status_counts: HashMap<String, usize>,
}

fn summarize(packages: &[Value]) -> Summary {
    let mut acc = Summary::default();
    for pkg in packages {
        let status = str_field(pkg, "status").unwrap_or("");
        if let Some(version) = str_field(pkg, "version") {
            if status == "newest" || status == "unique" {
                acc.newest_versions.insert(version.to_string());
            }
        }
        if acc.summary.is_none() {
            acc.summary = str_field(pkg, "summary").map(str::to_string);
        }
        if acc.licenses.is_empty() {
            let licenses: Vec<String> = pkg
                .get("licenses")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            acc.licenses = licenses;
        }
        for cat in pkg
            .get("categories")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            acc.categories.insert(cat.to_string());
        }
        *acc.status_counts.entry(status.to_string()).or_default() += 1;
    }
    if acc.newest_versions.is_empty() {
        if let Some(version) = packages.first().and_then(|p| str_field(p, "version")) {
            acc.newest_versions.insert(version.to_string());
        }
    }
    acc
}

fn append_header(md: &mut String, project: &str, packages: &[Value], summary: &Summary) {
    let _ = write!(md, "# {project}\n\n");
    if let Some(text) = &summary.summary {
        let _ = write!(md, "{text}\n\n");
    }
    let newest: Vec<&str> = summary.newest_versions.iter().map(String::as_str).collect();
    let newest = if newest.is_empty() {
        "unknown".to_string()
    } else {
        newest.join(", ")
    };
    let _ = writeln!(md, "**Newest Version:** {newest}");
    let _ = writeln!(md, "**Repositories:** {}", packages.len());
    if !summary.licenses.is_empty() {
        let _ = writeln!(md, "**License:** {}", summary.licenses.join(", "));
    }
    if !summary.categories.is_empty() {
        let cats: Vec<&str> = summary.categories.iter().map(String::as_str).collect();
        let _ = writeln!(md, "**Categories:** {}", cats.join(", "));
    }
    md.push('\n');
}

fn append_status_summary(md: &mut String, counts: &HashMap<String, usize>) {
    md.push_str("## Version Status Summary\n\n");
    for status in STATUS_ORDER {
        if let Some(count) = counts.get(status).filter(|c| **c > 0) {
            let _ = writeln!(
                md,
                "- {} **{status}**: {count} repos",
                status_indicator(status)
            );
        }
    }
    md.push('\n');
}

fn append_table(md: &mut String, packages: &[Value]) {
    let mut sorted: Vec<&Value> = packages.iter().collect();
    sorted.sort_by(|a, b| {
        let pa = status_priority(str_field(a, "status").unwrap_or(""));
        let pb = status_priority(str_field(b, "status").unwrap_or(""));
        pa.cmp(&pb).then_with(|| {
            str_field(a, "repo")
                .unwrap_or("")
                .cmp(str_field(b, "repo").unwrap_or(""))
        })
    });

    md.push_str("## Package Versions by Repository\n\n");
    md.push_str("| Repository | Version | Status |\n");
    md.push_str("|------------|---------|--------|\n");

    let mut shown: HashSet<String> = HashSet::new();
    let mut count = 0;
    for pkg in sorted {
        let repo = str_field(pkg, "repo").unwrap_or("");
        let repo_key = match str_field(pkg, "subrepo") {
            Some(sub) => format!("{repo}/{sub}"),
            None => repo.to_string(),
        };
        if !shown.insert(repo_key) {
            continue;
        }
        let version = str_field(pkg, "origversion")
            .or_else(|| str_field(pkg, "version"))
            .unwrap_or("");
        let status = str_field(pkg, "status").unwrap_or("");
        let _ = writeln!(
            md,
            "| {} | `{version}` | {} {status} |",
            prettify_repo(repo),
            status_indicator(status)
        );
        count += 1;
        if count >= 15 {
            break;
        }
    }
    if packages.len() > 15 {
        let _ = write!(md, "\n[…{} repositories elided…]\n", packages.len() - 15);
    }
}

fn render(packages: &[Value], project: &str, url: &str) -> String {
    let summary = summarize(packages);
    let mut md = String::new();
    append_header(&mut md, project, packages, &summary);
    append_status_summary(&mut md, &summary.status_counts);
    append_table(&mut md, packages);
    let _ = write!(md, "\n---\n\n[View on Repology]({url})\n");
    md
}

#[async_trait]
impl SpecialHandler for RepologyHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let project = parse_project(url)?;
        let api_url = format!(
            "https://repology.org/api/v1/project/{}",
            super::util::percent_encode_component(&project)
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
        let packages: Vec<Value> = serde_json::from_str(&result.content).ok()?;
        if packages.is_empty() {
            return None;
        }
        let md = render(&packages, &project, url);
        Some(build_result(
            &md,
            url,
            "repology",
            vec!["Fetched via Repology API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_project_reads_name() {
        assert_eq!(
            parse_project("https://repology.org/project/ripgrep/versions"),
            Some("ripgrep".to_string())
        );
        assert_eq!(parse_project("https://example.com/project/x"), None);
    }

    #[test]
    fn prettify_repo_maps_and_titlecases() {
        assert_eq!(prettify_repo("arch"), "Arch Linux");
        assert_eq!(prettify_repo("nix_unstable"), "Nixpkgs Unstable");
        assert_eq!(prettify_repo("some_distro"), "Some Distro");
    }

    #[test]
    fn render_summarizes_status_and_table() {
        let packages = vec![
            json!({ "repo": "arch", "version": "14.1.0", "status": "newest", "summary": "Fast grep", "licenses": ["MIT"] }),
            json!({ "repo": "debian_stable", "version": "13.0.0", "status": "outdated" }),
        ];
        let md = render(&packages, "ripgrep", "https://repology.org/project/ripgrep");
        assert!(md.contains("# ripgrep"));
        assert!(md.contains("Fast grep"));
        assert!(md.contains("**Newest Version:** 14.1.0"));
        assert!(md.contains("**Repositories:** 2"));
        assert!(md.contains("**License:** MIT"));
        assert!(md.contains("- ✅ **newest**: 1 repos"));
        assert!(md.contains("- 🔴 **outdated**: 1 repos"));
        assert!(md.contains("| Arch Linux | `14.1.0` | ✅ newest |"));
        assert!(md.contains("| Debian Stable | `13.0.0` | 🔴 outdated |"));
        assert!(md.contains("[View on Repology](https://repology.org/project/ripgrep)"));
    }
}
