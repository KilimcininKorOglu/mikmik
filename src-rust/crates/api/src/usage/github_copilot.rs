//! GitHub Copilot usage, ported from oh-my-pi `usage/github-copilot.ts`.
//!
//! Endpoint only: `/copilot_internal/user` returns a `quota_snapshots` map
//! keyed by meter (`chat`, `completions`, `premium_interactions`), each with an
//! `entitlement`, a `remaining`, and a `percent_remaining`.

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_iso_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://api.github.com";

pub struct GithubCopilotUsage;

fn meter_label(key: &str) -> String {
    match key {
        "premium_interactions" => "Premium".to_string(),
        "chat" => "Chat".to_string(),
        "completions" => "Completions".to_string(),
        other => other.to_string(),
    }
}

/// Builds a limit from one `quota_snapshots` entry. `unlimited` meters report no
/// usable fraction, so they are skipped.
fn snapshot_limit(
    key: &str,
    snap: &serde_json::Value,
    resets_at: Option<u64>,
) -> Option<UsageLimit> {
    if snap.get("unlimited").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    let entitlement = snap.get("entitlement").and_then(|v| v.as_f64());
    let remaining = snap.get("remaining").and_then(|v| v.as_f64());
    let used_fraction = snap
        .get("percent_remaining")
        .and_then(|v| v.as_f64())
        .map(|p| (1.0 - p / 100.0).clamp(0.0, 1.0))
        .or_else(|| match (entitlement, remaining) {
            (Some(e), Some(r)) if e > 0.0 => Some(((e - r) / e).clamp(0.0, 1.0)),
            _ => None,
        })?;
    let used = match (entitlement, remaining) {
        (Some(e), Some(r)) => Some(e - r),
        _ => None,
    };
    Some(
        UsageLimit {
            id: key.to_string(),
            label: meter_label(key),
            scope: None,
            window: resets_at.map(|r| UsageWindow {
                id: key.to_string(),
                label: meter_label(key),
                duration_ms: None,
                resets_at: Some(r),
                reset_label: None,
            }),
            amount: UsageAmount {
                used,
                limit: entitlement,
                remaining,
                used_fraction: Some(used_fraction),
                remaining_fraction: Some((1.0 - used_fraction).max(0.0)),
                unit: super::types::UsageUnit::Requests,
            },
            status: None,
            notes: Vec::new(),
        }
        .with_derived_status(),
    )
}

/// Parses the `/copilot_internal/user` body. Split out for fixture testing.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let resets_at = body
        .get("quota_reset_date")
        .and_then(|v| v.as_str())
        .and_then(parse_iso_timestamp);
    let mut limits = Vec::new();
    if let Some(snaps) = body.get("quota_snapshots").and_then(|v| v.as_object()) {
        for (key, snap) in snaps {
            if let Some(limit) = snapshot_limit(key, snap, resets_at) {
                limits.push(limit);
            }
        }
    }
    UsageReport {
        provider: provider.to_string(),
        fetched_at: now_ms(),
        limits,
        notes: Vec::new(),
    }
}

#[async_trait]
impl UsageProvider for GithubCopilotUsage {
    fn id(&self) -> &str {
        "github-copilot"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref().or(ctx.api_key.as_deref()) else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!("{}/copilot_internal/user", base.trim_end_matches('/'));
        let resp = ctx
            .client
            .get(&url)
            .bearer_auth(token)
            .header(reqwest::header::USER_AGENT, "mikmik")
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Copilot usage endpoint returned {}", resp.status());
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(Some(parse_usage_body(self.id(), &body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::types::UsageStatus;

    #[test]
    fn body_parses_quota_snapshots_and_skips_unlimited() {
        let body = serde_json::json!({
            "quota_reset_date": "2023-11-14T22:13:20Z",
            "quota_snapshots": {
                "premium_interactions": { "entitlement": 300.0, "remaining": 30.0, "percent_remaining": 10.0 },
                "chat": { "unlimited": true, "entitlement": 0.0, "remaining": 0.0 }
            }
        });
        let report = parse_usage_body("github-copilot", &body);
        assert_eq!(report.limits.len(), 1);
        let premium = &report.limits[0];
        assert_eq!(premium.label, "Premium");
        assert_eq!(premium.amount.used_fraction, Some(0.9));
        assert_eq!(premium.amount.used, Some(270.0));
        assert_eq!(premium.status, Some(UsageStatus::Warning));
        assert_eq!(
            premium.window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
    }
}
