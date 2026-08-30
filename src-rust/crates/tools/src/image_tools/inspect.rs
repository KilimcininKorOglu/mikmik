//! `inspect_image`: ask a vision model a question about a local image.
//!
//! Not what `Read` does. `Read` puts an image in front of the model already
//! driving the session; this sends it to a *separate* model, chosen the way the
//! advisor's model is, so the session can consult a vision model it is not
//! itself running.

use std::path::Path;

use async_trait::async_trait;
use base64::Engine as _;
use mikmik_core::types::{ContentBlock, ImageSource, Message};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};

pub struct InspectImageTool;

#[derive(Debug, Deserialize)]
struct InspectInput {
    /// Path to the image to inspect.
    path: String,
    /// The question to ask about it.
    question: String,
    /// Optional model to ask. Defaults to `advisorModel`, then the session model.
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl Tool for InspectImageTool {
    fn name(&self) -> &str {
        "inspect_image"
    }

    fn description(&self) -> &str {
        "Ask a vision-capable model a question about a local image. Unlike Read, \
         which shows the image to the model running this session, this sends it \
         to a separate model (advisorModel by default) and returns its answer."
    }

    fn permission_level(&self) -> PermissionLevel {
        // Reads a local file and sends it to a model; the same level as Read.
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the image to inspect." },
                "question": { "type": "string", "description": "The question to ask about the image." },
                "model": {
                    "type": "string",
                    "description": "Optional model to ask. Defaults to advisorModel, then the session model."
                }
            },
            "required": ["path", "question"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: InspectInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::error(format!("Invalid input: {error}")),
        };

        let message = match build_image_message(Path::new(&params.path), &params.question) {
            Ok(message) => message,
            Err(error) => return ToolResult::error(error),
        };

        let model = resolve_model(ctx, params.model.as_deref());
        let route = ctx.config.resolve_route(&model);
        let provider = match mikmik_api::provider_for_account(&ctx.config, &route.account).await {
            Ok(provider) => provider,
            Err(error) => {
                return ToolResult::error(format!(
                    "could not reach a model for account {:?}: {error}",
                    route.account
                ))
            }
        };

        let request = mikmik_api::ProviderRequest {
            model: route.model,
            messages: vec![message],
            system_prompt: None,
            tools: Vec::new(),
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            provider_options: json!({}),
        };

        match provider.create_message(request).await {
            Ok(response) => ToolResult::success(text_of(&response.content)),
            Err(error) => ToolResult::error(format!("the vision model failed: {error}")),
        }
    }
}

/// Read the image and pair it with the question as one user message.
fn build_image_message(path: &Path, question: &str) -> Result<Message, String> {
    let media_type = super::media_type_for(path).ok_or_else(|| {
        format!(
            "{}: unsupported image type; use png, jpeg, gif or webp",
            path.display()
        )
    })?;
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(Message::user_blocks(vec![
        ContentBlock::Image {
            source: ImageSource {
                source_type: "base64".to_string(),
                media_type: Some(media_type.to_string()),
                data: Some(data),
                url: None,
            },
        },
        ContentBlock::Text {
            text: question.to_string(),
        },
    ]))
}

/// The model to ask: the explicit one, else `advisorModel`, else the session's.
fn resolve_model(ctx: &ToolContext, explicit: Option<&str>) -> String {
    if let Some(model) = explicit.filter(|model| !model.is_empty()) {
        return model.to_string();
    }
    if let Some(advisor) = ctx
        .config
        .advisor_model
        .as_deref()
        .filter(|m| !m.is_empty())
    {
        return advisor.to_string();
    }
    ctx.config.effective_model().to_string()
}

/// The text of a response, joining every text block.
fn text_of(content: &[ContentBlock]) -> String {
    let text: Vec<&str> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    text.join("\n")
}

#[cfg(test)]
pub(crate) fn tests_ctx(config: mikmik_core::config::Config) -> ToolContext {
    use mikmik_core::permissions::AutoPermissionHandler;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    ToolContext {
        working_dir: PathBuf::from("."),
        permission_handler: Arc::new(AutoPermissionHandler {
            mode: mikmik_core::config::PermissionMode::Default,
        }),
        cost_tracker: mikmik_core::cost::CostTracker::new(),
        session_id: "test-inspect".to_string(),
        file_history: Arc::new(parking_lot::Mutex::new(
            mikmik_core::file_history::FileHistory::new(),
        )),
        file_snapshots: Arc::new(parking_lot::Mutex::new(
            mikmik_core::file_snapshot::FileSnapshotStore::new(),
        )),
        current_turn: Arc::new(AtomicUsize::new(0)),
        non_interactive: true,
        mcp_manager: None,
        config,
        managed_agent_config: None,
        completion_notifier: None,
        pending_permissions: None,
        permission_manager: None,
        user_question_tx: None,
        plan_approval_tx: None,
        tool_output_tx: None,
        plan_mode_tx: None,
        advisor_note_tx: None,
        advisor_name: None,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        current_call: None,
        editor: None,
        inbox: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn an_unsupported_image_type_is_refused_before_any_model_call() {
        // The media-type guard must fire before the network, so a `.txt` file
        // is reported as an unsupported image, not sent to a model.
        let mut file = tempfile::Builder::new()
            .suffix(".txt")
            .tempfile()
            .expect("temp file");
        file.write_all(b"not an image").expect("write");
        let error =
            build_image_message(file.path(), "what is this?").expect_err("a .txt is not an image");
        assert!(error.contains("unsupported image type"), "{error}");
    }

    #[test]
    fn a_supported_image_becomes_an_image_then_text_message() {
        let mut file = tempfile::Builder::new()
            .suffix(".png")
            .tempfile()
            .expect("temp file");
        file.write_all(b"\x89PNG\r\n").expect("write");
        let message = build_image_message(file.path(), "describe it").expect("a message");

        let blocks = message.content_blocks();
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], ContentBlock::Image { .. }));
        match &blocks[1] {
            ContentBlock::Text { text } => assert_eq!(text, "describe it"),
            other => panic!("expected the question as text, got {other:?}"),
        }
    }

    #[test]
    fn the_model_falls_back_from_explicit_to_advisor_to_session() {
        use mikmik_core::config::Config;
        let mut config = Config {
            advisor_model: Some("advisor-vision".to_string()),
            ..Default::default()
        };
        let ctx = crate::image_tools::inspect::tests_ctx(config.clone());
        assert_eq!(resolve_model(&ctx, Some("explicit")), "explicit");
        assert_eq!(resolve_model(&ctx, None), "advisor-vision");

        config.advisor_model = None;
        let ctx = crate::image_tools::inspect::tests_ctx(config);
        assert_eq!(
            resolve_model(&ctx, None),
            ctx.config.effective_model().to_string()
        );
    }
}
