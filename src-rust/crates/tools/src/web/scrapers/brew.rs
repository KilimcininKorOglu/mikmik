// Homebrew handler: renders a formula or cask via the formulae.brew.sh API.

use super::util::{
    build_result, format_number, load_page, percent_encode_component, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct BrewHandler;

/// Which Homebrew resource a URL names.
enum Kind {
    Formula(String),
    Cask(String),
}

fn parse_kind(url: &str) -> Option<Kind> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "formulae.brew.sh" {
        return None;
    }
    let path = parsed.path().trim_end_matches('/');
    if let Some(name) = path.strip_prefix("/formula/") {
        if !name.is_empty() && !name.contains('/') {
            return Some(Kind::Formula(super::util::percent_decode(name)));
        }
    }
    if let Some(name) = path.strip_prefix("/cask/") {
        if !name.is_empty() && !name.contains('/') {
            return Some(Kind::Cask(super::util::percent_decode(name)));
        }
    }
    None
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

/// Total 30-day installs from the analytics map, if present.
fn install_count(data: &Value) -> Option<u64> {
    let map = data
        .get("analytics")?
        .get("install")?
        .get("30d")?
        .as_object()?;
    let total: u64 = map.values().filter_map(Value::as_u64).sum();
    Some(total)
}

fn append_list(md: &mut String, heading: &str, items: &[&str]) {
    if items.is_empty() {
        return;
    }
    let _ = write!(md, "\n## {heading}\n\n");
    for item in items {
        let _ = writeln!(md, "- {item}");
    }
}

fn append_installs_and_command(md: &mut String, data: &Value, command: &str) {
    if let Some(installs) = install_count(data) {
        let _ = writeln!(md, "**Installs (30d):** {}", format_number(installs));
    }
    md.push('\n');
    let _ = write!(md, "```bash\n{command}\n```\n\n");
}

fn render_formula(formula: &Value) -> String {
    let name = str_field(formula, "name").unwrap_or("(formula)");
    let title = str_field(formula, "full_name").unwrap_or(name);
    let mut md = format!("# {title}\n\n");
    if let Some(desc) = str_field(formula, "desc") {
        let _ = write!(md, "{desc}\n\n");
    }
    let stable = formula
        .get("versions")
        .and_then(|v| str_field(v, "stable"))
        .unwrap_or("unknown");
    let _ = write!(md, "**Version:** {stable}");
    if let Some(license) = str_field(formula, "license") {
        let _ = write!(md, " · **License:** {license}");
    }
    md.push('\n');
    append_installs_and_command(&mut md, formula, &format!("brew install {name}"));
    if let Some(home) = str_field(formula, "homepage") {
        let _ = writeln!(md, "**Homepage:** {home}");
    }
    append_list(&mut md, "Dependencies", &str_list(formula, "dependencies"));
    append_list(
        &mut md,
        "Build Dependencies",
        &str_list(formula, "build_dependencies"),
    );
    append_list(
        &mut md,
        "Conflicts With",
        &str_list(formula, "conflicts_with"),
    );
    if let Some(caveats) = str_field(formula, "caveats") {
        let _ = write!(md, "\n## Caveats\n\n{caveats}\n");
    }
    md
}

fn render_cask(cask: &Value) -> String {
    let token = str_field(cask, "token").unwrap_or("(cask)");
    let display = cask
        .get("name")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(token);
    let mut md = format!("# {display}\n\n");
    if let Some(desc) = str_field(cask, "desc") {
        let _ = write!(md, "{desc}\n\n");
    }
    let version = str_field(cask, "version").unwrap_or("unknown");
    let _ = writeln!(md, "**Version:** {version}");
    append_installs_and_command(&mut md, cask, &format!("brew install --cask {token}"));
    if let Some(home) = str_field(cask, "homepage") {
        let _ = writeln!(md, "**Homepage:** {home}");
    }
    let conflicts = cask
        .get("conflicts_with")
        .map(|c| str_list(c, "cask"))
        .unwrap_or_default();
    append_list(&mut md, "Conflicts With", &conflicts);
    if let Some(caveats) = str_field(cask, "caveats") {
        let _ = write!(md, "\n## Caveats\n\n{caveats}\n");
    }
    md
}

#[async_trait]
impl SpecialHandler for BrewHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let (api_url, is_formula) = match parse_kind(url)? {
            Kind::Formula(name) => (
                format!(
                    "https://formulae.brew.sh/api/formula/{}.json",
                    percent_encode_component(&name)
                ),
                true,
            ),
            Kind::Cask(name) => (
                format!(
                    "https://formulae.brew.sh/api/cask/{}.json",
                    percent_encode_component(&name)
                ),
                false,
            ),
        };
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
        let (md, note) = if is_formula {
            (render_formula(&data), "Fetched via Homebrew formula API")
        } else {
            (render_cask(&data), "Fetched via Homebrew cask API")
        };
        Some(build_result(&md, url, "brew", vec![note.to_string()]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_kind_reads_formula_and_cask() {
        assert!(matches!(
            parse_kind("https://formulae.brew.sh/formula/wget"),
            Some(Kind::Formula(n)) if n == "wget"
        ));
        assert!(matches!(
            parse_kind("https://formulae.brew.sh/cask/firefox/"),
            Some(Kind::Cask(n)) if n == "firefox"
        ));
        assert!(parse_kind("https://formulae.brew.sh/analytics").is_none());
        assert!(parse_kind("https://example.com/formula/wget").is_none());
    }

    #[test]
    fn install_count_sums_the_30d_map() {
        let data =
            json!({ "analytics": { "install": { "30d": { "wget": 100, "wget --HEAD": 5 } } } });
        assert_eq!(install_count(&data), Some(105));
        assert_eq!(install_count(&json!({})), None);
    }

    #[test]
    fn render_formula_lays_out_version_installs_and_deps() {
        let formula = json!({
            "name": "wget",
            "full_name": "wget",
            "desc": "Internet file retriever",
            "license": "GPL-3.0-or-later",
            "versions": { "stable": "1.21.4" },
            "homepage": "https://www.gnu.org/software/wget/",
            "dependencies": ["libidn2", "openssl@3"],
            "analytics": { "install": { "30d": { "wget": 250000 } } }
        });
        let md = render_formula(&formula);
        assert!(md.contains("# wget"));
        assert!(md.contains("**Version:** 1.21.4 · **License:** GPL-3.0-or-later"));
        assert!(md.contains("**Installs (30d):** 250,000"));
        assert!(md.contains("brew install wget"));
        assert!(md.contains("## Dependencies\n\n- libidn2\n- openssl@3"));
    }

    #[test]
    fn render_cask_uses_the_display_name_and_token() {
        let cask = json!({
            "token": "firefox",
            "name": ["Mozilla Firefox"],
            "version": "121.0",
            "homepage": "https://www.mozilla.org/firefox/"
        });
        let md = render_cask(&cask);
        assert!(md.contains("# Mozilla Firefox"));
        assert!(md.contains("**Version:** 121.0"));
        assert!(md.contains("brew install --cask firefox"));
    }
}
