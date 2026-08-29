//! Kimi Code usage, ported from oh-my-pi `usage/kimi.ts`.
//!
//! Endpoint only: `usages` returns `{ usage, limits: [{ detail, window }] }`,
//! each entry carrying `used`/`limit`/`remaining` (under `detail` or at the
//! entry) and a reset under several possible keys.

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_iso_timestamp, parse_positive_timestamp, UsageAmount, UsageLimit, UsageReport,
    UsageWindow,
};
use async_trait::async_trait;

const DEFAULT_BASE: &str = "https://api.kimi.com/coding/v1";

pub struct KimiUsage;

/// Reads `key` as f64 from `obj`, or from `obj.detail` as a fallback.
fn field(entry: &serde_json::Value, key: &str) -> Option<f64> {
    entry.get(key).and_then(|v| v.as_f64()).or_else(|| {
        entry
            .get("detail")
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_f64())
    })
}

/// Reads a reset timestamp from the entry or its `window`, trying the several
/// key spellings Kimi uses.
fn reset_at(entry: &serde_json::Value) -> Option<u64> {
    let scopes = [Some(entry), entry.get("window"), entry.get("detail")];
    for scope in scopes.into_iter().flatten() {
        for key in ["reset_at", "resetAt", "reset_time", "resetTime"] {
            if let Some(v) = scope.get(key) {
                if let Some(ms) = v
                    .as_str()
                    .and_then(parse_iso_timestamp)
                    .or_else(|| parse_positive_timestamp(v))
                {
                    return Some(ms);
                }
            }
        }
    }
    None
}

/// Parses one `limits[]` entry into a limit, or `None` when it has no usable
/// used/limit numbers.
fn entry_limit(idx: usize, entry: &serde_json::Value) -> Option<UsageLimit> {
    let used = field(entry, "used");
    let limit = field(entry, "limit");
    let remaining = field(entry, "remaining");
    let used_fraction = match (used, limit) {
        (Some(u), Some(l)) if l > 0.0 => (u / l).clamp(0.0, 1.0),
        _ => match (remaining, limit) {
            (Some(r), Some(l)) if l > 0.0 => (1.0 - r / l).clamp(0.0, 1.0),
            _ => return None,
        },
    };
    let label = entry
        .get("name")
        .or_else(|| entry.get("type"))
        .or_else(|| entry.get("detail").and_then(|d| d.get("name")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("limit {}", idx + 1));
    let resets_at = reset_at(entry);
    Some(
        UsageLimit {
            id: format!("kimi:{idx}"),
            label: label.clone(),
            scope: None,
            window: resets_at.map(|r| UsageWindow {
                id: format!("kimi:{idx}"),
                label,
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
                unit: super::types::UsageUnit::Tokens,
            },
            status: None,
            notes: Vec::new(),
        }
        .with_derived_status(),
    )
}

/// Parses the `usages` body. Split out for fixture testing.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let limits = body
        .get("limits")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(idx, entry)| entry_limit(idx, entry))
                .collect()
        })
        .unwrap_or_default();
    UsageReport {
        provider: provider.to_string(),
        fetched_at: now_ms(),
        limits,
        notes: Vec::new(),
    }
}

#[async_trait]
impl UsageProvider for KimiUsage {
    fn id(&self) -> &str {
        "kimi-code"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.access_token.as_deref().or(ctx.api_key.as_deref()) else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_BASE);
        let url = format!("{}/usages", base.trim_end_matches('/'));
        let resp = ctx.client.get(&url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Kimi usage endpoint returned {}", resp.status());
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
    fn body_parses_nested_detail_and_window() {
        let body = serde_json::json!({
            "usage": {},
            "limits": [
                { "name": "weekly", "detail": { "used": 95.0, "limit": 100.0, "remaining": 5.0 },
                  "window": { "reset_at": 1700000000 } }
            ]
        });
        let report = parse_usage_body("kimi-code", &body);
        assert_eq!(report.limits.len(), 1);
        assert_eq!(report.limits[0].label, "weekly");
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.95));
        assert_eq!(report.limits[0].status, Some(UsageStatus::Warning));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
    }
}
