// Artifact Hub handler: renders a Helm chart, operator, or policy package via
// the Artifact Hub API.

use super::util::{
    build_result, format_epoch_millis, format_number, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct ArtifactHubHandler;

static PACKAGE_PATH: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/packages/([^/]+)/([^/]+)/([^/]+)").expect("static artifacthub regex")
});

/// The `{kind, repo, name}` triple identifying a package.
struct PackageRef {
    kind: String,
    repo: String,
    name: String,
}

fn parse_package(url: &str) -> Option<PackageRef> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "artifacthub.io" && host != "www.artifacthub.io" {
        return None;
    }
    let caps = PACKAGE_PATH.captures(parsed.path())?;
    Some(PackageRef {
        kind: caps[1].to_string(),
        repo: caps[2].to_string(),
        name: caps[3].to_string(),
    })
}

/// Map a package-kind slug to a human label, else title-case the slug.
fn format_kind_label(kind: &str) -> String {
    let label = match kind {
        "helm" => "Helm Chart",
        "helm-plugin" => "Helm Plugin",
        "falco" => "Falco Rules",
        "opa" => "OPA Policy",
        "olm" => "OLM Operator",
        "tbaction" => "Tinkerbell Action",
        "krew" => "Krew Plugin",
        "tekton" => "Tekton Task",
        "tekton-pipeline" => "Tekton Pipeline",
        "keda" => "KEDA Scaler",
        "coredns" => "CoreDNS Plugin",
        "keptn" => "Keptn Integration",
        "container" => "Container Image",
        "kubewarden" => "Kubewarden Policy",
        "gatekeeper" => "Gatekeeper Policy",
        "kyverno" => "Kyverno Policy",
        "knative-client" => "Knative Client Plugin",
        "backstage" => "Backstage Plugin",
        "argo" => "Argo Template",
        "kubearmor" => "KubeArmor Policy",
        "kcl" => "KCL Module",
        "headlamp" => "Headlamp Plugin",
        "inspektor" => "Inspektor Gadget",
        "meshery-design" => "Meshery Design",
        "opencost-plugin" => "OpenCost Plugin",
        "radius" => "Radius Recipe",
        _ => return title_case(kind),
    };
    label.to_string()
}

fn title_case(slug: &str) -> String {
    let mut chars = slug.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => slug.to_string(),
    }
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

fn append_meta_line(md: &mut String, pkg: &Value, kind_label: &str) {
    let _ = write!(md, "**Type:** {kind_label}");
    if let Some(version) = str_field(pkg, "version") {
        let _ = write!(md, " · **Version:** {version}");
    }
    if let Some(app_version) = str_field(pkg, "app_version") {
        let _ = write!(md, " · **App Version:** {app_version}");
    }
    if let Some(license) = str_field(pkg, "license") {
        let _ = write!(md, " · **License:** {license}");
    }
    md.push('\n');
}

fn append_badges(md: &mut String, pkg: &Value) {
    let mut badges: Vec<String> = Vec::new();
    if pkg.get("official").and_then(Value::as_bool) == Some(true) {
        badges.push("Official".to_string());
    }
    if pkg.get("signed").and_then(Value::as_bool) == Some(true) {
        badges.push("Signed".to_string());
    }
    if let Some(stars) = pkg.get("stars").and_then(Value::as_u64).filter(|s| *s > 0) {
        badges.push(format!("{} stars", format_number(stars)));
    }
    if !badges.is_empty() {
        let _ = writeln!(md, "**{}**", badges.join(" · "));
    }
    md.push('\n');
}

fn append_repository(md: &mut String, pkg: &Value) {
    let Some(repo) = pkg.get("repository") else {
        return;
    };
    let display = str_field(repo, "organization_display_name")
        .or_else(|| str_field(repo, "display_name"))
        .or_else(|| str_field(repo, "name"))
        .unwrap_or("");
    let _ = write!(md, "**Repository:** {display}");
    if let Some(url) = str_field(repo, "url") {
        let _ = write!(md, " ([{url}]({url}))");
    }
    md.push('\n');
}

fn append_security(md: &mut String, pkg: &Value) {
    let Some(sec) = pkg.get("security_report_summary") else {
        return;
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, label) in [
        ("critical", "critical"),
        ("high", "high"),
        ("medium", "medium"),
        ("low", "low"),
    ] {
        if let Some(n) = sec.get(key).and_then(Value::as_u64).filter(|n| *n > 0) {
            parts.push(format!("{n} {label}"));
        }
    }
    if !parts.is_empty() {
        let _ = writeln!(md, "**Security:** {}", parts.join(", "));
    }
}

fn append_versions(md: &mut String, pkg: &Value) {
    let Some(versions) = pkg.get("available_versions").and_then(Value::as_array) else {
        return;
    };
    if versions.is_empty() {
        return;
    }
    md.push_str("\n## Recent Versions\n\n");
    for ver in versions.iter().take(5) {
        let version = str_field(ver, "version").unwrap_or("unknown");
        let ts = ver.get("ts").and_then(Value::as_i64).unwrap_or(0);
        let date = format_epoch_millis(ts * 1000);
        let _ = writeln!(md, "- **{version}** ({date})");
    }
}

fn render(pkg: &Value, kind: &str) -> String {
    let display_name = str_field(pkg, "display_name")
        .or_else(|| str_field(pkg, "name"))
        .unwrap_or("(package)");
    let kind_label = format_kind_label(kind);
    let mut md = format!("# {display_name}\n\n");
    if let Some(desc) = str_field(pkg, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    append_meta_line(&mut md, pkg, &kind_label);
    append_badges(&mut md, pkg);
    append_repository(&mut md, pkg);
    if let Some(home) = str_field(pkg, "home_url") {
        let _ = writeln!(md, "**Homepage:** {home}");
    }
    let keywords = str_list(pkg, "keywords");
    if !keywords.is_empty() {
        let _ = writeln!(md, "**Keywords:** {}", keywords.join(", "));
    }
    if let Some(maintainers) = pkg.get("maintainers").and_then(Value::as_array) {
        let names: Vec<&str> = maintainers
            .iter()
            .filter_map(|m| str_field(m, "name"))
            .collect();
        if !names.is_empty() {
            let _ = writeln!(md, "**Maintainers:** {}", names.join(", "));
        }
    }
    append_security(&mut md, pkg);

    if let Some(links) = pkg
        .get("links")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty())
    {
        md.push_str("\n## Links\n\n");
        for link in links {
            if let (Some(name), Some(url)) = (str_field(link, "name"), str_field(link, "url")) {
                let _ = writeln!(md, "- [{name}]({url})");
            }
        }
    }
    if let Some(install) = str_field(pkg, "install") {
        let _ = write!(
            md,
            "\n## Installation\n\n```bash\n{}\n```\n",
            install.trim()
        );
    }
    append_versions(&mut md, pkg);
    if let Some(readme) = str_field(pkg, "readme") {
        let _ = write!(md, "\n---\n\n## README\n\n{readme}\n");
    }
    md
}

#[async_trait]
impl SpecialHandler for ArtifactHubHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let pkg_ref = parse_package(url)?;
        let api_url = format!(
            "https://artifacthub.io/api/v1/packages/{}/{}/{}",
            pkg_ref.kind, pkg_ref.repo, pkg_ref.name
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
        let pkg: Value = serde_json::from_str(&result.content).ok()?;
        let kind_label = format_kind_label(&pkg_ref.kind);
        let md = render(&pkg, &pkg_ref.kind);
        Some(build_result(
            &md,
            url,
            "artifacthub",
            vec![format!("Fetched via Artifact Hub API ({kind_label})")],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_package_reads_kind_repo_name() {
        let p = parse_package("https://artifacthub.io/packages/helm/bitnami/nginx").unwrap();
        assert_eq!(
            (p.kind.as_str(), p.repo.as_str(), p.name.as_str()),
            ("helm", "bitnami", "nginx")
        );
        assert!(parse_package("https://example.com/packages/helm/bitnami/nginx").is_none());
    }

    #[test]
    fn kind_label_maps_known_and_titlecases_unknown() {
        assert_eq!(format_kind_label("helm"), "Helm Chart");
        assert_eq!(format_kind_label("mystery"), "Mystery");
    }

    #[test]
    fn render_lays_out_meta_badges_and_versions() {
        let pkg = json!({
            "display_name": "nginx",
            "description": "A web server",
            "version": "15.0.0",
            "app_version": "1.25.0",
            "license": "Apache-2.0",
            "official": true,
            "stars": 1234,
            "repository": { "display_name": "Bitnami", "url": "https://charts.bitnami.com" },
            "keywords": ["web", "server"],
            "install": "helm install nginx bitnami/nginx",
            "available_versions": [{ "version": "15.0.0", "ts": 1609459200 }],
            "security_report_summary": { "critical": 1, "high": 2 }
        });
        let md = render(&pkg, "helm");
        assert!(md.contains("# nginx"));
        assert!(md.contains("**Type:** Helm Chart · **Version:** 15.0.0 · **App Version:** 1.25.0 · **License:** Apache-2.0"));
        assert!(md.contains("**Official · 1,234 stars**"));
        assert!(md.contains(
            "**Repository:** Bitnami ([https://charts.bitnami.com](https://charts.bitnami.com))"
        ));
        assert!(md.contains("**Security:** 1 critical, 2 high"));
        assert!(md.contains("```bash\nhelm install nginx bitnami/nginx\n```"));
        assert!(md.contains("- **15.0.0** (2021-01-01)"));
    }
}
