// searchcode handler: renders a single code result or a search-results page
// from the searchcode.com API.

use super::util::{build_result, format_number, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct SearchcodeHandler;

const MAX_RESULTS: usize = 10;

static VIEW_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/codesearch/view/([^/?#]+)").expect("static searchcode regex"));

fn is_valid_host(host: &str) -> bool {
    host == "searchcode.com" || host == "www.searchcode.com"
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Normalize the `lines` field (number, string, or array) to a number list.
fn parse_line_numbers(lines: &Value) -> Option<Vec<i64>> {
    let parsed: Vec<i64> = match lines {
        Value::Number(n) => n.as_i64().map(|v| vec![v]).unwrap_or_default(),
        Value::String(s) => s
            .split([',', ' ', '\t', '\n'])
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    (!parsed.is_empty()).then_some(parsed)
}

fn format_line_numbers(lines: &[i64]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    if lines.len() <= 10 {
        return Some(
            lines
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    let min = lines.iter().min()?;
    let max = lines.iter().max()?;
    Some(format!("{min}-{max} ({} lines)", lines.len()))
}

/// A fenced code block, numbering each line when the counts line up.
fn format_code_block(
    code: Option<&str>,
    language: Option<&str>,
    lines: Option<&[i64]>,
) -> Option<String> {
    let code = code?;
    let code_lines: Vec<&str> = code
        .trim_end()
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    let fence = language
        .map(|l| l.trim().to_lowercase())
        .unwrap_or_default();
    let body = match lines.filter(|l| l.len() == code_lines.len()) {
        Some(numbers) => code_lines
            .iter()
            .zip(numbers)
            .map(|(line, n)| format!("{n}: {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        None => code_lines.join("\n"),
    };
    Some(format!("\n\n```{fence}\n{body}\n```\n"))
}

/// Append the shared metadata block used by both single and list results.
fn append_result_meta(md: &mut String, item: &Value) {
    for (field, label) in [
        ("repo", "Repository"),
        ("language", "Language"),
        ("filename", "File"),
        ("location", "Location"),
    ] {
        if let Some(value) = str_field(item, field) {
            let _ = writeln!(md, "**{label}:** {value}");
        }
    }
    if let Some(lines) = parse_line_numbers(item.get("lines").unwrap_or(&Value::Null))
        .as_deref()
        .and_then(format_line_numbers)
    {
        let _ = writeln!(md, "**Lines:** {lines}");
    }
}

fn render_single(data: &Value, id: &str) -> String {
    let filename = str_field(data, "filename")
        .or_else(|| str_field(data, "location"))
        .map(str::to_string)
        .unwrap_or_else(|| format!("Result {id}"));
    let lines = parse_line_numbers(data.get("lines").unwrap_or(&Value::Null));
    let view_url = str_field(data, "url")
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://searchcode.com/codesearch/view/{id}"));

    let mut md = format!("# {filename}\n\n## Description\n\nCode snippet from searchcode.com.\n\n");
    md.push_str("## Metadata\n\n");
    append_result_meta(&mut md, data);
    let _ = writeln!(md, "**Result ID:** {id}");
    let _ = writeln!(md, "**URL:** {view_url}");
    md.push_str("\n## Snippet");
    match format_code_block(
        str_field(data, "code"),
        str_field(data, "language"),
        lines.as_deref(),
    ) {
        Some(block) => md.push_str(&block),
        None => md.push_str("\n\n_No snippet available._\n"),
    }
    md
}

fn render_result_item(md: &mut String, item: &Value) {
    let filename = str_field(item, "filename")
        .or_else(|| str_field(item, "location"))
        .unwrap_or("Result");
    let _ = write!(md, "### {filename}\n\n");
    append_result_meta(md, item);
    let id = item.get("id").map(value_to_string);
    let view_url = str_field(item, "url")
        .map(str::to_string)
        .or_else(|| id.map(|i| format!("https://searchcode.com/codesearch/view/{i}")));
    if let Some(url) = view_url {
        let _ = writeln!(md, "**URL:** {url}");
    }
    let lines = parse_line_numbers(item.get("lines").unwrap_or(&Value::Null));
    if let Some(block) = format_code_block(
        str_field(item, "code"),
        str_field(item, "language"),
        lines.as_deref(),
    ) {
        md.push_str(&block);
        md.push('\n');
    }
    md.push('\n');
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn render_search(data: &Value, query: &str, page: i64) -> String {
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = data
        .get("total")
        .and_then(Value::as_u64)
        .or_else(|| data.get("total_results").and_then(Value::as_u64));

    let mut md = format!(
        "# Searchcode Results\n\n## Description\n\nSearch results for `{query}` on searchcode.com.\n\n"
    );
    md.push_str("## Metadata\n\n");
    let _ = writeln!(md, "**Query:** `{query}`");
    let _ = writeln!(md, "**Page:** {page}");
    if let Some(total) = total {
        let _ = writeln!(md, "**Total Results:** {}", format_number(total));
    }
    let _ = writeln!(md, "**Result Count:** {}", results.len());
    if let Some(next) = data.get("nextpage").and_then(Value::as_i64) {
        let _ = writeln!(md, "**Next Page:** {next}");
    }

    md.push_str("\n## Results\n\n");
    if results.is_empty() {
        md.push_str("_No results found._\n");
        return md;
    }
    for item in results.iter().take(MAX_RESULTS) {
        render_result_item(&mut md, item);
    }
    if results.len() > MAX_RESULTS {
        let _ = write!(md, "\n_Only showing first {MAX_RESULTS} results._\n");
    }
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
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}

async fn handle_view(url: &str, id: &str, timeout: Duration) -> Option<RenderResult> {
    let api_url = format!(
        "https://searchcode.com/api/result/{}/",
        super::util::percent_encode_component(id)
    );
    let data = fetch_json(&api_url, timeout).await?;
    let md = render_single(&data, id);
    Some(build_result(
        &md,
        url,
        "searchcode",
        vec!["Fetched via searchcode API".to_string()],
    ))
}

async fn handle_search(url: &str, parsed: &url::Url, timeout: Duration) -> Option<RenderResult> {
    let query = parsed
        .query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.into_owned())?;
    let path = parsed.path();
    if path != "/" && path != "/codesearch" && path != "/codesearch/" {
        return None;
    }
    let page = parsed
        .query_pairs()
        .find(|(k, _)| k == "p" || k == "page")
        .and_then(|(_, v)| v.parse::<i64>().ok())
        .filter(|p| *p >= 0)
        .unwrap_or(0);
    let api_url = format!(
        "https://searchcode.com/api/codesearch_I/?q={}&p={page}",
        super::util::percent_encode_component(&query)
    );
    let data = fetch_json(&api_url, timeout).await?;
    let md = render_search(&data, &query, page);
    Some(build_result(
        &md,
        url,
        "searchcode",
        vec!["Fetched via searchcode API".to_string()],
    ))
}

#[async_trait]
impl SpecialHandler for SearchcodeHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        if !is_valid_host(parsed.host_str()?) {
            return None;
        }
        if let Some(caps) = VIEW_PATH.captures(parsed.path()) {
            let id = super::util::percent_decode(&caps[1]);
            return handle_view(url, &id, timeout).await;
        }
        handle_search(url, &parsed, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn line_numbers_parse_from_number_string_and_array() {
        assert_eq!(parse_line_numbers(&json!(5)), Some(vec![5]));
        assert_eq!(parse_line_numbers(&json!("1, 2 3")), Some(vec![1, 2, 3]));
        assert_eq!(parse_line_numbers(&json!(["4", 5])), Some(vec![4, 5]));
        assert_eq!(parse_line_numbers(&json!(null)), None);
    }

    #[test]
    fn line_numbers_collapse_when_long() {
        assert_eq!(format_line_numbers(&[1, 2, 3]), Some("1, 2, 3".to_string()));
        let many: Vec<i64> = (1..=15).collect();
        assert_eq!(
            format_line_numbers(&many),
            Some("1-15 (15 lines)".to_string())
        );
    }

    #[test]
    fn code_block_numbers_lines_when_counts_match() {
        let block = format_code_block(Some("a\nb"), Some("Rust"), Some(&[10, 11])).unwrap();
        assert_eq!(block, "\n\n```rust\n10: a\n11: b\n```\n");
    }

    #[test]
    fn render_single_lays_out_snippet() {
        let data = json!({
            "filename": "main.rs",
            "repo": "https://github.com/x/y",
            "language": "Rust",
            "lines": 2,
            "code": "fn main() {}"
        });
        let md = render_single(&data, "42");
        assert!(md.contains("# main.rs"));
        assert!(md.contains("**Repository:** https://github.com/x/y"));
        assert!(md.contains("**Language:** Rust"));
        assert!(md.contains("**Result ID:** 42"));
        // A single line number matching the one-line body prefixes the line.
        assert!(md.contains("```rust\n2: fn main() {}\n```"));
    }

    #[test]
    fn render_search_lists_results() {
        let data = json!({
            "results": [{ "filename": "a.py", "language": "Python", "id": 7 }],
            "total": 1234,
            "nextpage": 1
        });
        let md = render_search(&data, "parser", 0);
        assert!(md.contains("# Searchcode Results"));
        assert!(md.contains("**Query:** `parser`"));
        assert!(md.contains("**Total Results:** 1,234"));
        assert!(md.contains("### a.py"));
        assert!(md.contains("**URL:** https://searchcode.com/codesearch/view/7"));
    }
}
