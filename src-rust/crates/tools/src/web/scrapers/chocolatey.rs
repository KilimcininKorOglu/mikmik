// Chocolatey handler: renders a package from the community NuGet v2 OData API.

use super::util::{
    build_result, format_iso_date, format_number, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct ChocolateyHandler;

static PACKAGE_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/packages/([^/]+)(?:/([^/]+))?").expect("static choco regex"));

/// Fields pulled from a NuGet OData entry, whether JSON or Atom XML.
#[derive(Default)]
struct Package {
    id: String,
    version: String,
    title: Option<String>,
    description: Option<String>,
    summary: Option<String>,
    authors: Option<String>,
    project_url: Option<String>,
    source_url: Option<String>,
    tags: Option<String>,
    download_count: Option<u64>,
    version_download_count: Option<u64>,
    published: Option<String>,
    license_url: Option<String>,
    release_notes: Option<String>,
    dependencies: Option<String>,
}

fn parse_package(url: &str) -> Option<(String, Option<String>)> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("chocolatey.org") {
        return None;
    }
    let caps = PACKAGE_PATH.captures(parsed.path())?;
    let name = super::util::percent_decode(&caps[1]);
    let version = caps.get(2).map(|m| super::util::percent_decode(m.as_str()));
    Some((name, version))
}

/// Read `<d:Field>value</d:Field>` from the OData Atom XML body.
fn xml_field(xml: &str, field: &str) -> Option<String> {
    let pattern = format!(r"(?is)<d:{field}[^>]*>(.*?)</d:{field}>");
    let re = Regex::new(&pattern).ok()?;
    let value = re.captures(xml)?[1].trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn opt_string(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build a package from the OData JSON envelope `{ d: { results: [...] } }`.
fn package_from_json(body: &str) -> Option<Package> {
    let data: Value = serde_json::from_str(body).ok()?;
    let entry = data.get("d")?.get("results")?.as_array()?.first()?;
    Some(Package {
        id: opt_string(entry, "Id").unwrap_or_default(),
        version: opt_string(entry, "Version").unwrap_or_default(),
        title: opt_string(entry, "Title"),
        description: opt_string(entry, "Description"),
        summary: opt_string(entry, "Summary"),
        authors: opt_string(entry, "Authors"),
        project_url: opt_string(entry, "ProjectUrl"),
        source_url: opt_string(entry, "PackageSourceUrl"),
        tags: opt_string(entry, "Tags"),
        download_count: entry.get("DownloadCount").and_then(Value::as_u64),
        version_download_count: entry.get("VersionDownloadCount").and_then(Value::as_u64),
        published: opt_string(entry, "Published"),
        license_url: opt_string(entry, "LicenseUrl"),
        release_notes: opt_string(entry, "ReleaseNotes"),
        dependencies: opt_string(entry, "Dependencies"),
    })
}

/// Build a package by scraping the OData Atom XML fields.
fn package_from_xml(xml: &str) -> Option<Package> {
    let id = xml_field(xml, "Id")?;
    Some(Package {
        id,
        version: xml_field(xml, "Version").unwrap_or_default(),
        title: xml_field(xml, "Title"),
        description: xml_field(xml, "Description"),
        summary: xml_field(xml, "Summary"),
        authors: xml_field(xml, "Authors"),
        project_url: xml_field(xml, "ProjectUrl"),
        source_url: xml_field(xml, "PackageSourceUrl"),
        tags: xml_field(xml, "Tags"),
        download_count: xml_field(xml, "DownloadCount").and_then(|v| v.parse().ok()),
        version_download_count: xml_field(xml, "VersionDownloadCount").and_then(|v| v.parse().ok()),
        published: xml_field(xml, "Published"),
        license_url: xml_field(xml, "LicenseUrl"),
        release_notes: xml_field(xml, "ReleaseNotes"),
        dependencies: xml_field(xml, "Dependencies"),
    })
}

fn append_downloads(md: &mut String, pkg: &Package) {
    if let Some(total) = pkg.download_count {
        let _ = write!(md, "**Total Downloads:** {}", format_number(total));
        if let Some(version) = pkg.version_download_count {
            let _ = write!(md, " · **Version Downloads:** {}", format_number(version));
        }
        md.push('\n');
    }
}

fn append_dependencies(md: &mut String, pkg: &Package) {
    let Some(deps) = &pkg.dependencies else {
        return;
    };
    let entries: Vec<&str> = deps.split('|').filter(|d| !d.trim().is_empty()).collect();
    if entries.is_empty() {
        return;
    }
    md.push_str("\n## Dependencies\n\n");
    for dep in entries {
        let mut parts = dep.split(':');
        if let Some(id) = parts.next().filter(|s| !s.is_empty()) {
            match parts.next().filter(|s| !s.is_empty()) {
                Some(version) => {
                    let _ = writeln!(md, "- {id}: {version}");
                }
                None => {
                    let _ = writeln!(md, "- {id}");
                }
            }
        }
    }
}

fn render(pkg: &Package, package_name: &str) -> String {
    let mut md = format!("# {}\n\n", pkg.title.as_deref().unwrap_or(&pkg.id));
    if let Some(summary) = &pkg.summary {
        let _ = write!(md, "{summary}\n\n");
    } else if let Some(desc) = &pkg.description {
        let first = desc.split("\n\n").next().unwrap_or(desc);
        let _ = write!(md, "{first}\n\n");
    }

    let _ = write!(md, "**Version:** {}", pkg.version);
    if let Some(authors) = &pkg.authors {
        let _ = write!(md, " · **Authors:** {authors}");
    }
    md.push('\n');
    append_downloads(&mut md, pkg);
    if let Some(published) = pkg.published.as_deref().map(format_iso_date) {
        if !published.is_empty() {
            let _ = writeln!(md, "**Published:** {published}");
        }
    }
    md.push('\n');
    if let Some(project) = &pkg.project_url {
        let _ = writeln!(md, "**Project URL:** {project}");
    }
    if let Some(source) = &pkg.source_url {
        let _ = writeln!(md, "**Source:** {source}");
    }
    if let Some(license) = &pkg.license_url {
        let _ = writeln!(md, "**License:** {license}");
    }
    if let Some(tags) = &pkg.tags {
        let list: Vec<&str> = tags.split_whitespace().collect();
        if !list.is_empty() {
            let _ = writeln!(md, "**Tags:** {}", list.join(", "));
        }
    }

    if let Some(desc) = &pkg.description {
        if Some(desc) != pkg.summary.as_ref() {
            let _ = write!(md, "\n## Description\n\n{desc}\n");
        }
    }
    if let Some(notes) = &pkg.release_notes {
        let _ = write!(md, "\n## Release Notes\n\n{notes}\n");
    }
    append_dependencies(&mut md, pkg);
    let _ = write!(md, "\n---\n**Install:** `choco install {package_name}`\n");
    md
}

fn fallback(url: &str, package_name: &str, message: &str, note: &str) -> RenderResult {
    let md = format!(
        "# {package_name}\n\n{message}\n\n---\n**Install:** `choco install {package_name}`\n"
    );
    build_result(&md, url, "chocolatey", vec![note.to_string()])
}

#[async_trait]
impl SpecialHandler for ChocolateyHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let (package_name, version) = parse_package(url)?;
        let encoded = super::util::percent_encode_component(&package_name);
        let mut api_url = format!(
            "https://community.chocolatey.org/api/v2/Packages()?$filter=Id%20eq%20'{encoded}'"
        );
        match &version {
            Some(v) => {
                let ev = super::util::percent_encode_component(v);
                api_url.push_str(&format!("%20and%20Version%20eq%20'{ev}'"));
            }
            None => api_url.push_str("&$orderby=Version%20desc&$top=1"),
        }

        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                headers: vec![(
                    "Accept".to_string(),
                    "application/atom+xml, application/xml".to_string(),
                )],
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return Some(fallback(
                url,
                &package_name,
                "Chocolatey package metadata is currently unavailable.",
                "Chocolatey API request failed",
            ));
        }

        let pkg = package_from_json(&result.content).or_else(|| package_from_xml(&result.content));
        let Some(pkg) = pkg else {
            return Some(fallback(
                url,
                &package_name,
                "Chocolatey package metadata could not be parsed.",
                "Chocolatey API response parsing failed",
            ));
        };
        let md = render(&pkg, &package_name);
        Some(build_result(
            &md,
            url,
            "chocolatey",
            vec!["Fetched via Chocolatey NuGet API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_package_reads_name_and_optional_version() {
        assert_eq!(
            parse_package("https://community.chocolatey.org/packages/git"),
            Some(("git".to_string(), None))
        );
        assert_eq!(
            parse_package("https://chocolatey.org/packages/git/2.40.0"),
            Some(("git".to_string(), Some("2.40.0".to_string())))
        );
        assert_eq!(parse_package("https://example.com/packages/git"), None);
    }

    #[test]
    fn xml_field_extracts_odata_values() {
        let xml = "<entry><m:properties><d:Id>git</d:Id><d:Version>2.40.0</d:Version><d:DownloadCount>500</d:DownloadCount></m:properties></entry>";
        assert_eq!(xml_field(xml, "Id"), Some("git".to_string()));
        assert_eq!(xml_field(xml, "Version"), Some("2.40.0".to_string()));
        assert_eq!(xml_field(xml, "Missing"), None);
    }

    #[test]
    fn render_lays_out_package_from_xml() {
        let xml = concat!(
            "<d:Id>git</d:Id><d:Version>2.40.0</d:Version><d:Title>Git</d:Title>",
            "<d:Summary>Version control</d:Summary><d:Authors>Git Team</d:Authors>",
            "<d:DownloadCount>1234567</d:DownloadCount><d:Published>2023-03-01T00:00:00Z</d:Published>",
            "<d:Dependencies>chocolatey:1.0|git.install:2.40.0</d:Dependencies>"
        );
        let pkg = package_from_xml(xml).expect("package");
        let md = render(&pkg, "git");
        assert!(md.contains("# Git"));
        assert!(md.contains("Version control"));
        assert!(md.contains("**Version:** 2.40.0 · **Authors:** Git Team"));
        assert!(md.contains("**Total Downloads:** 1,234,567"));
        assert!(md.contains("**Published:** 2023-03-01"));
        assert!(md.contains("- chocolatey: 1.0"));
        assert!(md.contains("- git.install: 2.40.0"));
        assert!(md.contains("**Install:** `choco install git`"));
    }
}
