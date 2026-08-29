//! OpenCode Go usage, ported from oh-my-pi `usage/opencode-go.ts`.
//!
//! Endpoint only: `/v1/usage` returns `usage` keyed by window, each `{ percent,
//! status, resetsAt }`. `percent` is the used percent (0..100).

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_iso_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://opencode.ai/zen/go";

pub struct OpencodeGoUsage;

fn entry_limit(name: &str, obj: &serde_json::Value) -> Option<UsageLimit> {
    let percent = obj.get("percent").and_then(|v| v.as_f64())?;
    let resets_at = obj
        .get("resetsAt")
        .and_then(|v| v.as_str())
        .and_then(parse_iso_timestamp);
    Some(
        UsageLimit {
            id: name.to_string(),
            label: name.to_string(),
            scope: None,
            window: resets_at.map(|r| UsageWindow {
                id: name.to_string(),
                label: name.to_string(),
                duration_ms: None,
                resets_at: Some(r),
                reset_label: None,
            }),
            amount: UsageAmount::from_used_fraction((percent / 100.0).clamp(0.0, 1.0)),
            status: None,
            notes: Vec::new(),
        }
        .with_derived_status(),
    )
}

fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let usage = body
        .get("payload")
        .and_then(|p| p.get("usage"))
        .or_else(|| body.get("usage"));
    let mut limits = Vec::new();
    if let Some(map) = usage.and_then(|v| v.as_object()) {
        for (name, obj) in map {
            if let Some(limit) = entry_limit(name, obj) {
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
impl UsageProvider for OpencodeGoUsage {
    fn id(&self) -> &str {
        "opencode-go"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref().or(ctx.api_key.as_deref()) else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!("{}/v1/usage", base.trim_end_matches('/'));
        let resp = ctx.client.get(&url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("OpenCode Go usage endpoint returned {}", resp.status());
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
    fn body_parses_percent_windows_under_payload() {
        let body = serde_json::json!({
            "payload": { "usage": {
                "daily": { "percent": 95.0, "status": "ok", "resetsAt": "2023-11-14T22:13:20Z" }
            } }
        });
        let report = parse_usage_body("opencode-go", &body);
        assert_eq!(report.limits.len(), 1);
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.95));
        assert_eq!(report.limits[0].status, Some(UsageStatus::Warning));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
    }
}
