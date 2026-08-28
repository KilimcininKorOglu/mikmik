//! `generate_image`: draw an image from a structured prompt and save it.
//!
//! The provider is chosen through the same account resolution the rest of the
//! app uses: the named account, or the active one, and its key and base URL.
//! No new credential path is opened. The result is written to a file and the
//! tool reports the path.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};

/// The default image model when none is named. An OpenAI-compatible endpoint is
/// what the account resolution points at, so its current image model is used.
const DEFAULT_IMAGE_MODEL: &str = "gpt-image-1";
/// The size used when neither an explicit size nor an aspect ratio is given.
const DEFAULT_SIZE: &str = "1024x1024";
/// The base an account falls back to when it configures none.
const DEFAULT_OPENAI_BASE: &str = "https://api.openai.com/v1";

pub struct GenerateImageTool;

#[derive(Debug, Default, Deserialize)]
struct GenerateInput {
    /// The main subject of the image.
    #[serde(default)]
    subject: Option<String>,
    /// What the subject is doing.
    #[serde(default)]
    action: Option<String>,
    /// The setting around the subject.
    #[serde(default)]
    scene: Option<String>,
    /// How the frame is composed.
    #[serde(default)]
    composition: Option<String>,
    /// The lighting of the scene.
    #[serde(default)]
    lighting: Option<String>,
    /// The visual style.
    #[serde(default)]
    style: Option<String>,
    /// Text that should appear in the image.
    #[serde(default)]
    text: Option<String>,
    /// Changes to make when editing an input image.
    #[serde(default)]
    changes: Option<String>,
    /// The aspect ratio, for example `16:9`. Mapped to a size when no explicit
    /// `image_size` is given.
    #[serde(default)]
    aspect_ratio: Option<String>,
    /// An explicit size, for example `1024x1024`. Wins over `aspect_ratio`.
    #[serde(default)]
    image_size: Option<String>,
    /// A local path or base64 image to edit rather than generate from scratch.
    #[serde(default)]
    input: Option<String>,
    /// The account (provider) to use. Defaults to the active one.
    #[serde(default)]
    provider: Option<String>,
    /// The image model to ask. Defaults to `gpt-image-1`.
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl Tool for GenerateImageTool {
    fn name(&self) -> &str {
        "generate_image"
    }

    fn description(&self) -> &str {
        "Generate an image from a structured prompt and save it to a file. Give \
         the parts you want (subject, action, scene, composition, lighting, \
         style, text, changes); pass `input` to edit an existing image instead \
         of drawing from scratch. Returns the path of the saved file."
    }

    fn permission_level(&self) -> PermissionLevel {
        // Writes a file and spends on a provider; the same level as a write.
        PermissionLevel::Write
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string", "description": "The main subject." },
                "action": { "type": "string", "description": "What the subject is doing." },
                "scene": { "type": "string", "description": "The setting around the subject." },
                "composition": { "type": "string", "description": "How the frame is composed." },
                "lighting": { "type": "string", "description": "The lighting of the scene." },
                "style": { "type": "string", "description": "The visual style." },
                "text": { "type": "string", "description": "Text that should appear in the image." },
                "changes": { "type": "string", "description": "Changes to make when editing an input image." },
                "aspect_ratio": { "type": "string", "description": "Aspect ratio, e.g. 16:9. Mapped to a size." },
                "image_size": { "type": "string", "description": "Explicit size, e.g. 1024x1024. Wins over aspect_ratio." },
                "input": { "type": "string", "description": "A local path or base64 image to edit instead of generating." },
                "provider": { "type": "string", "description": "Account to use. Defaults to the active one." },
                "model": { "type": "string", "description": "Image model. Defaults to gpt-image-1." }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: GenerateInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        let prompt = compose_prompt(&params);
        if prompt.is_empty() {
            return ToolResult::error(
                "give at least one prompt field: subject, action, scene, style, text, or changes"
                    .to_string(),
            );
        }

        let account = params
            .provider
            .clone()
            .unwrap_or_else(|| ctx.config.selected_provider_id().to_string());
        let Some(api_key) = ctx.config.resolve_provider_api_key(&account) else {
            return ToolResult::error(format!(
                "account {account:?} has no API key; configure one or name another provider"
            ));
        };
        let base = ctx
            .config
            .resolve_provider_api_base(&account)
            .unwrap_or_else(|| DEFAULT_OPENAI_BASE.to_string());
        let model = params
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_IMAGE_MODEL.to_string());
        let size = resolve_size(params.aspect_ratio.as_deref(), params.image_size.as_deref());

        let payload = match request_image(
            &base,
            &api_key,
            &model,
            &prompt,
            &size,
            params.input.as_deref(),
        )
        .await
        {
            Ok(payload) => payload,
            Err(error) => return ToolResult::error(error),
        };

        match save_image(payload).await {
            Ok(path) => ToolResult::success(format!("Saved the image to {}", path.display())),
            Err(error) => ToolResult::error(error),
        }
    }
}

/// The image bytes came back one of two ways, so both are carried.
enum ImagePayload {
    Base64(String),
    Url(String),
}

/// Build one prompt string from the structured fields.
///
/// Only the fields that were given appear, each on its own labelled line, so
/// the model reads an ordered brief rather than a blob.
fn compose_prompt(input: &GenerateInput) -> String {
    let parts = [
        ("Subject", &input.subject),
        ("Action", &input.action),
        ("Scene", &input.scene),
        ("Composition", &input.composition),
        ("Lighting", &input.lighting),
        ("Style", &input.style),
        ("Text", &input.text),
        ("Changes", &input.changes),
    ];
    parts
        .iter()
        .filter_map(|(label, value)| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("{label}: {value}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The size to request: an explicit size wins, then an aspect ratio mapped to
/// one, then the default square.
fn resolve_size(aspect_ratio: Option<&str>, image_size: Option<&str>) -> String {
    if let Some(size) = image_size.map(str::trim).filter(|size| !size.is_empty()) {
        return size.to_string();
    }
    match aspect_ratio.map(str::trim) {
        Some("16:9") => "1792x1024".to_string(),
        Some("9:16") => "1024x1792".to_string(),
        Some("3:2") => "1536x1024".to_string(),
        Some("2:3") => "1024x1536".to_string(),
        _ => DEFAULT_SIZE.to_string(),
    }
}

/// The JSON body for a from-scratch generation.
fn generations_body(model: &str, prompt: &str, size: &str) -> Value {
    json!({
        "model": model,
        "prompt": prompt,
        "size": size,
        "n": 1,
        "response_format": "b64_json",
    })
}

/// Pull the image out of a provider's response, error surfaced not swallowed.
fn image_from_response(response: &Value) -> Result<ImagePayload, String> {
    let first = response
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .ok_or_else(|| format!("the provider returned no image: {response}"))?;
    if let Some(b64) = first.get("b64_json").and_then(Value::as_str) {
        return Ok(ImagePayload::Base64(b64.to_string()));
    }
    if let Some(url) = first.get("url").and_then(Value::as_str) {
        return Ok(ImagePayload::Url(url.to_string()));
    }
    Err(format!("the provider returned no image data: {response}"))
}

/// Call the provider: an edit when `input` is given, a generation otherwise.
async fn request_image(
    base: &str,
    api_key: &str,
    model: &str,
    prompt: &str,
    size: &str,
    input: Option<&str>,
) -> Result<ImagePayload, String> {
    let client = reqwest::Client::new();
    let base = base.trim_end_matches('/');
    let response: Value = match input {
        Some(input) => {
            let bytes = read_input_image(input)?;
            let part = reqwest::multipart::Part::bytes(bytes)
                .file_name("input.png")
                .mime_str("image/png")
                .map_err(|error| error.to_string())?;
            let form = reqwest::multipart::Form::new()
                .text("model", model.to_string())
                .text("prompt", prompt.to_string())
                .text("size", size.to_string())
                .part("image", part);
            send(
                client
                    .post(format!("{base}/images/edits"))
                    .bearer_auth(api_key)
                    .multipart(form),
            )
            .await?
        }
        None => {
            let body = generations_body(model, prompt, size);
            send(
                client
                    .post(format!("{base}/images/generations"))
                    .bearer_auth(api_key)
                    .json(&body),
            )
            .await?
        }
    };
    image_from_response(&response)
}

/// Send a built request and read a JSON body, turning a non-2xx into an error.
async fn send(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!("the image provider returned {status}: {body}"));
    }
    serde_json::from_str(&body)
        .map_err(|error| format!("the provider's reply was not JSON: {error}"))
}

/// Read the edit input, whether it is a file path or a base64 string.
fn read_input_image(input: &str) -> Result<Vec<u8>, String> {
    if Path::new(input).exists() {
        return std::fs::read(input).map_err(|error| format!("{input}: {error}"));
    }
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|error| format!("input was neither a readable path nor base64: {error}"))
}

/// Write the image to a fresh file and return its path.
async fn save_image(payload: ImagePayload) -> Result<PathBuf, String> {
    let bytes = match payload {
        ImagePayload::Base64(data) => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| format!("the provider's image was not valid base64: {error}"))?,
        ImagePayload::Url(url) => reqwest::get(&url)
            .await
            .map_err(|error| format!("{url}: {error}"))?
            .bytes()
            .await
            .map_err(|error| format!("{url}: {error}"))?
            .to_vec(),
    };
    let path = std::env::temp_dir().join(format!("mikmik-image-{}.png", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_given_fields_appear_in_the_prompt() {
        let input = GenerateInput {
            subject: Some("a red fox".to_string()),
            style: Some("watercolour".to_string()),
            scene: Some("  ".to_string()),
            ..Default::default()
        };
        let prompt = compose_prompt(&input);
        assert_eq!(prompt, "Subject: a red fox\nStyle: watercolour");
    }

    #[test]
    fn an_empty_input_composes_an_empty_prompt() {
        // The execute path refuses an empty prompt, so this must be empty when
        // nothing was given rather than a stray label.
        assert!(compose_prompt(&GenerateInput::default()).is_empty());
    }

    #[test]
    fn an_explicit_size_wins_over_the_aspect_ratio() {
        assert_eq!(resolve_size(Some("16:9"), Some("512x512")), "512x512");
    }

    #[test]
    fn an_aspect_ratio_maps_to_a_size_when_no_size_is_given() {
        assert_eq!(resolve_size(Some("16:9"), None), "1792x1024");
        assert_eq!(resolve_size(None, None), DEFAULT_SIZE);
    }

    #[test]
    fn the_generation_body_carries_prompt_size_and_model() {
        let body = generations_body("gpt-image-1", "a fox", "1024x1024");
        assert_eq!(body["model"], "gpt-image-1");
        assert_eq!(body["prompt"], "a fox");
        assert_eq!(body["size"], "1024x1024");
        assert_eq!(body["n"], 1);
    }

    #[test]
    fn a_base64_image_is_read_from_the_response() {
        let response = json!({ "data": [ { "b64_json": "aGk=" } ] });
        match image_from_response(&response) {
            Ok(ImagePayload::Base64(data)) => assert_eq!(data, "aGk="),
            _ => panic!("expected a base64 image"),
        }
    }

    #[test]
    fn a_url_image_is_read_from_the_response() {
        let response = json!({ "data": [ { "url": "https://example.com/i.png" } ] });
        match image_from_response(&response) {
            Ok(ImagePayload::Url(url)) => assert_eq!(url, "https://example.com/i.png"),
            _ => panic!("expected a url image"),
        }
    }

    #[test]
    fn a_response_with_no_image_is_an_error_not_an_empty_success() {
        let response = json!({ "data": [] });
        assert!(image_from_response(&response).is_err());
    }
}
