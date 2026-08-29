// Shared GitHub API access for the GitHub-family handlers.
//
// The full repo/issue/commit handlers are ported separately; this module holds
// the pieces they all need: the authenticated `api.github.com` fetch and the
// token lookup.

use super::util::{load_page, LoadOptions};
use serde_json::Value;
use std::time::Duration;

/// The GitHub token: the stored credential first, then `GITHUB_TOKEN`/`GH_TOKEN`.
///
/// Rule: any key the app needs must be enterable from the TUI, so a value
/// written into the auth store under the `github` id wins; the env vars stay as
/// a headless fallback. A token is optional here and only raises the rate limit.
pub(super) fn github_token() -> Option<String> {
    let stored = mikmik_core::AuthStore::load().api_key_for("github");
    if let Some(token) = stored.filter(|t| !t.is_empty()) {
        return Some(token);
    }
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

/// GET `https://api.github.com{endpoint}` as JSON, or `None` on any failure.
pub(super) async fn fetch_github_api(endpoint: &str, timeout: Duration) -> Option<Value> {
    let mut headers = vec![(
        "Accept".to_string(),
        "application/vnd.github.v3+json".to_string(),
    )];
    if let Some(token) = github_token() {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    let result = load_page(
        &format!("https://api.github.com{endpoint}"),
        LoadOptions {
            timeout,
            headers,
            ..Default::default()
        },
    )
    .await;
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}
