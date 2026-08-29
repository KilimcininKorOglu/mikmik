//! xAI (SuperGrok) usage, ported from oh-my-pi `usage/xai-oauth.ts`.
//!
//! Endpoint only. The billing URL is built by the xAI OAuth registry, so the
//! caller supplies it as `base_url`. The body reports a weekly credit percent
//! (`creditUsagePercent`, with `currentPeriod.end`) and/or a monthly included
//! quota (`used`/`monthlyLimit`, with `billingPeriodEnd`).

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_iso_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;

pub struct XaiOAuthUsage;

fn iso_at(value: Option<&serde_json::Value>) -> Option<u64> {
    value?.as_str().and_then(parse_iso_timestamp)
}

fn percent_limit(id: &str, label: &str, used_percent: f64, resets_at: Option<u64>) -> UsageLimit {
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
        amount: UsageAmount::from_used_fraction((used_percent / 100.0).clamp(0.0, 1.0)),
        status: None,
        notes: Vec::new(),
    }
    .with_derived_status()
}

/// Parses the billing body. Split out for fixture testing.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let mut limits = Vec::new();
    // Weekly credits: a used percent plus a period end.
    if let Some(pct) = body.get("creditUsagePercent").and_then(|v| v.as_f64()) {
        let resets_at = iso_at(body.get("currentPeriod").and_then(|p| p.get("end")));
        limits.push(percent_limit(
            "weekly-credits",
            "Weekly credits",
            pct,
            resets_at,
        ));
    }
    // Monthly included quota: used out of monthlyLimit.
    if let (Some(used), Some(limit)) = (
        body.get("used").and_then(|v| v.as_f64()),
        body.get("monthlyLimit").and_then(|v| v.as_f64()),
    ) {
        if limit > 0.0 {
            let resets_at = iso_at(body.get("billingPeriodEnd"));
            let mut ul = percent_limit(
                "monthly-quota",
                "Monthly quota",
                (used / limit) * 100.0,
                resets_at,
            );
            ul.amount.used = Some(used);
            ul.amount.limit = Some(limit);
            ul.amount.remaining = Some((limit - used).max(0.0));
            limits.push(ul);
        }
    }
    // Per-product weekly usage.
    if let Some(products) = body.get("productUsage").and_then(|v| v.as_array()) {
        for entry in products {
            let Some(pct) = entry.get("usagePercent").and_then(|v| v.as_f64()) else {
                continue;
            };
            let name = entry
                .get("product")
                .and_then(|v| v.as_str())
                .unwrap_or("product");
            limits.push(percent_limit(name, name, pct, None));
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
impl UsageProvider for XaiOAuthUsage {
    fn id(&self) -> &str {
        "xai-oauth"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        // The billing URL is registry-built; without it there is nothing to hit.
        let (Some(token), Some(url)) = (ctx.access_token.as_deref(), ctx.base_url.as_deref())
        else {
            return Ok(None);
        };
        let resp = ctx.client.get(url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("xAI billing endpoint returned {}", resp.status());
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
    fn body_parses_weekly_credits_and_monthly_quota() {
        let body = serde_json::json!({
            "creditUsagePercent": 92.0,
            "currentPeriod": { "end": "2023-11-14T22:13:20Z" },
            "used": 30.0,
            "monthlyLimit": 100.0,
            "billingPeriodEnd": "2023-11-14T22:13:20Z"
        });
        let report = parse_usage_body("xai-oauth", &body);
        assert_eq!(report.limits.len(), 2);
        assert_eq!(report.limits[0].label, "Weekly credits");
        assert_eq!(report.limits[0].status, Some(UsageStatus::Warning));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
        assert_eq!(report.limits[1].amount.used_fraction, Some(0.3));
        assert_eq!(report.limits[1].amount.used, Some(30.0));
    }
}
