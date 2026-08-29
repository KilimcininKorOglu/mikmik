//! Umans usage, ported from oh-my-pi `usage/umans.ts`.
//!
//! Endpoint only: `/v1/usage` reports a request window under `payload` with
//! `requests_in_window` used out of `limits.requests.limit`, and a
//! `window.resets_at`.

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_iso_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://api.code.umans.ai";

pub struct UmansUsage;

fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let root = body.get("payload").unwrap_or(body);
    let used = root
        .get("requests_in_window")
        .or_else(|| root.get("weighted_in_window"))
        .and_then(|v| v.as_f64());
    let limit = root
        .get("limits")
        .and_then(|l| l.get("requests"))
        .and_then(|r| r.get("limit"))
        .and_then(|v| v.as_f64());
    let remaining = root
        .get("remaining_requests")
        .or_else(|| root.get("weighted_remaining_requests"))
        .and_then(|v| v.as_f64());
    let resets_at = root
        .get("window")
        .and_then(|w| w.get("resets_at"))
        .and_then(|v| v.as_str())
        .and_then(parse_iso_timestamp);

    let mut limits = Vec::new();
    let used_fraction = match (used, limit) {
        (Some(u), Some(l)) if l > 0.0 => Some((u / l).clamp(0.0, 1.0)),
        _ => match (remaining, limit) {
            (Some(r), Some(l)) if l > 0.0 => Some((1.0 - r / l).clamp(0.0, 1.0)),
            _ => None,
        },
    };
    if let Some(frac) = used_fraction {
        limits.push(
            UsageLimit {
                id: "requests".into(),
                label: "Requests".into(),
                scope: None,
                window: resets_at.map(|r| UsageWindow {
                    id: "requests".into(),
                    label: "Requests".into(),
                    duration_ms: None,
                    resets_at: Some(r),
                    reset_label: None,
                }),
                amount: UsageAmount {
                    used,
                    limit,
                    remaining,
                    used_fraction: Some(frac),
                    remaining_fraction: Some((1.0 - frac).max(0.0)),
                    unit: super::types::UsageUnit::Requests,
                },
                status: None,
                notes: Vec::new(),
            }
            .with_derived_status(),
        );
    }
    UsageReport {
        provider: provider.to_string(),
        fetched_at: now_ms(),
        limits,
        notes: Vec::new(),
    }
}

#[async_trait]
impl UsageProvider for UmansUsage {
    fn id(&self) -> &str {
        "umans"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref().or(ctx.api_key.as_deref()) else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!("{}/v1/usage", base.trim_end_matches('/'));
        let resp = ctx.client.get(&url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Umans usage endpoint returned {}", resp.status());
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
    fn body_parses_the_request_window() {
        let body = serde_json::json!({
            "payload": {
                "requests_in_window": 90.0,
                "remaining_requests": 10.0,
                "limits": { "requests": { "limit": 100.0 } },
                "window": { "resets_at": "2023-11-14T22:13:20Z" }
            }
        });
        let report = parse_usage_body("umans", &body);
        assert_eq!(report.limits.len(), 1);
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.9));
        assert_eq!(report.limits[0].status, Some(UsageStatus::Warning));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
    }
}
