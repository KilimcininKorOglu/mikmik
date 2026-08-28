// Hugging Face handler: renders a model, dataset, space, or user via the API,
// appending the resource's raw-markdown README card when present.

use super::util::{build_result, format_number, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct HuggingFaceHandler;

const RESERVED: [&str; 7] = [
    "docs",
    "blog",
    "pricing",
    "enterprise",
    "join",
    "login",
    "settings",
];

/// Which Hugging Face resource a URL names.
enum Target {
    Model(String),
    Dataset(String),
    Space(String),
    ModelOrUser(String),
}

fn parse_target(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "huggingface.co" {
        return None;
    }
    let parts: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        ["datasets", rest @ ..] if !rest.is_empty() => Some(Target::Dataset(rest.join("/"))),
        ["spaces", org, space, ..] => Some(Target::Space(format!("{org}/{space}"))),
        [first, ..] if RESERVED.contains(first) => None,
        [org, name, ..] => Some(Target::Model(format!("{org}/{name}"))),
        [id] => Some(Target::ModelOrUser((*id).to_string())),
        _ => None,
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Append `**label:** value` when `value` is present.
fn push_field(md: &mut String, label: &str, value: Option<&str>) {
    if let Some(v) = value {
        let _ = writeln!(md, "**{label}:** {v}");
    }
}

fn push_number(md: &mut String, label: &str, value: Option<u64>) {
    if let Some(n) = value {
        let _ = writeln!(md, "**{label}:** {}", format_number(n));
    }
}

fn str_list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The `language` card field, which may be a string or a list.
fn join_language(card: &Value) -> Option<String> {
    match card.get("language") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(_)) => {
            let langs = str_list(card, "language");
            (!langs.is_empty()).then(|| langs.join(", "))
        }
        _ => None,
    }
}

fn joined(v: &Value, key: &str) -> Option<String> {
    let list = str_list(v, key);
    (!list.is_empty()).then(|| list.join(", "))
}

fn append_readme(md: &mut String, heading: &str, readme: Option<&str>) {
    if let Some(body) = readme.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = write!(md, "## {heading}\n\n{body}");
    }
}

fn render_model(model: &Value, readme: Option<&str>) -> String {
    let id = str_field(model, "modelId").unwrap_or("(model)");
    let mut md = format!("# {id}\n\n");
    push_field(&mut md, "Task", str_field(model, "pipeline_tag"));
    push_field(&mut md, "Library", str_field(model, "library_name"));
    push_number(
        &mut md,
        "Downloads",
        model.get("downloads").and_then(Value::as_u64),
    );
    push_number(&mut md, "Likes", model.get("likes").and_then(Value::as_u64));
    if model.get("private").and_then(Value::as_bool) == Some(true) {
        md.push_str("**Visibility:** Private\n");
    }
    if is_gated(model) {
        md.push_str("**Access:** Gated\n");
    }
    if let Some(card) = model.get("cardData") {
        push_field(&mut md, "License", str_field(card, "license"));
        push_field(&mut md, "Language", join_language(card).as_deref());
        push_field(&mut md, "Datasets", joined(card, "datasets").as_deref());
        push_field(&mut md, "Metrics", joined(card, "metrics").as_deref());
    }
    push_field(&mut md, "Tags", joined(model, "tags").as_deref());
    md.push('\n');
    append_readme(&mut md, "Model Card", readme);
    md
}

fn is_gated(v: &Value) -> bool {
    match v.get("gated") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty() && s != "false",
        _ => false,
    }
}

fn render_dataset(dataset: &Value, readme: Option<&str>) -> String {
    let id = str_field(dataset, "id").unwrap_or("(dataset)");
    let mut md = format!("# {id}\n\n");
    if let Some(desc) = str_field(dataset, "description") {
        let _ = write!(md, "{desc}\n\n");
    }
    push_number(
        &mut md,
        "Downloads",
        dataset.get("downloads").and_then(Value::as_u64),
    );
    push_number(
        &mut md,
        "Likes",
        dataset.get("likes").and_then(Value::as_u64),
    );
    if dataset.get("private").and_then(Value::as_bool) == Some(true) {
        md.push_str("**Visibility:** Private\n");
    }
    if is_gated(dataset) {
        md.push_str("**Access:** Gated\n");
    }
    if let Some(card) = dataset.get("cardData") {
        push_field(&mut md, "License", str_field(card, "license"));
        push_field(&mut md, "Language", join_language(card).as_deref());
        push_field(&mut md, "Tasks", joined(card, "task_categories").as_deref());
        push_field(&mut md, "Size", joined(card, "size_categories").as_deref());
    }
    push_field(&mut md, "Tags", joined(dataset, "tags").as_deref());
    md.push('\n');
    append_readme(&mut md, "Dataset Card", readme);
    md
}

fn render_space(space: &Value, readme: Option<&str>) -> String {
    let id = str_field(space, "id").unwrap_or("(space)");
    let mut md = format!("# {id}\n\n");
    if let Some(title) = str_field(space, "title") {
        let _ = write!(md, "{title}\n\n");
    }
    push_field(&mut md, "Author", str_field(space, "author"));
    push_field(&mut md, "SDK", str_field(space, "sdk"));
    push_number(&mut md, "Likes", space.get("likes").and_then(Value::as_u64));
    if space.get("private").and_then(Value::as_bool) == Some(true) {
        md.push_str("**Visibility:** Private\n");
    }
    if let Some(card) = space.get("cardData") {
        push_field(&mut md, "License", str_field(card, "license"));
        push_field(&mut md, "App File", str_field(card, "app_file"));
    }
    push_field(&mut md, "Tags", joined(space, "tags").as_deref());
    md.push('\n');
    append_readme(&mut md, "Space Info", readme);
    md
}

fn render_user(user: &Value, id: &str) -> String {
    let name = str_field(user, "user").unwrap_or(id);
    let mut md = format!("# {name}\n\n");
    push_field(&mut md, "Name", str_field(user, "fullname"));
    push_number(
        &mut md,
        "Models",
        user.get("numModels").and_then(Value::as_u64),
    );
    push_number(
        &mut md,
        "Datasets",
        user.get("numDatasets").and_then(Value::as_u64),
    );
    push_number(
        &mut md,
        "Spaces",
        user.get("numSpaces").and_then(Value::as_u64),
    );
    if let Some(orgs) = user.get("orgs").and_then(Value::as_array) {
        let names: Vec<&str> = orgs.iter().filter_map(|o| str_field(o, "name")).collect();
        if !names.is_empty() {
            let _ = writeln!(md, "**Organizations:** {}", names.join(", "));
        }
    }
    md
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    if !result.ok {
        return None;
    }
    serde_json::from_str(&result.content).ok()
}

async fn fetch_readme(url: &str, timeout: Duration) -> Option<String> {
    let result = load_page(
        url,
        LoadOptions {
            timeout: timeout.min(Duration::from_secs(5)),
            ..Default::default()
        },
    )
    .await;
    result.ok.then_some(result.content)
}

impl HuggingFaceHandler {
    async fn render(&self, target: Target, timeout: Duration) -> Option<String> {
        match target {
            Target::Model(id) => {
                let api = format!("https://huggingface.co/api/models/{id}");
                let readme_url = format!("https://huggingface.co/{id}/raw/main/README.md");
                let (data, readme) = tokio::join!(
                    fetch_json(&api, timeout),
                    fetch_readme(&readme_url, timeout)
                );
                Some(render_model(&data?, readme.as_deref()))
            }
            Target::Dataset(id) => {
                let api = format!("https://huggingface.co/api/datasets/{id}");
                let readme_url = format!("https://huggingface.co/datasets/{id}/raw/main/README.md");
                let (data, readme) = tokio::join!(
                    fetch_json(&api, timeout),
                    fetch_readme(&readme_url, timeout)
                );
                Some(render_dataset(&data?, readme.as_deref()))
            }
            Target::Space(id) => {
                let api = format!("https://huggingface.co/api/spaces/{id}");
                let readme_url = format!("https://huggingface.co/spaces/{id}/raw/main/README.md");
                let (data, readme) = tokio::join!(
                    fetch_json(&api, timeout),
                    fetch_readme(&readme_url, timeout)
                );
                Some(render_space(&data?, readme.as_deref()))
            }
            Target::ModelOrUser(id) => self.render_model_or_user(&id, timeout).await,
        }
    }

    async fn render_model_or_user(&self, id: &str, timeout: Duration) -> Option<String> {
        if let Some(model) =
            fetch_json(&format!("https://huggingface.co/api/models/{id}"), timeout).await
        {
            if model.get("modelId").is_some() {
                let readme = fetch_readme(
                    &format!("https://huggingface.co/{id}/raw/main/README.md"),
                    timeout,
                )
                .await;
                return Some(render_model(&model, readme.as_deref()));
            }
        }
        let user = fetch_json(&format!("https://huggingface.co/api/users/{id}"), timeout).await?;
        Some(render_user(&user, id))
    }
}

#[async_trait]
impl SpecialHandler for HuggingFaceHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let target = parse_target(url)?;
        let md = self.render(target, timeout).await?;
        Some(build_result(
            &md,
            url,
            "huggingface",
            vec!["Fetched via Hugging Face API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_target_classifies_paths() {
        assert!(
            matches!(parse_target("https://huggingface.co/bert-base-uncased"), Some(Target::ModelOrUser(i)) if i == "bert-base-uncased")
        );
        assert!(
            matches!(parse_target("https://huggingface.co/google/flan-t5"), Some(Target::Model(i)) if i == "google/flan-t5")
        );
        assert!(
            matches!(parse_target("https://huggingface.co/datasets/squad"), Some(Target::Dataset(i)) if i == "squad")
        );
        assert!(
            matches!(parse_target("https://huggingface.co/spaces/org/demo"), Some(Target::Space(i)) if i == "org/demo")
        );
        assert!(parse_target("https://huggingface.co/docs").is_none());
        assert!(parse_target("https://example.com/x").is_none());
    }

    #[test]
    fn render_model_lays_out_fields_language_and_readme() {
        let model = json!({
            "modelId": "google/flan-t5",
            "pipeline_tag": "text2text-generation",
            "library_name": "transformers",
            "downloads": 5_000_000,
            "likes": 1200,
            "gated": "auto",
            "tags": ["nlp", "t5"],
            "cardData": { "license": "apache-2.0", "language": ["en", "fr"] }
        });
        let md = render_model(&model, Some("Model details here."));
        assert!(md.contains("# google/flan-t5"));
        assert!(md.contains("**Task:** text2text-generation"));
        assert!(md.contains("**Downloads:** 5,000,000"));
        assert!(md.contains("**Access:** Gated"));
        assert!(md.contains("**License:** apache-2.0"));
        assert!(md.contains("**Language:** en, fr"));
        assert!(md.contains("## Model Card\n\nModel details here."));
    }

    #[test]
    fn render_user_lists_counts_and_orgs() {
        let user = json!({
            "user": "ferris",
            "fullname": "Ferris Crab",
            "numModels": 3,
            "orgs": [{ "name": "rust-ml" }]
        });
        let md = render_user(&user, "ferris");
        assert!(md.contains("# ferris"));
        assert!(md.contains("**Name:** Ferris Crab"));
        assert!(md.contains("**Models:** 3"));
        assert!(md.contains("**Organizations:** rust-ml"));
    }

    #[test]
    fn gated_reads_bool_and_string() {
        assert!(is_gated(&json!({ "gated": true })));
        assert!(is_gated(&json!({ "gated": "manual" })));
        assert!(!is_gated(&json!({ "gated": false })));
        assert!(!is_gated(&json!({ "gated": "false" })));
        assert!(!is_gated(&json!({})));
    }
}
