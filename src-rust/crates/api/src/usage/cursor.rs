//! Cursor usage, ported from oh-my-pi `usage/cursor.ts`.
//!
//! Endpoint only: `/auth/usage` returns an object keyed by meter, each with a
//! used and a limit under one of several field spellings, and a reset under a
//! few more. Field names vary, so each is looked up against a list.

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_iso_timestamp, parse_positive_timestamp, UsageAmount, UsageLimit, UsageReport,
    UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://api2.cursor.sh";
const USED_KEYS: &[&str] = &["numRequests", "used", "amountUsed", "usdUsed"];
const LIMIT_KEYS: &[&str] = &["maxRequestUsage", "limit", "amountLimit", "usdLimit"];
const RESET_KEYS: &[&str] = &["billingCycleEnd", "endOfMonth", "resetsAt", "nextReset"];

fn first_f64(obj: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_f64()))
}

fn first_reset(obj: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| {
        obj.get(*k).and_then(|v| {
            v.as_str()
                .and_then(parse_iso_timestamp)
                .or_else(|| parse_positive_timestamp(v))
        })
    })
}

/// Parses one meter object into a limit, or `None` when it has no used/limit.
fn meter_limit(name: &str, obj: &serde_json::Value) -> Option<UsageLimit> {
    let used = first_f64(obj, USED_KEYS);
    let limit = first_f64(obj, LIMIT_KEYS);
    let used_fraction = match (used, limit) {
        (Some(u), Some(l)) if l > 0.0 => (u / l).clamp(0.0, 1.0),
        _ => return None,
    };
    let resets_at = first_reset(obj, RESET_KEYS);
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
            amount: UsageAmount {
                used,
                limit,
                remaining: match (limit, used) {
                    (Some(l), Some(u)) => Some((l - u).max(0.0)),
                    _ => None,
                },
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

/// Parses the `/auth/usage` body: an object of meters. Split out for testing.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let mut limits = Vec::new();
    if let Some(map) = body.as_object() {
        for (name, obj) in map {
            if obj.is_object() {
                if let Some(limit) = meter_limit(name, obj) {
                    limits.push(limit);
                }
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
impl UsageProvider for CursorUsage {
    fn id(&self) -> &str {
        "cursor"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref().or(ctx.api_key.as_deref()) else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!("{}/auth/usage", base.trim_end_matches('/'));
        let resp = ctx.client.get(&url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Cursor usage endpoint returned {}", resp.status());
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(Some(parse_usage_body(self.id(), &body)))
    }
}

pub struct CursorUsage;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::types::UsageStatus;

    #[test]
    fn body_parses_meters_with_varied_field_names() {
        let body = serde_json::json!({
            "gpt-4": { "numRequests": 90.0, "maxRequestUsage": 100.0, "billingCycleEnd": "2023-11-14T22:13:20Z" },
            "meta": "ignored-non-object"
        });
        let report = parse_usage_body("cursor", &body);
        assert_eq!(report.limits.len(), 1);
        assert_eq!(report.limits[0].label, "gpt-4");
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.9));
        assert_eq!(report.limits[0].status, Some(UsageStatus::Warning));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
    }
}
