// WebSearch tool: a thin front-end over the `web::search` pipeline.
//
// Validates the model's arguments (query, result cap, recency window) and
// delegates to `web::search::execute_search`, which walks the auto-fallback
// provider chain. The provider implementations, the query-constraint layer and
// the LLM formatter all live under `crate::web::search`.

use crate::web::search::execute_search;
use crate::web::search::provider::Recency;
use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

/// Largest `num_results` the tool honours. Brave documents 20 as the ceiling of
/// its `count` parameter, and the other backends are cut with `take`.
const MAX_NUM_RESULTS: usize = 20;

pub struct WebSearchTool;

#[derive(Debug, Deserialize)]
struct WebSearchInput {
    query: String,
    #[serde(default = "default_num_results")]
    num_results: usize,
    /// How recent a result has to be, as `day`, `week`, `month` or `year`.
    #[serde(default)]
    recency: Option<String>,
}

fn default_num_results() -> usize {
    5
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        mikmik_core::constants::TOOL_NAME_WEB_SEARCH
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns a list of relevant web pages with \
         titles, URLs, and snippets. Use this when you need current information \
         not available in your training data, or when searching for documentation, \
         examples, or news."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "num_results": {
                    "type": "number",
                    "description": "Number of results to return (default: 5, max: 20)"
                },
                "recency": {
                    "type": "string",
                    "enum": ["day", "week", "month", "year"],
                    "description": "Only return results from within this window. Honoured by providers that support it; others ignore it."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: WebSearchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {e}")),
        };

        let num_results = params.num_results.clamp(1, MAX_NUM_RESULTS);
        let recency = match params.recency.as_deref().map(Recency::parse).transpose() {
            Ok(recency) => recency,
            Err(error) => return ToolResult::error(error),
        };
        debug!(query = %params.query, num_results, ?recency, "Web search");

        execute_search(&params.query, num_results, recency, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_query_is_rejected() {
        let input: Result<WebSearchInput, _> = serde_json::from_value(json!({ "num_results": 3 }));
        assert!(input.is_err());
    }

    #[test]
    fn defaults_fill_in_when_omitted() {
        let input: WebSearchInput =
            serde_json::from_value(json!({ "query": "rust" })).expect("parse");
        assert_eq!(input.num_results, 5);
        assert!(input.recency.is_none());
    }

    #[test]
    fn the_result_cap_is_clamped_to_the_ceiling() {
        assert_eq!(100usize.clamp(1, MAX_NUM_RESULTS), 20);
        assert_eq!(0usize.clamp(1, MAX_NUM_RESULTS), 1);
    }
}
