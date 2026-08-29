//! Z.AI (GLM Coding Plan) usage, ported from oh-my-pi `usage/zai.ts`.
//!
//! Endpoint only (`/api/monitor/usage/quota/limit`); the token rides in the
//! `Authorization` header with no `Bearer` prefix. Each entry is a meter keyed
//! by `type` (`TOKENS_LIMIT`, `TIME_LIMIT`, `CREDIT_LIMIT`).

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_positive_timestamp, UsageAmount, UsageLimit, UsageReport, UsageUnit, UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://api.z.ai";

pub struct ZaiUsage;

/// A friendly label for a Z.AI meter `type`.
fn type_label(kind: &str) -> String {
    match kind {
        "TOKENS_LIMIT" => "Tokens".to_string(),
        "TIME_LIMIT" => "Time".to_string(),
        "CREDIT_LIMIT" => "Credits".to_string(),
        other => other.to_string(),
    }
}

fn unit_for(kind: &str) -> UsageUnit {
    match kind {
        "TOKENS_LIMIT" => UsageUnit::Tokens,
        "CREDIT_LIMIT" => UsageUnit::Credits,
        _ => UsageUnit::Percent,
    }
}

/// Parses one quota entry (`{ type, currentValue, usage, remaining, percentage,
/// nextResetTime }`) into a limit. `currentValue` is the used amount and `usage`
/// is the cap; `percentage` is the used percent when present.
fn entry_limit(entry: &serde_json::Value) -> Option<UsageLimit> {
    let kind = entry
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("limit");
    let used = entry.get("currentValue").and_then(|v| v.as_f64());
    let limit = entry.get("usage").and_then(|v| v.as_f64());
    let remaining = entry.get("remaining").and_then(|v| v.as_f64());
    let used_fraction = entry
        .get("percentage")
        .and_then(|v| v.as_f64())
        .map(|p| (p / 100.0).clamp(0.0, 1.0))
        .or_else(|| match (used, limit) {
            (Some(u), Some(l)) if l > 0.0 => Some((u / l).clamp(0.0, 1.0)),
            _ => None,
        })?;
    let resets_at = entry
        .get("nextResetTime")
        .and_then(parse_positive_timestamp);
    Some(
        UsageLimit {
            id: kind.to_string(),
            label: type_label(kind),
            scope: None,
            window: resets_at.map(|r| UsageWindow {
                id: kind.to_string(),
                label: type_label(kind),
                duration_ms: None,
                resets_at: Some(r),
                reset_label: None,
            }),
            amount: UsageAmount {
                used,
                limit,
                remaining,
                used_fraction: Some(used_fraction),
                remaining_fraction: Some((1.0 - used_fraction).max(0.0)),
                unit: unit_for(kind),
            },
            status: None,
            notes: Vec::new(),
        }
        .with_derived_status(),
    )
}

/// Parses the quota body: the entries live under `data` or at the top level.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let entries = body
        .get("data")
        .and_then(|v| v.as_array())
        .or_else(|| body.as_array());
    let limits = entries
        .map(|arr| arr.iter().filter_map(entry_limit).collect())
        .unwrap_or_default();
    UsageReport {
        provider: provider.to_string(),
        fetched_at: now_ms(),
        limits,
        notes: Vec::new(),
    }
}

#[async_trait]
impl UsageProvider for ZaiUsage {
    fn id(&self) -> &str {
        "zai"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref().or(ctx.api_key.as_deref()) else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!(
            "{}/api/monitor/usage/quota/limit",
            base.trim_end_matches('/')
        );
        // Z.AI takes the token in Authorization with no `Bearer` prefix.
        let resp = ctx
            .client
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, token)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Z.AI usage endpoint returned {}", resp.status());
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
    fn body_parses_each_quota_entry() {
        let body = serde_json::json!({
            "data": [
                { "type": "TOKENS_LIMIT", "currentValue": 40.0, "usage": 100.0, "remaining": 60.0, "percentage": 40.0, "nextResetTime": 1700000000 },
                { "type": "CREDIT_LIMIT", "currentValue": 95.0, "usage": 100.0, "percentage": 95.0 }
            ]
        });
        let report = parse_usage_body("zai", &body);
        assert_eq!(report.limits.len(), 2);
        assert_eq!(report.limits[0].label, "Tokens");
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.4));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
    }

    #[test]
    fn falls_back_to_used_over_limit_without_percentage() {
        let body = serde_json::json!({ "data": [
            { "type": "TIME_LIMIT", "currentValue": 100.0, "usage": 100.0 }
        ] });
        let report = parse_usage_body("zai", &body);
        assert_eq!(report.limits[0].amount.used_fraction, Some(1.0));
        assert_eq!(report.limits[0].status, Some(UsageStatus::Exhausted));
    }
}
