// Choose a License handler: renders a license's metadata and text from the
// choosealicense.com source files (Jekyll frontmatter plus the license body).

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::Duration;

pub struct ChooseALicenseHandler;

static LICENSE_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^/licenses/([^/]+)/?$").expect("static cal license regex"));
static APPENDIX_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^/appendix/?$").expect("static cal appendix regex"));

#[derive(Default)]
struct Frontmatter {
    scalars: BTreeMap<String, String>,
    lists: BTreeMap<String, Vec<String>>,
}

fn strip_quotes(value: &str) -> String {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Split Jekyll frontmatter from the body and parse the flat scalar/list keys.
fn parse_frontmatter(content: &str) -> (Frontmatter, String) {
    let rest = match content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    {
        Some(rest) => rest,
        None => return (Frontmatter::default(), content.to_string()),
    };
    let Some(end) = rest.find("\n---") else {
        return (Frontmatter::default(), content.to_string());
    };
    let block = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\n', '\r']).to_string();
    (parse_block(block), body)
}

fn parse_block(block: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    let mut current_list: Option<String> = None;
    for line in block.lines() {
        let trimmed = line.trim_end();
        if let Some(item) = trimmed.trim_start().strip_prefix("- ") {
            if let Some(key) = &current_list {
                let value = strip_quotes(item);
                if !value.is_empty() {
                    fm.lists.entry(key.clone()).or_default().push(value);
                }
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim();
        if value.is_empty() {
            current_list = Some(key.clone());
            fm.lists.entry(key).or_default();
        } else {
            current_list = None;
            fm.scalars.insert(key, strip_quotes(value));
        }
    }
    fm
}

impl Frontmatter {
    fn scalar(&self, key: &str) -> Option<&str> {
        self.scalars
            .get(key)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// A list value, falling back to splitting a comma-separated scalar.
    fn list(&self, key: &str) -> Vec<String> {
        if let Some(list) = self.lists.get(key).filter(|l| !l.is_empty()) {
            return list.clone();
        }
        self.scalars
            .get(key)
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|i| !i.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn format_label(value: &str) -> String {
    let cleaned = value.replace(['-', '_'], " ");
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = cleaned.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => value.to_string(),
    }
}

fn format_section(md: &mut String, title: &str, items: &[String]) {
    let _ = write!(md, "## {title}\n\n");
    if items.is_empty() {
        md.push_str("- None listed\n\n");
        return;
    }
    for item in items {
        let _ = writeln!(md, "- {}", format_label(item));
    }
    md.push('\n');
}

struct License {
    slug: String,
    is_appendix: bool,
}

fn parse_url(url: &str) -> Option<License> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "choosealicense.com" && host != "www.choosealicense.com" {
        return None;
    }
    let path = parsed.path();
    if let Some(caps) = LICENSE_PATH.captures(path) {
        return Some(License {
            slug: super::util::percent_decode(&caps[1]).to_lowercase(),
            is_appendix: false,
        });
    }
    if APPENDIX_PATH.is_match(path) {
        return Some(License {
            slug: "appendix".to_string(),
            is_appendix: true,
        });
    }
    None
}

fn raw_url(license: &License) -> String {
    if license.is_appendix {
        "https://raw.githubusercontent.com/github/choosealicense.com/gh-pages/_pages/appendix.md"
            .to_string()
    } else {
        format!(
            "https://raw.githubusercontent.com/github/choosealicense.com/gh-pages/_licenses/{}.txt",
            license.slug
        )
    }
}

fn render(fm: &Frontmatter, body: &str, license: &License) -> String {
    let title = fm
        .scalar("title")
        .map(str::to_string)
        .unwrap_or_else(|| format_label(&license.slug));
    let mut md = format!("# {title}\n\n");
    if let Some(description) = fm.scalar("description") {
        let _ = write!(md, "{description}\n\n");
    }
    let spdx = fm
        .scalar("spdx-id")
        .or_else(|| fm.scalar("spdxId"))
        .unwrap_or("Unknown");
    let _ = writeln!(md, "**SPDX ID:** {spdx}");
    let source = if license.is_appendix {
        "https://choosealicense.com/appendix".to_string()
    } else {
        format!("https://choosealicense.com/licenses/{}/", license.slug)
    };
    let _ = write!(md, "**Source:** {source}\n\n");
    format_section(&mut md, "Permissions", &fm.list("permissions"));
    format_section(&mut md, "Conditions", &fm.list("conditions"));
    format_section(&mut md, "Limitations", &fm.list("limitations"));
    let text = body.trim();
    if !text.is_empty() {
        let _ = write!(md, "---\n\n## License Text\n\n{text}\n");
    }
    md
}

#[async_trait]
impl SpecialHandler for ChooseALicenseHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let license = parse_url(url)?;
        let result = load_page(
            &raw_url(&license),
            LoadOptions {
                timeout,
                headers: vec![("Accept".to_string(), "text/plain".to_string())],
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            return None;
        }
        let (fm, body) = parse_frontmatter(&result.content);
        let md = render(&fm, &body, &license);
        Some(build_result(
            &md,
            url,
            "choosealicense",
            vec!["Fetched via Choose a License".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_reads_license_and_appendix() {
        let mit = parse_url("https://choosealicense.com/licenses/MIT/").unwrap();
        assert_eq!(mit.slug, "mit");
        assert!(!mit.is_appendix);
        assert!(
            parse_url("https://choosealicense.com/appendix")
                .unwrap()
                .is_appendix
        );
        assert!(parse_url("https://example.com/licenses/mit").is_none());
    }

    #[test]
    fn frontmatter_splits_scalars_and_lists() {
        let content = "---\ntitle: MIT License\nspdx-id: MIT\npermissions:\n  - commercial-use\n  - modifications\n---\nPermission is granted.\n";
        let (fm, body) = parse_frontmatter(content);
        assert_eq!(fm.scalar("title"), Some("MIT License"));
        assert_eq!(fm.scalar("spdx-id"), Some("MIT"));
        assert_eq!(
            fm.list("permissions"),
            vec!["commercial-use".to_string(), "modifications".to_string()]
        );
        assert_eq!(body.trim(), "Permission is granted.");
    }

    #[test]
    fn render_lays_out_license() {
        let content = "---\ntitle: MIT License\nspdx-id: MIT\ndescription: A short license.\npermissions:\n  - commercial-use\nconditions:\n  - include-copyright\n---\nMIT text here.\n";
        let (fm, body) = parse_frontmatter(content);
        let license = License {
            slug: "mit".to_string(),
            is_appendix: false,
        };
        let md = render(&fm, &body, &license);
        assert!(md.contains("# MIT License"));
        assert!(md.contains("A short license."));
        assert!(md.contains("**SPDX ID:** MIT"));
        assert!(md.contains("## Permissions\n\n- Commercial use"));
        assert!(md.contains("## Conditions\n\n- Include copyright"));
        assert!(md.contains("## Limitations\n\n- None listed"));
        assert!(md.contains("## License Text\n\nMIT text here."));
    }
}
