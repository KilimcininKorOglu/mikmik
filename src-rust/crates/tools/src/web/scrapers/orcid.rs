// ORCID handler: renders a researcher profile from the ORCID Public API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct OrcidHandler;

const MAX_WORKS: usize = 50;

static ORCID_ID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"/(\d{4}-\d{4}-\d{4}-\d{3}[\dXx])(?:/|$)").expect("static orcid regex")
});

fn parse_orcid(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "orcid.org" && host != "www.orcid.org" {
        return None;
    }
    Some(ORCID_ID.captures(parsed.path())?[1].to_string())
}

/// Read a nested `{ value: "..." }` string, trimmed and non-empty.
fn nested_value<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(|n| n.get("value"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Preferred display name: credit name, else given + family.
fn format_name(name: &Value) -> Option<String> {
    if let Some(credit) = nested_value(name, "credit-name") {
        return Some(credit.to_string());
    }
    let given = nested_value(name, "given-names");
    let family = nested_value(name, "family-name");
    match (given, family) {
        (Some(g), Some(f)) => Some(format!("{g} {f}")),
        (Some(g), None) => Some(g.to_string()),
        (None, Some(f)) => Some(f.to_string()),
        (None, None) => None,
    }
}

/// Format an ORCID date object as `YYYY`, `YYYY-MM`, or `YYYY-MM-DD`.
fn format_date(date: &Value) -> Option<String> {
    let year = nested_value(date, "year")?;
    let month = nested_value(date, "month");
    let day = nested_value(date, "day");
    Some(match (month, day) {
        (Some(m), Some(d)) => format!("{year}-{:0>2}-{:0>2}", m, d),
        (Some(m), None) => format!("{year}-{:0>2}", m),
        _ => year.to_string(),
    })
}

/// Gather affiliation summaries from both the direct and grouped shapes.
fn collect_affiliations(container: &Value, key: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if let Some(direct) = container.get(key).and_then(Value::as_array) {
        out.extend(direct.iter().cloned());
    }
    if let Some(groups) = container.get("affiliation-group").and_then(Value::as_array) {
        for group in groups {
            let summaries = group.get("summaries").and_then(Value::as_array);
            for summary in summaries.into_iter().flatten() {
                if let Some(entry) = summary.get(key) {
                    out.push(entry.clone());
                }
            }
        }
    }
    out
}

fn affiliation_dates(summary: &Value) -> Option<String> {
    let start = summary.get("start-date").and_then(format_date);
    let end = summary.get("end-date").and_then(format_date);
    match (start, end) {
        (Some(s), Some(e)) => Some(format!("{s} - {e}")),
        (Some(s), None) => Some(format!("{s} - Present")),
        (None, Some(e)) => Some(format!("Until {e}")),
        (None, None) => None,
    }
}

fn format_affiliation(summary: &Value) -> Option<String> {
    let organization = summary
        .get("organization")
        .and_then(|o| str_field(o, "name"));
    let role = str_field(summary, "role-title");
    let department = str_field(summary, "department-name");
    let label = organization.or(role).or(department)?;

    let location = summary
        .get("organization")
        .and_then(|o| o.get("address"))
        .map(|addr| {
            ["city", "region", "country"]
                .iter()
                .filter_map(|k| str_field(addr, k))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty());

    let mut details: Vec<String> = Vec::new();
    if organization.is_some() {
        if let Some(role) = role {
            details.push(role.to_string());
        }
    } else if role.is_some() {
        if let Some(dept) = department {
            details.push(dept.to_string());
        }
    }
    if let (Some(_), Some(dept)) = (organization, department) {
        details.push(format!("Dept: {dept}"));
    }
    if let Some(location) = location {
        details.push(format!("Location: {location}"));
    }
    if let Some(dates) = affiliation_dates(summary) {
        details.push(format!("Dates: {dates}"));
    }

    Some(if details.is_empty() {
        label.to_string()
    } else {
        format!("{label} ({})", details.join("; "))
    })
}

fn collect_work_titles(works: &Value) -> Vec<String> {
    let mut titles: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let groups = works.get("group").and_then(Value::as_array);
    for group in groups.into_iter().flatten() {
        let summaries = group.get("work-summary").and_then(Value::as_array);
        for summary in summaries.into_iter().flatten() {
            let title = summary
                .get("title")
                .and_then(|t| nested_value(t, "title"))
                .map(str::to_string);
            if let Some(title) = title.filter(|t| seen.insert(t.clone())) {
                titles.push(title);
                if titles.len() >= MAX_WORKS {
                    return titles;
                }
            }
        }
    }
    titles
}

fn append_affiliation_section(md: &mut String, heading: &str, summaries: &[Value]) -> bool {
    if summaries.is_empty() {
        return false;
    }
    let _ = write!(md, "### {heading}\n\n");
    for summary in summaries {
        if let Some(line) = format_affiliation(summary) {
            let _ = writeln!(md, "- {line}");
        }
    }
    md.push('\n');
    true
}

fn render(record: &Value, orcid: &str) -> String {
    let person = record.get("person").cloned().unwrap_or(Value::Null);
    let name = person.get("name").and_then(format_name);
    let biography = person
        .get("biography")
        .and_then(|b| str_field(b, "content"));

    let activities = record
        .get("activities-summary")
        .cloned()
        .unwrap_or(Value::Null);
    let employments = activities
        .get("employments")
        .map(|c| collect_affiliations(c, "employment-summary"))
        .unwrap_or_default();
    let educations = activities
        .get("educations")
        .map(|c| collect_affiliations(c, "education-summary"))
        .unwrap_or_default();
    let works = activities
        .get("works")
        .map(collect_work_titles)
        .unwrap_or_default();

    let mut md = format!("# {}\n\n", name.as_deref().unwrap_or("ORCID Profile"));
    let _ = writeln!(md, "**ORCID:** {orcid}");
    let _ = write!(md, "**ORCID Profile:** https://orcid.org/{orcid}\n\n");

    md.push_str("## Biography\n\n");
    match biography {
        Some(bio) => {
            let _ = write!(md, "{bio}\n\n");
        }
        None => md.push_str("No biography available.\n\n"),
    }

    md.push_str("## Affiliations\n\n");
    let mut has_affiliations = append_affiliation_section(&mut md, "Employment", &employments);
    has_affiliations |= append_affiliation_section(&mut md, "Education", &educations);
    if !has_affiliations {
        md.push_str("No affiliations available.\n\n");
    }

    md.push_str("## Works\n\n");
    if works.is_empty() {
        md.push_str("No works available.\n");
    } else {
        for title in works {
            let _ = writeln!(md, "- {title}");
        }
    }
    md
}

#[async_trait]
impl SpecialHandler for OrcidHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let orcid = parse_orcid(url)?;
        let api_url = format!("https://pub.orcid.org/v3.0/{orcid}/record");
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                headers: vec![("Accept".to_string(), "application/json".to_string())],
                ..Default::default()
            },
        )
        .await;
        if !result.ok || result.content.is_empty() {
            return None;
        }
        let record: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&record, &orcid);
        Some(build_result(
            &md,
            url,
            "orcid-api",
            vec!["Fetched via ORCID Public API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_orcid_reads_the_identifier() {
        assert_eq!(
            parse_orcid("https://orcid.org/0000-0002-1825-0097"),
            Some("0000-0002-1825-0097".to_string())
        );
        assert_eq!(
            parse_orcid("https://orcid.org/0000-0002-1825-009X/works"),
            Some("0000-0002-1825-009X".to_string())
        );
        assert_eq!(parse_orcid("https://example.com/0000-0002-1825-0097"), None);
    }

    #[test]
    fn format_name_prefers_credit_then_given_family() {
        assert_eq!(
            format_name(&json!({ "credit-name": { "value": "J. Doe" } })),
            Some("J. Doe".to_string())
        );
        assert_eq!(
            format_name(&json!({
                "given-names": { "value": "Jane" },
                "family-name": { "value": "Doe" }
            })),
            Some("Jane Doe".to_string())
        );
    }

    #[test]
    fn render_lays_out_profile_sections() {
        let record = json!({
            "person": {
                "name": { "given-names": { "value": "Jane" }, "family-name": { "value": "Doe" } },
                "biography": { "content": "Researcher in physics." }
            },
            "activities-summary": {
                "employments": {
                    "affiliation-group": [{
                        "summaries": [{
                            "employment-summary": {
                                "organization": { "name": "MIT" },
                                "role-title": "Professor",
                                "start-date": { "year": { "value": "2010" } }
                            }
                        }]
                    }]
                },
                "works": {
                    "group": [{
                        "work-summary": [{ "title": { "title": { "value": "A Paper" } } }]
                    }]
                }
            }
        });
        let md = render(&record, "0000-0002-1825-0097");
        assert!(md.contains("# Jane Doe"));
        assert!(md.contains("**ORCID:** 0000-0002-1825-0097"));
        assert!(md.contains("Researcher in physics."));
        assert!(md.contains("### Employment"));
        assert!(md.contains("- MIT (Professor; Dates: 2010 - Present)"));
        assert!(md.contains("## Works\n\n- A Paper"));
    }
}
