// Terraform Registry handler: renders a module or a provider via the v1 API.

use super::util::{
    build_result, format_iso_date, format_number, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::Duration;

pub struct TerraformHandler;

/// Which registry resource a URL names, with its API path.
enum Target {
    /// `/modules/{namespace}/{name}/{provider}`
    Module {
        namespace: String,
        name: String,
        provider: String,
    },
    /// `/providers/{namespace}/{type}`
    Provider { namespace: String, kind: String },
}

fn parse_target(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("registry.terraform.io") {
        return None;
    }
    let segments: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["modules", namespace, name, provider, ..] => Some(Target::Module {
            namespace: (*namespace).to_string(),
            name: (*name).to_string(),
            provider: (*provider).to_string(),
        }),
        ["providers", namespace, kind, ..] => Some(Target::Provider {
            namespace: (*namespace).to_string(),
            kind: (*kind).to_string(),
        }),
        _ => None,
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn one_line(text: &str, max: usize) -> String {
    let collapsed = text.replace('\n', " ");
    collapsed.chars().take(max).collect()
}

fn root_list<'a>(module: &'a Value, key: &str) -> Vec<&'a Value> {
    module
        .get("root")
        .and_then(|r| r.get(key))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn append_inputs(md: &mut String, module: &Value) {
    let inputs = root_list(module, "inputs");
    if inputs.is_empty() {
        return;
    }
    let _ = write!(md, "## Inputs ({})\n\n", inputs.len());
    md.push_str("| Name | Type | Required | Description |\n");
    md.push_str("|------|------|----------|-------------|\n");
    for input in inputs.iter().take(30) {
        let name = str_field(input, "name").unwrap_or("");
        let required = match input.get("required").and_then(Value::as_bool) {
            Some(r) => r,
            None => input.get("default").is_none() || input.get("default") == Some(&Value::Null),
        };
        let ty = str_field(input, "type").unwrap_or("any");
        let desc = str_field(input, "description")
            .map(|d| one_line(&d.replace('|', "\\|"), 80))
            .unwrap_or_default();
        let _ = writeln!(
            md,
            "| {name} | `{ty}` | {} | {desc} |",
            if required { "Yes" } else { "No" }
        );
    }
    if inputs.len() > 30 {
        let _ = write!(md, "\n[…{} inputs elided…]\n", inputs.len() - 30);
    }
    md.push('\n');
}

fn append_named_list(md: &mut String, module: &Value, key: &str, heading: &str, limit: usize) {
    let items = root_list(module, key);
    if items.is_empty() {
        return;
    }
    let _ = write!(md, "## {heading} ({})\n\n", items.len());
    for item in items.iter().take(limit) {
        let name = str_field(item, "name").unwrap_or("?");
        match key {
            "outputs" => {
                let _ = write!(md, "- **{name}**");
                if let Some(d) = str_field(item, "description") {
                    let _ = write!(md, ": {}", one_line(d, 100));
                }
                md.push('\n');
            }
            "dependencies" => {
                let source = str_field(item, "source").unwrap_or("");
                let _ = write!(md, "- **{name}**: {source}");
                if let Some(v) = str_field(item, "version") {
                    let _ = write!(md, " ({v})");
                }
                md.push('\n');
            }
            "resources" => {
                let ty = str_field(item, "type").unwrap_or("?");
                let _ = writeln!(md, "- `{ty}` ({name})");
            }
            _ => {}
        }
    }
    if items.len() > limit {
        let _ = write!(md, "\n[…{} {} elided…]\n", items.len() - limit, key);
    }
    md.push('\n');
}

fn render_module(module: &Value) -> String {
    let namespace = str_field(module, "namespace").unwrap_or("");
    let name = str_field(module, "name").unwrap_or("");
    let provider = str_field(module, "provider").unwrap_or("");
    let version = str_field(module, "version").unwrap_or("unknown");
    let mut md = format!("# {namespace}/{name}/{provider}\n\n");
    if let Some(desc) = str_field(module, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    let _ = write!(md, "**Version:** {version}");
    if module.get("verified").and_then(Value::as_bool) == Some(true) {
        md.push_str(" ✓ Verified");
    }
    md.push('\n');
    let downloads = module.get("downloads").and_then(Value::as_u64).unwrap_or(0);
    let _ = writeln!(md, "**Downloads:** {}", format_number(downloads));
    if let Some(published) = str_field(module, "published_at") {
        let _ = writeln!(md, "**Published:** {}", format_iso_date(published));
    }
    if let Some(source) = str_field(module, "source") {
        let _ = writeln!(md, "**Source:** {source}");
    }
    let _ = write!(
        md,
        "\n## Usage\n\n```hcl\nmodule \"{name}\" {{\n  source  = \"{namespace}/{name}/{provider}\"\n  version = \"{version}\"\n}}\n```\n\n"
    );
    append_inputs(&mut md, module);
    append_named_list(&mut md, module, "outputs", "Outputs", 20);
    append_named_list(&mut md, module, "dependencies", "Dependencies", 15);
    append_named_list(&mut md, module, "resources", "Resources", 20);
    md
}

fn append_docs(md: &mut String, provider: &Value, namespace: &str, kind: &str) {
    let Some(docs) = provider.get("docs").and_then(Value::as_array) else {
        return;
    };
    if docs.is_empty() {
        return;
    }
    let mut by_category: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for doc in docs {
        let cat = str_field(doc, "category").unwrap_or("other").to_string();
        by_category.entry(cat).or_default().push(doc);
    }
    md.push_str("## Documentation\n\n");
    for (category, docs) in &by_category {
        let title_cat = capitalize(category);
        let _ = write!(md, "### {title_cat} ({})\n\n", docs.len());
        for doc in docs.iter().take(15) {
            let title = str_field(doc, "title").unwrap_or("");
            let slug = str_field(doc, "slug").unwrap_or("");
            let _ = writeln!(
                md,
                "- [{title}](https://registry.terraform.io/providers/{namespace}/{kind}/latest/docs/{category}/{slug})"
            );
        }
        if docs.len() > 15 {
            let _ = write!(md, "\n[…{} documents elided…]\n", docs.len() - 15);
        }
        md.push('\n');
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn render_provider(provider: &Value, namespace: &str, kind: &str) -> String {
    let ns = str_field(provider, "namespace").unwrap_or(namespace);
    let name = str_field(provider, "name").unwrap_or(kind);
    let version = str_field(provider, "version").unwrap_or("unknown");
    let mut md = format!("# {ns}/{name}\n\n");
    if let Some(desc) = str_field(provider, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    let _ = writeln!(md, "**Version:** {version}");
    if let Some(tier) = str_field(provider, "tier") {
        let _ = writeln!(md, "**Tier:** {tier}");
    }
    let downloads = provider
        .get("downloads")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let _ = writeln!(md, "**Downloads:** {}", format_number(downloads));
    if let Some(published) = str_field(provider, "published_at") {
        let _ = writeln!(md, "**Published:** {}", format_iso_date(published));
    }
    if let Some(source) = str_field(provider, "source") {
        let _ = writeln!(md, "**Source:** {source}");
    }
    let _ = write!(
        md,
        "\n## Usage\n\n```hcl\nterraform {{\n  required_providers {{\n    {name} = {{\n      source  = \"{ns}/{name}\"\n      version = \"~> {version}\"\n    }}\n  }}\n}}\n\nprovider \"{name}\" {{\n  # Configuration options\n}}\n```\n\n"
    );
    append_docs(&mut md, provider, namespace, kind);
    md
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

#[async_trait]
impl SpecialHandler for TerraformHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let md = match parse_target(url)? {
            Target::Module {
                namespace,
                name,
                provider,
            } => {
                let api = format!(
                    "https://registry.terraform.io/v1/modules/{namespace}/{name}/{provider}"
                );
                render_module(&fetch_json(&api, timeout).await?)
            }
            Target::Provider { namespace, kind } => {
                let api = format!("https://registry.terraform.io/v1/providers/{namespace}/{kind}");
                render_provider(&fetch_json(&api, timeout).await?, &namespace, &kind)
            }
        };
        Some(build_result(
            &md,
            url,
            "terraform",
            vec!["Fetched via Terraform Registry API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_reads_module_and_provider() {
        assert!(matches!(
            parse_target("https://registry.terraform.io/modules/terraform-aws-modules/vpc/aws"),
            Some(Target::Module { name, .. }) if name == "vpc"
        ));
        assert!(matches!(
            parse_target("https://registry.terraform.io/providers/hashicorp/aws/latest"),
            Some(Target::Provider { kind, .. }) if kind == "aws"
        ));
        assert!(parse_target("https://example.com/modules/a/b/c").is_none());
    }

    #[test]
    fn render_module_lays_out_usage_and_inputs() {
        let module = json!({
            "namespace": "terraform-aws-modules",
            "name": "vpc",
            "provider": "aws",
            "version": "5.0.0",
            "description": "AWS VPC",
            "verified": true,
            "downloads": 50_000_000,
            "published_at": "2024-01-01T00:00:00Z",
            "root": {
                "inputs": [{ "name": "cidr", "type": "string", "required": true, "description": "The CIDR" }],
                "outputs": [{ "name": "vpc_id", "description": "The VPC id" }]
            }
        });
        let md = render_module(&module);
        assert!(md.contains("# terraform-aws-modules/vpc/aws"));
        assert!(md.contains("**Version:** 5.0.0 ✓ Verified"));
        assert!(md.contains("**Downloads:** 50,000,000"));
        assert!(md.contains("**Published:** 2024-01-01"));
        assert!(md.contains("source  = \"terraform-aws-modules/vpc/aws\""));
        assert!(md.contains("| cidr | `string` | Yes | The CIDR |"));
        assert!(md.contains("- **vpc_id**: The VPC id"));
    }

    #[test]
    fn render_provider_groups_docs_by_category() {
        let provider = json!({
            "namespace": "hashicorp",
            "name": "aws",
            "version": "5.0.0",
            "tier": "official",
            "downloads": 1_000_000_000,
            "docs": [
                { "title": "aws_instance", "slug": "instance", "category": "resources" },
                { "title": "aws_ami", "slug": "ami", "category": "data-sources" }
            ]
        });
        let md = render_provider(&provider, "hashicorp", "aws");
        assert!(md.contains("# hashicorp/aws"));
        assert!(md.contains("**Tier:** official"));
        assert!(md.contains("required_providers"));
        assert!(md.contains("### Resources (1)"));
        assert!(md.contains("[aws_instance](https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/instance)"));
    }
}
