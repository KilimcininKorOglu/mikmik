// OpenCorporates handler: renders a company record from the OpenCorporates
// v0.4 API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct OpenCorporatesHandler;

static COMPANY_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/companies/([^/]+)/([^/]+)").expect("static opencorp regex"));

fn parse_company(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("opencorporates.com") {
        return None;
    }
    let caps = COMPANY_PATH.captures(parsed.path())?;
    Some((
        super::util::percent_decode(&caps[1]),
        super::util::percent_decode(&caps[2]),
    ))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn append_info_table(md: &mut String, company: &Value) {
    md.push_str("| Field | Value |\n|-------|-------|\n");
    let number = str_field(company, "company_number").unwrap_or("");
    let _ = writeln!(md, "| **Company Number** | {number} |");
    let jurisdiction = str_field(company, "jurisdiction_code")
        .unwrap_or("")
        .to_uppercase();
    let _ = writeln!(md, "| **Jurisdiction** | {jurisdiction} |");
    for (field, label) in [
        ("current_status", "Status"),
        ("company_type", "Company Type"),
        ("incorporation_date", "Incorporated"),
        ("dissolution_date", "Dissolved"),
    ] {
        if let Some(value) = str_field(company, field) {
            let _ = writeln!(md, "| **{label}** | {value} |");
        }
    }
    if let Some(branch) = str_field(company, "branch") {
        let status = str_field(company, "branch_status")
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        let _ = writeln!(md, "| **Branch** | {branch}{status} |");
    }
    if let Some(native) = str_field(company, "native_company_number")
        .filter(|n| Some(*n) != str_field(company, "company_number"))
    {
        let _ = writeln!(md, "| **Native Number** | {native} |");
    }
    md.push('\n');
}

fn append_address(md: &mut String, company: &Value) {
    if let Some(full) = str_field(company, "registered_address_in_full") {
        let _ = write!(md, "## Registered Address\n\n{full}\n\n");
        return;
    }
    let Some(addr) = company.get("registered_address") else {
        return;
    };
    let parts: Vec<&str> = [
        "street_address",
        "locality",
        "region",
        "postal_code",
        "country",
    ]
    .iter()
    .filter_map(|k| str_field(addr, k))
    .collect();
    if !parts.is_empty() {
        let _ = write!(md, "## Registered Address\n\n{}\n\n", parts.join(", "));
    }
}

fn append_agent(md: &mut String, company: &Value) {
    if let Some(agent) = str_field(company, "agent_name") {
        let _ = write!(md, "## Registered Agent\n\n**{agent}**");
        if let Some(address) = str_field(company, "agent_address") {
            let _ = write!(md, "\n{address}");
        }
        md.push_str("\n\n");
    }
}

fn is_inactive(officer: &Value) -> bool {
    officer.get("inactive").and_then(Value::as_bool) == Some(true)
        || str_field(officer, "end_date").is_some()
}

fn format_active_officer(officer: &Value) -> String {
    let mut line = format!("- **{}**", str_field(officer, "name").unwrap_or(""));
    if let Some(position) = str_field(officer, "position") {
        let _ = write!(line, " - {position}");
    }
    if let Some(start) = str_field(officer, "start_date") {
        let _ = write!(line, " (since {start})");
    }
    if let Some(occupation) = str_field(officer, "occupation") {
        let _ = write!(line, " [{occupation}]");
    }
    if let Some(nationality) = str_field(officer, "nationality") {
        let _ = write!(line, " ({nationality})");
    }
    line
}

fn format_former_officer(officer: &Value) -> String {
    let mut line = format!("- **{}**", str_field(officer, "name").unwrap_or(""));
    if let Some(position) = str_field(officer, "position") {
        let _ = write!(line, " - {position}");
    }
    match (
        str_field(officer, "start_date"),
        str_field(officer, "end_date"),
    ) {
        (Some(start), Some(end)) => {
            let _ = write!(line, " ({start} to {end})");
        }
        (None, Some(end)) => {
            let _ = write!(line, " (until {end})");
        }
        _ => {}
    }
    line
}

fn append_officers(md: &mut String, company: &Value) {
    let Some(entries) = company.get("officers").and_then(Value::as_array) else {
        return;
    };
    let officers: Vec<&Value> = entries.iter().filter_map(|o| o.get("officer")).collect();
    let (former, active): (Vec<&Value>, Vec<&Value>) =
        officers.iter().partition(|o| is_inactive(o));

    if !active.is_empty() {
        let _ = write!(md, "## Current Officers ({})\n\n", active.len());
        for officer in &active {
            let _ = writeln!(md, "{}", format_active_officer(officer));
        }
        md.push('\n');
    }
    if !former.is_empty() {
        let _ = write!(md, "## Former Officers ({})\n\n", former.len());
        for officer in former.iter().take(10) {
            let _ = writeln!(md, "{}", format_former_officer(officer));
        }
        if former.len() > 10 {
            let _ = write!(md, "\n[…{} former officers elided…]\n", former.len() - 10);
        }
        md.push('\n');
    }
}

/// Append a bulleted section built by `line` for each array entry.
fn append_list_section(
    md: &mut String,
    company: &Value,
    key: &str,
    heading: &str,
    line: impl Fn(&Value) -> String,
) {
    let Some(items) = company
        .get(key)
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        return;
    };
    let _ = write!(md, "## {heading}\n\n");
    for item in items {
        let _ = writeln!(md, "{}", line(item));
    }
    md.push('\n');
}

fn append_source(md: &mut String, company: &Value) {
    md.push_str("---\n\n");
    if let Some(source) = company.get("source") {
        if let Some(publisher) = str_field(source, "publisher") {
            let _ = write!(md, "**Source:** {publisher}");
            if let Some(url) = str_field(source, "url") {
                let _ = write!(md, " ([registry]({url}))");
            }
            md.push('\n');
        }
    }
    if let Some(registry) = str_field(company, "registry_url") {
        let _ = writeln!(md, "**Official Registry:** {registry}");
    }
    if let Some(retrieved) = str_field(company, "retrieved_at") {
        let _ = writeln!(md, "**Data Retrieved:** {retrieved}");
    }
}

fn render(company: &Value) -> String {
    let mut md = format!(
        "# {}\n\n",
        str_field(company, "name").unwrap_or("(company)")
    );
    append_info_table(&mut md, company);
    append_address(&mut md, company);
    append_agent(&mut md, company);
    append_officers(&mut md, company);
    append_list_section(&mut md, company, "industry_codes", "Industry Codes", |ic| {
        let mut line = format!("- **{}**", str_field(ic, "code").unwrap_or(""));
        if let Some(desc) = str_field(ic, "description") {
            let _ = write!(line, ": {desc}");
        }
        if let Some(scheme) = str_field(ic, "code_scheme_name") {
            let _ = write!(line, " ({scheme})");
        }
        line
    });
    append_list_section(&mut md, company, "identifiers", "Identifiers", |id| {
        let name = str_field(id, "identifier_system_name")
            .or_else(|| str_field(id, "identifier_system_code"))
            .unwrap_or("");
        format!(
            "- **{name}**: {}",
            str_field(id, "identifier_uid").unwrap_or("")
        )
    });
    append_list_section(&mut md, company, "previous_names", "Previous Names", |pn| {
        let mut line = format!("- {}", str_field(pn, "company_name").unwrap_or(""));
        if let Some(date) = str_field(pn, "con_date") {
            let _ = write!(line, " (until {date})");
        }
        line
    });
    append_list_section(
        &mut md,
        company,
        "alternative_names",
        "Alternative Names",
        |an| {
            let mut line = format!("- {}", str_field(an, "company_name").unwrap_or(""));
            if let Some(kind) = str_field(an, "type") {
                let _ = write!(line, " ({kind})");
            }
            line
        },
    );
    append_source(&mut md, company);
    md
}

fn fallback(url: &str, jurisdiction: &str, number: &str, body: &str, note: &str) -> RenderResult {
    let md = format!(
        "# OpenCorporates Company\n\n**Jurisdiction:** {}\n**Company Number:** {number}\n\n{body}\n",
        jurisdiction.to_uppercase()
    );
    build_result(&md, url, "opencorporates", vec![note.to_string()])
}

#[async_trait]
impl SpecialHandler for OpenCorporatesHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let (jurisdiction, number) = parse_company(url)?;
        let api_url =
            format!("https://api.opencorporates.com/v0.4/companies/{jurisdiction}/{number}");
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
            return Some(fallback(
                url,
                &jurisdiction,
                &number,
                "OpenCorporates API request failed. Company details are currently unavailable.",
                "OpenCorporates API request failed",
            ));
        }
        let Ok(data): Result<Value, _> = serde_json::from_str(&result.content) else {
            return Some(fallback(
                url,
                &jurisdiction,
                &number,
                "OpenCorporates response could not be parsed.",
                "OpenCorporates API response parsing failed",
            ));
        };
        let Some(company) = data.get("results").and_then(|r| r.get("company")) else {
            return Some(fallback(
                url,
                &jurisdiction,
                &number,
                "Company details were not available from the OpenCorporates API.",
                "OpenCorporates company payload was missing",
            ));
        };
        let md = render(company);
        Some(build_result(
            &md,
            url,
            "opencorporates",
            vec!["Fetched via OpenCorporates API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_company_reads_jurisdiction_and_number() {
        assert_eq!(
            parse_company("https://opencorporates.com/companies/gb/12345678"),
            Some(("gb".to_string(), "12345678".to_string()))
        );
        assert_eq!(parse_company("https://example.com/companies/gb/1"), None);
    }

    #[test]
    fn officers_split_active_and_former() {
        let company = json!({
            "officers": [
                { "officer": { "name": "Alice", "position": "Director", "start_date": "2020-01-01" } },
                { "officer": { "name": "Bob", "position": "Secretary", "end_date": "2021-06-01" } }
            ]
        });
        let mut md = String::new();
        append_officers(&mut md, &company);
        assert!(md.contains("## Current Officers (1)"));
        assert!(md.contains("- **Alice** - Director (since 2020-01-01)"));
        assert!(md.contains("## Former Officers (1)"));
        assert!(md.contains("- **Bob** - Secretary (until 2021-06-01)"));
    }

    #[test]
    fn render_lays_out_company_record() {
        let company = json!({
            "name": "Acme Ltd",
            "company_number": "12345678",
            "jurisdiction_code": "gb",
            "current_status": "Active",
            "incorporation_date": "2010-05-01",
            "registered_address_in_full": "1 High St, London",
            "industry_codes": [{ "code": "62012", "description": "Business software development" }]
        });
        let md = render(&company);
        assert!(md.contains("# Acme Ltd"));
        assert!(md.contains("| **Company Number** | 12345678 |"));
        assert!(md.contains("| **Jurisdiction** | GB |"));
        assert!(md.contains("| **Status** | Active |"));
        assert!(md.contains("## Registered Address\n\n1 High St, London"));
        assert!(md.contains("- **62012**: Business software development"));
    }
}
