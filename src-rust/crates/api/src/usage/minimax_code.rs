//! MiniMax Coding Plan usage, ported from oh-my-pi `usage/minimax-code.ts`.
//!
//! Endpoint only: `/v1/token_plan/remains` returns `model_remains[]`, each with
//! an interval window and a weekly window reporting a remaining percent. Used
//! fraction is `(100 - remaining_percent) / 100`.

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_positive_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://api.minimax.io";

pub struct MinimaxCodeUsage;

/// Builds a limit from a `remaining_percent` (0..100), used counts, and a reset.
fn window_limit(
    id: String,
    label: String,
    remaining_percent: f64,
    used: Option<f64>,
    limit: Option<f64>,
    resets_at: Option<u64>,
) -> UsageLimit {
    let used_fraction = ((100.0 - remaining_percent) / 100.0).clamp(0.0, 1.0);
    UsageLimit {
        id: id.clone(),
        label: label.clone(),
        scope: None,
        window: resets_at.map(|r| UsageWindow {
            id,
            label,
            duration_ms: None,
            resets_at: Some(r),
            reset_label: None,
        }),
        amount: UsageAmount {
            used,
            limit,
            remaining: match (limit, used) {
                (Some(l), Some(u)) => Some(l - u),
                _ => None,
            },
            used_fraction: Some(used_fraction),
            remaining_fraction: Some((remaining_percent / 100.0).clamp(0.0, 1.0)),
            unit: super::types::UsageUnit::Requests,
        },
        status: None,
        notes: Vec::new(),
    }
    .with_derived_status()
}

/// Parses one `model_remains[]` entry into its interval and weekly limits.
fn model_limits(entry: &serde_json::Value) -> Vec<UsageLimit> {
    let model = entry
        .get("model_name")
        .and_then(|v| v.as_str())
        .unwrap_or("model");
    let mut out = Vec::new();
    if let Some(rp) = entry
        .get("current_interval_remaining_percent")
        .and_then(|v| v.as_f64())
    {
        out.push(window_limit(
            format!("{model}:interval"),
            format!("{model} · 5h"),
            rp,
            entry
                .get("current_interval_usage_count")
                .and_then(|v| v.as_f64()),
            entry
                .get("current_interval_total_count")
                .and_then(|v| v.as_f64()),
            entry.get("end_time").and_then(parse_positive_timestamp),
        ));
    }
    if let Some(rp) = entry
        .get("current_weekly_remaining_percent")
        .and_then(|v| v.as_f64())
    {
        out.push(window_limit(
            format!("{model}:weekly"),
            format!("{model} · 7d"),
            rp,
            entry
                .get("current_weekly_usage_count")
                .and_then(|v| v.as_f64()),
            entry
                .get("current_weekly_total_count")
                .and_then(|v| v.as_f64()),
            entry
                .get("weekly_end_time")
                .and_then(parse_positive_timestamp),
        ));
    }
    out
}

/// Parses the `token_plan/remains` body. Split out for fixture testing.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let limits = body
        .get("model_remains")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().flat_map(model_limits).collect())
        .unwrap_or_default();
    UsageReport {
        provider: provider.to_string(),
        fetched_at: now_ms(),
        limits,
        notes: Vec::new(),
    }
}

#[async_trait]
impl UsageProvider for MinimaxCodeUsage {
    fn id(&self) -> &str {
        "minimax-code"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref().or(ctx.api_key.as_deref()) else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!("{}/v1/token_plan/remains", base.trim_end_matches('/'));
        let resp = ctx.client.get(&url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("MiniMax usage endpoint returned {}", resp.status());
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
    fn body_parses_interval_and_weekly_windows_per_model() {
        let body = serde_json::json!({
            "base_resp": { "status_code": 0 },
            "model_remains": [{
                "model_name": "MiniMax-M2",
                "current_interval_remaining_percent": 60.0,
                "current_interval_usage_count": 40.0,
                "current_interval_total_count": 100.0,
                "end_time": 1700000000,
                "current_weekly_remaining_percent": 5.0
            }]
        });
        let report = parse_usage_body("minimax-code", &body);
        assert_eq!(report.limits.len(), 2);
        // interval: 60% remaining -> 40% used.
        assert_eq!(report.limits[0].label, "MiniMax-M2 · 5h");
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.4));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
        // weekly: 5% remaining -> 95% used -> warning.
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
    }
}
