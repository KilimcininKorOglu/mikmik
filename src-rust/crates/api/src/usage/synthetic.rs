//! Synthetic.new usage, ported from oh-my-pi `usage/synthetic.ts`.
//!
//! Endpoint only: `/v2/quotas` returns a `rollingFiveHourLimit` (remaining/max,
//! `nextTickAt`) and a `weeklyTokenLimit` (remainingCredits/maxCredits,
//! `percentRemaining`, `nextRegenAt`).

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_iso_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://api.synthetic.new/v2";

pub struct SyntheticUsage;

fn build_limit(
    id: &str,
    label: &str,
    used_fraction: f64,
    used: Option<f64>,
    limit: Option<f64>,
    resets_at: Option<u64>,
) -> UsageLimit {
    UsageLimit {
        id: id.to_string(),
        label: label.to_string(),
        scope: None,
        window: resets_at.map(|r| UsageWindow {
            id: id.to_string(),
            label: label.to_string(),
            duration_ms: None,
            resets_at: Some(r),
            reset_label: None,
        }),
        amount: UsageAmount {
            used,
            limit,
            remaining: match (limit, used) {
                (Some(l), Some(u)) => Some((l - u).max(0.0)),
                _ => None,
            },
            used_fraction: Some(used_fraction.clamp(0.0, 1.0)),
            remaining_fraction: Some((1.0 - used_fraction).clamp(0.0, 1.0)),
            unit: super::types::UsageUnit::Tokens,
        },
        status: None,
        notes: Vec::new(),
    }
    .with_derived_status()
}

fn iso(obj: &serde_json::Value, key: &str) -> Option<u64> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .and_then(parse_iso_timestamp)
}

fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let mut limits = Vec::new();
    if let Some(rolling) = body.get("rollingFiveHourLimit") {
        let remaining = rolling.get("remaining").and_then(|v| v.as_f64());
        let max = rolling.get("max").and_then(|v| v.as_f64());
        let used_fraction = match (remaining, max) {
            (Some(r), Some(m)) if m > 0.0 => (1.0 - r / m).clamp(0.0, 1.0),
            _ => rolling
                .get("tickPercent")
                .and_then(|v| v.as_f64())
                .map(|p| (p / 100.0).clamp(0.0, 1.0))
                .unwrap_or(0.0),
        };
        let used = match (max, remaining) {
            (Some(m), Some(r)) => Some(m - r),
            _ => None,
        };
        limits.push(build_limit(
            "5h",
            "5 hour",
            used_fraction,
            used,
            max,
            iso(rolling, "nextTickAt"),
        ));
    }
    if let Some(weekly) = body.get("weeklyTokenLimit") {
        let remaining = weekly.get("remainingCredits").and_then(|v| v.as_f64());
        let max = weekly.get("maxCredits").and_then(|v| v.as_f64());
        let used_fraction = weekly
            .get("percentRemaining")
            .and_then(|v| v.as_f64())
            .map(|p| (1.0 - p / 100.0).clamp(0.0, 1.0))
            .or_else(|| match (remaining, max) {
                (Some(r), Some(m)) if m > 0.0 => Some((1.0 - r / m).clamp(0.0, 1.0)),
                _ => None,
            })
            .unwrap_or(0.0);
        let used = match (max, remaining) {
            (Some(m), Some(r)) => Some(m - r),
            _ => None,
        };
        limits.push(build_limit(
            "7d",
            "7 day",
            used_fraction,
            used,
            max,
            iso(weekly, "nextRegenAt"),
        ));
    }
    UsageReport {
        provider: provider.to_string(),
        fetched_at: now_ms(),
        limits,
        notes: Vec::new(),
    }
}

#[async_trait]
impl UsageProvider for SyntheticUsage {
    fn id(&self) -> &str {
        "synthetic"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref().or(ctx.api_key.as_deref()) else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!("{}/quotas", base.trim_end_matches('/'));
        let resp = ctx.client.get(&url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Synthetic usage endpoint returned {}", resp.status());
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
    fn body_parses_rolling_and_weekly_limits() {
        let body = serde_json::json!({
            "rollingFiveHourLimit": { "remaining": 60.0, "max": 100.0, "nextTickAt": "2023-11-14T22:13:20Z" },
            "weeklyTokenLimit": { "remainingCredits": 5.0, "maxCredits": 100.0, "percentRemaining": 5.0 }
        });
        let report = parse_usage_body("synthetic", &body);
        assert_eq!(report.limits.len(), 2);
        // 60/100 remaining -> 40% used.
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.4));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
        // 5% remaining -> 95% used -> warning.
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
    }
}
