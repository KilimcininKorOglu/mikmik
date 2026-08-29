// MDN handler: renders a Mozilla developer doc from its `index.json` API.

use super::util::{build_result, html_to_markdown, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

pub struct MdnHandler;

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn section_prose(value: &Value) -> Option<String> {
    let content = str_field(value, "content")?;
    let markdown = html_to_markdown(content);
    match str_field(value, "title") {
        Some(title) => {
            let level = if value.get("isH3").and_then(Value::as_bool) == Some(true) {
                "###"
            } else {
                "##"
            };
            Some(format!("{level} {title}\n\n{markdown}"))
        }
        None => Some(markdown),
    }
}

fn section_pointer(value: &Value, note: &str) -> Option<String> {
    let title = str_field(value, "title")?;
    Some(format!("## {title}\n\n(See {note} at MDN)"))
}

fn section_code(value: &Value) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(title) = str_field(value, "title") {
        parts.push(format!("### {title}"));
    }
    if let Some(code) = str_field(value, "code") {
        let lang = str_field(value, "language").unwrap_or("");
        parts.push(format!("```{lang}\n{code}\n```"));
    }
    parts
}

fn section_definitions(value: &Value) -> Vec<String> {
    let mut parts = Vec::new();
    for item in value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(term) = str_field(item, "term") {
            parts.push(format!("**{term}**"));
        }
        if let Some(desc) = str_field(item, "description") {
            parts.push(html_to_markdown(desc));
        }
    }
    parts
}

fn table_row(row: &Value) -> Option<String> {
    let cells: Vec<String> = row
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(html_to_markdown)
        .collect();
    Some(cells.join(" | "))
}

fn section_table(value: &Value) -> Vec<String> {
    let Some(rows) = value
        .get("rows")
        .and_then(Value::as_array)
        .filter(|r| !r.is_empty())
    else {
        return Vec::new();
    };
    let Some(header_cells) = rows[0].as_array() else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    if let Some(header) = table_row(&rows[0]) {
        parts.push(format!("| {header} |"));
    }
    let separator = header_cells
        .iter()
        .map(|_| "---")
        .collect::<Vec<_>>()
        .join(" | ");
    parts.push(format!("| {separator} |"));
    for row in &rows[1..] {
        if let Some(line) = table_row(row) {
            parts.push(format!("| {line} |"));
        }
    }
    parts
}

fn convert_section(section: &Value) -> Vec<String> {
    let value = section.get("value").unwrap_or(&Value::Null);
    match str_field(section, "type") {
        Some("prose") => section_prose(value).into_iter().collect(),
        Some("browser_compatibility") => section_pointer(value, "browser compatibility data")
            .into_iter()
            .collect(),
        Some("specifications") => section_pointer(value, "specifications")
            .into_iter()
            .collect(),
        Some("code_example") => section_code(value),
        Some("definition_list") => section_definitions(value),
        Some("table") => section_table(value),
        _ => Vec::new(),
    }
}

fn render(doc: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let title = str_field(doc, "title").unwrap_or("MDN");
    parts.push(format!("# {title}"));
    if let Some(summary) = str_field(doc, "summary") {
        parts.push(html_to_markdown(summary));
    }
    for section in doc
        .get("body")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        parts.extend(convert_section(section));
    }
    parts.join("\n\n")
}

/// Rewrite an MDN doc URL to its `index.json` sibling.
fn json_url(url: &str) -> String {
    format!("{}/index.json", url.trim_end_matches('/'))
}

#[async_trait]
impl SpecialHandler for MdnHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host_str()?;
        if !host.contains("developer.mozilla.org") || !parsed.path().contains("/docs/") {
            return None;
        }
        let result = load_page(
            &json_url(url),
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
        let data: Value = serde_json::from_str(&result.content).ok()?;
        let doc = data.get("doc")?;
        str_field(doc, "title")?;
        let md = render(doc);
        Some(build_result(&md, url, "mdn", Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_url_appends_index() {
        assert_eq!(
            json_url("https://developer.mozilla.org/en-US/docs/Web/JavaScript"),
            "https://developer.mozilla.org/en-US/docs/Web/JavaScript/index.json"
        );
        assert_eq!(
            json_url("https://developer.mozilla.org/en-US/docs/Web/JavaScript/"),
            "https://developer.mozilla.org/en-US/docs/Web/JavaScript/index.json"
        );
    }

    #[test]
    fn table_section_builds_markdown_table() {
        let value = json!({ "rows": [["A", "B"], ["1", "2"]] });
        let parts = section_table(&value);
        assert_eq!(parts, vec!["| A | B |", "| --- | --- |", "| 1 | 2 |"]);
    }

    #[test]
    fn render_lays_out_doc() {
        let doc = json!({
            "title": "Array.prototype.map()",
            "summary": "Creates a <strong>new array</strong>.",
            "body": [
                { "type": "prose", "value": { "title": "Syntax", "content": "<p>Use it.</p>" } },
                { "type": "code_example", "value": { "title": "Example", "code": "[1].map(x => x)", "language": "js" } },
                { "type": "specifications", "value": { "title": "Specifications" } }
            ]
        });
        let md = render(&doc);
        assert!(md.contains("# Array.prototype.map()"));
        assert!(md.contains("Creates a **new array**."));
        assert!(md.contains("## Syntax\n\nUse it."));
        assert!(md.contains("### Example\n\n```js\n[1].map(x => x)\n```"));
        assert!(md.contains("## Specifications\n\n(See specifications at MDN)"));
    }
}
