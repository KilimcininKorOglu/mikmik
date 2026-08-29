// Anthropic provider: Claude's built-in `web_search_20250305` server tool.
//
// Ported from oh-my-pi `web/search/providers/anthropic.ts`. The session's own
// Anthropic login becomes the grounding engine: a raw `POST /v1/messages`
// carrying the web-search tool, whose synthesized answer, citations, and
// source metadata map onto `SearchResponse`. Available when an Anthropic
// account is configured (OAuth or API key) or `ANTHROPIC_SEARCH_API_KEY` is set.
//
// The request is built by hand rather than through `mikmik_api`'s transformer,
// exactly as omp bypasses its own: the transformer neither forwards the
// server-tool block nor preserves the `server_tool_use` / `web_search_tool_result`
// / citation blocks the response carries. The OAuth path mirrors the streaming
// client's stealth treatment (billing block as `system[0]`, identity block as
// `system[1]`, `metadata.user_id`, tier-selected `anthropic-beta`).

use crate::web::search::provider::{SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax, StructuredQuery};
use crate::web::search::types::{
    SearchCitation, SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
    SearchUsage,
};
use crate::ToolContext;
use async_trait::async_trait;
use mikmik_core::provider_id::ProviderId;
use mikmik_core::AuthStore;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Value};

pub struct AnthropicProvider;

const DEFAULT_MODEL: &str = "claude-haiku-4-5";
const DEFAULT_MAX_TOKENS: u64 = 4096;
const WEB_SEARCH_TOOL_NAME: &str = "web_search";
const WEB_SEARCH_TOOL_TYPE: &str = "web_search_20250305";

/// Claude's search backend parses the classic operator set, so most directives
/// are re-emitted as query text. `site:` is intentionally omitted: includes and
/// excludes map onto the tool's native `allowed_domains`/`blocked_domains`.
const ANTHROPIC_SYNTAX: QuerySyntax = QuerySyntax {
    phrases: true,
    negation: true,
    or: true,
    site: false,
    in_url: true,
    in_title: true,
    in_text: false,
    filetype: true,
    date_range: true,
};

/// A resolved Anthropic credential and which auth scheme it speaks.
enum Credential {
    /// `x-api-key` path.
    ApiKey(String),
    /// `Authorization: Bearer` path (Claude.ai OAuth), needing stealth headers.
    OAuth(String),
}

/// A non-empty environment variable, or `None`.
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The Anthropic account to search through: the active account when it is one,
/// else the first Anthropic account on record.
fn anthropic_account_id(ctx: &ToolContext, store: &AuthStore) -> Option<String> {
    let accounts = store.accounts_for_protocol(ProviderId::ANTHROPIC);
    if let Some(active) = ctx.config.provider.as_ref() {
        if accounts.iter().any(|a| a == active) {
            return Some(active.clone());
        }
    }
    accounts.into_iter().next()
}

/// Whether any Anthropic credential is reachable, without refreshing tokens.
fn has_anthropic_auth(ctx: &ToolContext) -> bool {
    nonempty_env("ANTHROPIC_SEARCH_API_KEY").is_some()
        || nonempty_env("ANTHROPIC_API_KEY").is_some()
        || anthropic_account_id(ctx, &AuthStore::load()).is_some()
}

/// Resolve a usable credential, refreshing an OAuth token when needed.
///
/// Precedence mirrors omp: the explicit `ANTHROPIC_SEARCH_API_KEY` override, the
/// configured Anthropic account (OAuth token first, then a stored API key), and
/// finally the `ANTHROPIC_API_KEY` environment fallback.
async fn resolve_credential(ctx: &ToolContext) -> Option<Credential> {
    if let Some(key) = nonempty_env("ANTHROPIC_SEARCH_API_KEY") {
        return Some(Credential::ApiKey(key));
    }
    let store = AuthStore::load();
    if let Some(id) = anthropic_account_id(ctx, &store) {
        if store.anthropic_tokens(&id).is_some() {
            if let Some((cred, bearer)) = mikmik_core::oauth::resolve_auth_for_account(&id).await {
                return Some(if bearer {
                    Credential::OAuth(cred)
                } else {
                    Credential::ApiKey(cred)
                });
            }
        }
        if let Some(key) = store.api_key_for(&id).filter(|k| !k.is_empty()) {
            return Some(Credential::ApiKey(key));
        }
    }
    nonempty_env("ANTHROPIC_API_KEY").map(Credential::ApiKey)
}

/// The search model: `ANTHROPIC_SEARCH_MODEL`, else the Haiku default.
fn model() -> String {
    nonempty_env("ANTHROPIC_SEARCH_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// The Messages endpoint: `ANTHROPIC_SEARCH_BASE_URL`, else the account's base.
fn messages_url(ctx: &ToolContext) -> String {
    let base = nonempty_env("ANTHROPIC_SEARCH_BASE_URL")
        .unwrap_or_else(|| ctx.config.resolve_anthropic_api_base());
    format!("{}/v1/messages", base.trim_end_matches('/'))
}

/// The upstream request derived from a parsed query.
struct QueryPlan {
    query: String,
    allowed_domains: Vec<String>,
    blocked_domains: Vec<String>,
}

/// Unique bare hosts from `site:` values, dropping any path component.
fn hosts(sites: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for site in sites {
        let host = match site.find('/') {
            Some(slash) => &site[..slash],
            None => site.as_str(),
        };
        if !host.is_empty() && !out.iter().any(|h| h == host) {
            out.push(host.to_string());
        }
    }
    out
}

/// Map parsed directives onto the request: `site:` includes become
/// `allowed_domains`, `-site:` exclusions become `blocked_domains` (mutually
/// exclusive on the API, so exclusions ride only when there are no includes),
/// and the rest re-emit as query syntax. Directive-free queries pass through.
fn plan_query(raw: &str, parsed: &StructuredQuery) -> QueryPlan {
    if !parsed.has_directives {
        return QueryPlan {
            query: raw.to_string(),
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
        };
    }
    let allowed = hosts(&parsed.sites);
    let blocked = if allowed.is_empty() {
        hosts(&parsed.excluded_sites)
    } else {
        Vec::new()
    };
    QueryPlan {
        query: format_query(parsed, ANTHROPIC_SYNTAX),
        allowed_domains: allowed,
        blocked_domains: blocked,
    }
}

/// The base request body, before any OAuth augmentation.
fn request_body(model: &str, plan: &QueryPlan) -> Value {
    let mut tool = json!({ "type": WEB_SEARCH_TOOL_TYPE, "name": WEB_SEARCH_TOOL_NAME });
    if !plan.allowed_domains.is_empty() {
        tool["allowed_domains"] = json!(plan.allowed_domains);
    } else if !plan.blocked_domains.is_empty() {
        tool["blocked_domains"] = json!(plan.blocked_domains);
    }
    json!({
        "model": model,
        "max_tokens": DEFAULT_MAX_TOKENS,
        "messages": [{ "role": "user", "content": plan.query }],
        "tools": [tool],
    })
}

/// Inject the OAuth stealth fields and return the tier-selected `anthropic-beta`
/// value. `system[0]` carries the billing header, `system[1]` the Claude Code
/// identity the gate requires; `metadata.user_id` carries the account triple.
async fn apply_oauth(body: &mut Value, user_content: &str) -> String {
    let (account_uuid, has_premium) = mikmik_core::oauth::current_anthropic_account_meta()
        .await
        .unwrap_or_default();

    let billing = mikmik_core::oauth_config::claude_code_billing_header(user_content);
    let identity = mikmik_core::oauth_config::CLAUDE_CODE_SYSTEM_PROMPT_PREFIX.to_string();
    body["system"] = json!([
        { "type": "text", "text": billing },
        { "type": "text", "text": identity },
    ]);

    let user_id = json!({
        "device_id": mikmik_core::oauth_config::anthropic_device_id(),
        "account_uuid": account_uuid,
        "session_id": uuid::Uuid::new_v4().to_string(),
    })
    .to_string();
    body["metadata"] = json!({ "user_id": user_id });

    mikmik_core::oauth_config::oauth_beta_flags(has_premium).join(",")
}

/// Attach the common and scheme-specific headers to a Messages request.
fn authed_request(
    builder: reqwest::RequestBuilder,
    cred: &Credential,
    oauth_beta: Option<&str>,
) -> reqwest::RequestBuilder {
    let builder = builder
        .header(
            "anthropic-version",
            mikmik_core::constants::ANTHROPIC_API_VERSION,
        )
        .header("content-type", "application/json")
        .header("accept", "application/json");
    match cred {
        Credential::ApiKey(key) => builder.header("x-api-key", key),
        Credential::OAuth(token) => builder
            .header("Authorization", format!("Bearer {token}"))
            .header("anthropic-beta", oauth_beta.unwrap_or(""))
            .header(
                "user-agent",
                mikmik_core::oauth_config::claude_code_user_agent(),
            )
            .header("x-app", "cli")
            .header("anthropic-dangerous-direct-browser-access", "true"),
    }
}

/// Send the request and return the parsed JSON body, or a provider error.
async fn send_request(
    cred: &Credential,
    url: &str,
    body: &Value,
    oauth_beta: Option<&str>,
    timeout: std::time::Duration,
) -> Result<Value, SearchProviderError> {
    let id = SearchProviderId::Anthropic;
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| SearchProviderError::new(id, format!("HTTP client: {e}")))?;
    let resp = authed_request(client.post(url), cred, oauth_beta)
        .json(body)
        .send()
        .await
        .map_err(|e| SearchProviderError::new(id, format!("Search request failed: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return Err(SearchProviderError::with_status(
            id,
            format!("Anthropic API returned status {status}"),
            status,
        ));
    }
    resp.json()
        .await
        .map_err(|e| SearchProviderError::new(id, format!("Failed to parse response: {e}")))
}

static PAGE_AGE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^(\d+)\s*(s|sec|second|m|min|minute|h|hour|d|day|w|week|mo|month|y|year)s?\s*(ago)?$",
    )
    .expect("static page-age regex")
});

/// Seconds encoded by a `page_age` label such as `2 days ago`, defaulting an
/// unknown unit to one day, or `None` when the label does not parse.
fn parse_page_age(page_age: Option<&str>) -> Option<f64> {
    let text = page_age?.trim();
    let caps = PAGE_AGE.captures(text)?;
    let value: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str().to_lowercase();
    let multiplier = match unit.as_str() {
        "s" | "sec" | "second" => 1.0,
        "m" | "min" | "minute" => 60.0,
        "h" | "hour" => 3600.0,
        "d" | "day" => 86_400.0,
        "w" | "week" => 604_800.0,
        "mo" | "month" => 2_592_000.0,
        "y" | "year" => 31_536_000.0,
        _ => 86_400.0,
    };
    Some(value * multiplier)
}

/// A string field on a JSON object, when present and a string.
fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// True when a `server_tool_use` block names the web-search tool. The upstream
/// name may carry a `claude_tool_` prefix, so a suffix match is enough.
fn is_web_search_tool(block: &Value) -> bool {
    str_field(block, "name").is_some_and(|name| name.ends_with(WEB_SEARCH_TOOL_NAME))
}

/// Sources from one `web_search_tool_result` block's `content` array.
fn sources_from_result(block: &Value) -> Vec<SearchSource> {
    let Some(items) = block.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| str_field(item, "type") == Some("web_search_result"))
        .map(|item| SearchSource {
            title: str_field(item, "title").unwrap_or("(No title)").to_string(),
            url: str_field(item, "url").unwrap_or("").to_string(),
            published_date: str_field(item, "page_age").map(str::to_string),
            age_seconds: parse_page_age(str_field(item, "page_age")),
            ..Default::default()
        })
        .collect()
}

/// Citations from a `text` block's `citations` array.
fn citations_from_text(block: &Value) -> Vec<SearchCitation> {
    let Some(items) = block.get("citations").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|c| SearchCitation {
            url: str_field(c, "url").unwrap_or("").to_string(),
            title: str_field(c, "title").unwrap_or("").to_string(),
            cited_text: str_field(c, "cited_text").map(str::to_string),
        })
        .collect()
}

/// Usage metrics from the response `usage` object.
fn usage_from(response: &Value) -> Option<SearchUsage> {
    let usage = response.get("usage")?;
    Some(SearchUsage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        search_requests: usage
            .get("server_tool_use")
            .and_then(|s| s.get("web_search_requests"))
            .and_then(Value::as_u64),
        ..Default::default()
    })
}

/// Fold one content block into the accumulating response.
fn absorb_block(block: &Value, resp: &mut SearchResponse, answer: &mut Vec<String>) {
    match str_field(block, "type") {
        Some("server_tool_use") if is_web_search_tool(block) => {
            if let Some(q) = block.get("input").and_then(|i| str_field(i, "query")) {
                resp.search_queries.push(q.to_string());
            }
        }
        Some("web_search_tool_result") => resp.sources.extend(sources_from_result(block)),
        Some("text") => {
            if let Some(text) = str_field(block, "text") {
                answer.push(text.to_string());
            }
            resp.citations.extend(citations_from_text(block));
        }
        _ => {}
    }
}

/// Map the raw Anthropic response onto a `SearchResponse`.
fn parse_response(response: &Value) -> SearchResponse {
    let mut resp = SearchResponse::empty(SearchProviderId::Anthropic.as_str());
    let mut answer_parts: Vec<String> = Vec::new();
    if let Some(blocks) = response.get("content").and_then(Value::as_array) {
        for block in blocks {
            absorb_block(block, &mut resp, &mut answer_parts);
        }
    }
    if !answer_parts.is_empty() {
        resp.answer = Some(answer_parts.join("\n\n"));
    }
    resp.usage = usage_from(response);
    resp.model = str_field(response, "model").map(str::to_string);
    resp
}

#[async_trait]
impl SearchProvider for AnthropicProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Anthropic
    }

    async fn is_available(&self, ctx: &ToolContext) -> bool {
        has_anthropic_auth(ctx)
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let cred = resolve_credential(params.ctx).await.ok_or_else(|| {
            SearchProviderError::new(
                self.id(),
                "No Anthropic credentials found. Set ANTHROPIC_SEARCH_API_KEY or ANTHROPIC_API_KEY, or configure an Anthropic account.",
            )
        })?;
        let model = model();
        let plan = plan_query(params.query, params.parsed);
        let mut body = request_body(&model, &plan);
        let oauth_beta = match &cred {
            Credential::OAuth(_) => Some(apply_oauth(&mut body, &plan.query).await),
            Credential::ApiKey(_) => None,
        };
        let value = send_request(
            &cred,
            &messages_url(params.ctx),
            &body,
            oauth_beta.as_deref(),
            params.timeout,
        )
        .await?;
        let mut response = parse_response(&value);
        if response.sources.len() > params.limit {
            response.sources.truncate(params.limit);
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directive_free_query_passes_through_without_domain_filters() {
        let parsed = crate::web::search::query::parse_search_query("rust ownership");
        let plan = plan_query("rust ownership", &parsed);
        assert_eq!(plan.query, "rust ownership");
        assert!(plan.allowed_domains.is_empty());
        assert!(plan.blocked_domains.is_empty());
    }

    #[test]
    fn site_includes_become_allowed_domains_and_drop_paths() {
        let parsed = crate::web::search::query::parse_search_query(
            "api site:github.com/rust-lang site:docs.rs",
        );
        let plan = plan_query("api site:github.com/rust-lang site:docs.rs", &parsed);
        assert_eq!(plan.allowed_domains, vec!["github.com", "docs.rs"]);
        assert!(plan.blocked_domains.is_empty());
        // `site:` is not re-emitted as query text; the free term remains.
        assert!(plan.query.contains("api"));
        assert!(!plan.query.contains("site:"));
    }

    #[test]
    fn exclusions_become_blocked_domains_only_without_includes() {
        let excl = crate::web::search::query::parse_search_query("cats -site:pinterest.com");
        let plan = plan_query("cats -site:pinterest.com", &excl);
        assert!(plan.allowed_domains.is_empty());
        assert_eq!(plan.blocked_domains, vec!["pinterest.com"]);

        // An include present, so exclusions are suppressed (API is exclusive).
        let both = crate::web::search::query::parse_search_query(
            "cats site:reddit.com -site:pinterest.com",
        );
        let plan = plan_query("cats site:reddit.com -site:pinterest.com", &both);
        assert_eq!(plan.allowed_domains, vec!["reddit.com"]);
        assert!(plan.blocked_domains.is_empty());
    }

    #[test]
    fn the_body_carries_the_tool_and_the_planned_query() {
        let parsed = crate::web::search::query::parse_search_query("x site:example.com");
        let plan = plan_query("x site:example.com", &parsed);
        let body = request_body("claude-haiku-4-5", &plan);
        assert_eq!(body["tools"][0]["type"], json!(WEB_SEARCH_TOOL_TYPE));
        assert_eq!(body["tools"][0]["name"], json!(WEB_SEARCH_TOOL_NAME));
        assert_eq!(body["tools"][0]["allowed_domains"], json!(["example.com"]));
        assert_eq!(body["messages"][0]["role"], json!("user"));
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        // No blocked_domains key when includes are present.
        assert!(body["tools"][0].get("blocked_domains").is_none());
    }

    #[test]
    fn page_age_labels_parse_into_seconds() {
        assert_eq!(parse_page_age(Some("2 days ago")), Some(172_800.0));
        assert_eq!(parse_page_age(Some("3h ago")), Some(10_800.0));
        assert_eq!(parse_page_age(Some("1 week")), Some(604_800.0));
        assert_eq!(parse_page_age(Some("5 minutes ago")), Some(300.0));
        assert_eq!(parse_page_age(None), None);
        assert_eq!(parse_page_age(Some("recently")), None);
    }

    #[test]
    fn a_full_response_maps_onto_answer_sources_citations_and_queries() {
        let response = json!({
            "id": "msg_1",
            "model": "claude-haiku-4-5",
            "content": [
                { "type": "server_tool_use", "name": "web_search", "input": { "query": "rust async" } },
                { "type": "web_search_tool_result", "content": [
                    { "type": "web_search_result", "title": "Async Rust", "url": "https://a", "page_age": "2 days ago" },
                    { "type": "web_search_result", "title": "Tokio", "url": "https://b", "page_age": null }
                ]},
                { "type": "text", "text": "Async in Rust uses futures.", "citations": [
                    { "url": "https://a", "title": "Async Rust", "cited_text": "futures" }
                ]}
            ],
            "usage": { "input_tokens": 10, "output_tokens": 20, "server_tool_use": { "web_search_requests": 1 } }
        });
        let parsed = parse_response(&response);
        assert_eq!(parsed.provider, "anthropic");
        assert_eq!(
            parsed.answer.as_deref(),
            Some("Async in Rust uses futures.")
        );
        assert_eq!(parsed.sources.len(), 2);
        assert_eq!(parsed.sources[0].title, "Async Rust");
        assert_eq!(parsed.sources[0].age_seconds, Some(172_800.0));
        assert!(parsed.sources[1].age_seconds.is_none());
        assert_eq!(parsed.search_queries, vec!["rust async"]);
        assert_eq!(parsed.citations.len(), 1);
        assert_eq!(parsed.citations[0].cited_text.as_deref(), Some("futures"));
        assert_eq!(parsed.usage.and_then(|u| u.search_requests), Some(1));
        assert_eq!(parsed.model.as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn an_api_key_request_uses_x_api_key_and_no_beta() {
        // The API-key scheme must not leak the OAuth-only headers.
        let cred = Credential::ApiKey("sk-test".to_string());
        let builder = reqwest::Client::new().post("https://api.anthropic.com/v1/messages");
        let req = authed_request(builder, &cred, None)
            .build()
            .expect("request builds");
        assert_eq!(
            req.headers().get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-test")
        );
        assert!(req.headers().get("authorization").is_none());
        assert!(req.headers().get("anthropic-beta").is_none());
        assert_eq!(
            req.headers()
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok()),
            Some("2023-06-01")
        );
    }

    #[test]
    fn an_oauth_request_uses_bearer_and_the_stealth_headers() {
        let cred = Credential::OAuth("oauth-token".to_string());
        let builder = reqwest::Client::new().post("https://api.anthropic.com/v1/messages");
        let req = authed_request(builder, &cred, Some("beta-a,beta-b"))
            .build()
            .expect("request builds");
        assert_eq!(
            req.headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer oauth-token")
        );
        assert!(req.headers().get("x-api-key").is_none());
        assert_eq!(
            req.headers()
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok()),
            Some("beta-a,beta-b")
        );
        assert_eq!(
            req.headers().get("x-app").and_then(|v| v.to_str().ok()),
            Some("cli")
        );
    }
}
