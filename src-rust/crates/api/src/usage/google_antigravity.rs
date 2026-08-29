//! Google Antigravity / Cloud Code usage, ported from oh-my-pi `usage/gemini.ts`.
//!
//! Endpoint only: a POST to `/v1internal:retrieveUserQuota` on the Cloud Code
//! plane returns `buckets[]`, each `{ modelId, remainingFraction, resetTime }`.
//! Used is `1 - remainingFraction`.

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_iso_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://cloudcode-pa.googleapis.com";

pub struct GoogleAntigravityUsage;

/// Parses one `buckets[]` entry into a limit.
fn bucket_limit(bucket: &serde_json::Value) -> Option<UsageLimit> {
    let remaining_fraction = bucket.get("remainingFraction").and_then(|v| v.as_f64())?;
    let used_fraction = (1.0 - remaining_fraction).clamp(0.0, 1.0);
    let model = bucket
        .get("modelId")
        .and_then(|v| v.as_str())
        .unwrap_or("quota");
    let resets_at = bucket
        .get("resetTime")
        .and_then(|v| v.as_str())
        .and_then(parse_iso_timestamp);
    Some(
        UsageLimit {
            id: model.to_string(),
            label: model.to_string(),
            scope: None,
            window: resets_at.map(|r| UsageWindow {
                id: model.to_string(),
                label: model.to_string(),
                duration_ms: None,
                resets_at: Some(r),
                reset_label: None,
            }),
            amount: UsageAmount {
                remaining_fraction: Some(remaining_fraction.clamp(0.0, 1.0)),
                ..UsageAmount::from_used_fraction(used_fraction)
            },
            status: None,
            notes: Vec::new(),
        }
        .with_derived_status(),
    )
}

/// Parses the `retrieveUserQuota` body. Split out for fixture testing.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let limits = body
        .get("buckets")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(bucket_limit).collect())
        .unwrap_or_default();
    UsageReport {
        provider: provider.to_string(),
        fetched_at: now_ms(),
        limits,
        notes: Vec::new(),
    }
}

#[async_trait]
impl UsageProvider for GoogleAntigravityUsage {
    fn id(&self) -> &str {
        "google-antigravity"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref() else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!(
            "{}/v1internal:retrieveUserQuota",
            base.trim_end_matches('/')
        );
        let resp = ctx
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Cloud Code quota endpoint returned {}", resp.status());
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
    fn body_parses_buckets_and_inverts_remaining_fraction() {
        let body = serde_json::json!({
            "buckets": [
                { "modelId": "gemini-3-pro", "remainingFraction": 0.6, "resetTime": "2023-11-14T22:13:20Z" },
                { "modelId": "gemini-3-flash", "remainingFraction": 0.05 }
            ]
        });
        let report = parse_usage_body("google-antigravity", &body);
        assert_eq!(report.limits.len(), 2);
        assert_eq!(report.limits[0].label, "gemini-3-pro");
        // remaining 0.6 -> used 0.4.
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.4));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
        // remaining 0.05 -> used 0.95 -> warning.
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
    }
}
