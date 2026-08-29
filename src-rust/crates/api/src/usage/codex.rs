//! OpenAI Codex usage, ported from oh-my-pi `usage/openai-codex.ts`.
//!
//! Two windows share the chat meter (a ~5-hour primary and a 7-day secondary),
//! read from the `x-codex-{primary,secondary}-*` headers or the `wham/usage`
//! endpoint. Spark models meter separately, reported under
//! `additional_rate_limits`.

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_positive_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;
use reqwest::header::HeaderMap;

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;

pub struct CodexUsage;

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok()
}

/// A human label for a window from its length: the two Codex windows are ~5h
/// and 7d, but fall back to hours/days for anything else.
fn window_label(duration_ms: Option<u64>, fallback: &str) -> String {
    match duration_ms {
        Some(ms) if (4 * HOUR_MS..=6 * HOUR_MS).contains(&ms) => "5 hour".to_string(),
        Some(ms) if (6 * DAY_MS..=8 * DAY_MS).contains(&ms) => "7 day".to_string(),
        Some(ms) if ms >= DAY_MS => format!("{} day", ms / DAY_MS),
        Some(ms) if ms >= HOUR_MS => format!("{} hour", ms / HOUR_MS),
        _ => fallback.to_string(),
    }
}

/// Builds a limit from a 0..100 `used_percent`, a window length, and a reset.
fn percent_limit(
    id: &str,
    duration_ms: Option<u64>,
    used_percent: f64,
    resets_at: Option<u64>,
    fallback_label: &str,
) -> UsageLimit {
    let label = window_label(duration_ms, fallback_label);
    UsageLimit {
        id: id.to_string(),
        label: label.clone(),
        scope: None,
        window: Some(UsageWindow {
            id: id.to_string(),
            label,
            duration_ms,
            resets_at,
            reset_label: None,
        }),
        amount: UsageAmount::from_used_fraction((used_percent / 100.0).clamp(0.0, 1.0)),
        status: None,
        notes: Vec::new(),
    }
    .with_derived_status()
}

/// Parses one `wham/usage` window object (`{ used_percent, limit_window_seconds,
/// reset_at }`) into a limit.
fn window_from_json(id: &str, obj: &serde_json::Value, fallback_label: &str) -> Option<UsageLimit> {
    let used_percent = obj.get("used_percent").and_then(|v| v.as_f64())?;
    let duration_ms = obj
        .get("limit_window_seconds")
        .and_then(|v| v.as_f64())
        .map(|s| (s * 1000.0) as u64);
    let resets_at = obj.get("reset_at").and_then(parse_positive_timestamp);
    Some(percent_limit(
        id,
        duration_ms,
        used_percent,
        resets_at,
        fallback_label,
    ))
}

#[async_trait]
impl UsageProvider for CodexUsage {
    fn id(&self) -> &str {
        "codex"
    }

    fn parse_rate_limit_headers(&self, headers: &HeaderMap, now_ms: u64) -> Option<UsageReport> {
        let mut limits = Vec::new();
        for (slot, id, fallback) in [
            ("primary", "primary", "primary"),
            ("secondary", "secondary", "secondary"),
        ] {
            let Some(used) = header_f64(headers, &format!("x-codex-{slot}-used-percent")) else {
                continue;
            };
            let duration_ms = header_f64(headers, &format!("x-codex-{slot}-window-minutes"))
                .map(|m| (m as u64) * MINUTE_MS);
            let resets_at = headers
                .get(format!("x-codex-{slot}-reset-at"))
                .and_then(|v| v.to_str().ok())
                .map(|s| serde_json::Value::String(s.trim().to_string()))
                .as_ref()
                .and_then(parse_positive_timestamp);
            limits.push(percent_limit(id, duration_ms, used, resets_at, fallback));
        }
        if limits.is_empty() {
            return None;
        }
        Some(UsageReport {
            provider: self.id().to_string(),
            fetched_at: now_ms,
            limits,
            notes: Vec::new(),
        })
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let (Some(token), Some(base)) = (ctx.access_token.as_deref(), ctx.base_url.as_deref())
        else {
            return Ok(None);
        };
        let url = format!("{}/wham/usage", base.trim_end_matches('/'));
        let resp = ctx.client.get(&url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Codex usage endpoint returned {}", resp.status());
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(Some(parse_usage_body(self.id(), &body)))
    }
}

/// Parses the `wham/usage` body: the two chat windows plus any Spark/extra
/// meters in `additional_rate_limits`. Split out for fixture testing.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let mut limits = Vec::new();
    if let Some(rl) = body.get("rate_limit") {
        if let Some(limit) = rl
            .get("primary_window")
            .and_then(|w| window_from_json("primary", w, "primary"))
        {
            limits.push(limit);
        }
        if let Some(limit) = rl
            .get("secondary_window")
            .and_then(|w| window_from_json("secondary", w, "secondary"))
        {
            limits.push(limit);
        }
    }
    // Spark and other metered features are reported separately, keyed by name.
    if let Some(extra) = body
        .get("additional_rate_limits")
        .and_then(|v| v.as_array())
    {
        for entry in extra {
            let name = entry
                .get("metered_feature")
                .or_else(|| entry.get("limit_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("extra");
            let Some(used) = entry.get("used_percent").and_then(|v| v.as_f64()) else {
                continue;
            };
            let duration_ms = entry
                .get("limit_window_seconds")
                .and_then(|v| v.as_f64())
                .map(|s| (s * 1000.0) as u64);
            let resets_at = entry.get("reset_at").and_then(parse_positive_timestamp);
            limits.push(percent_limit(name, duration_ms, used, resets_at, name));
        }
    }
    UsageReport {
        provider: provider.to_string(),
        fetched_at: now_ms(),
        limits,
        notes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::types::UsageStatus;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn header_parse_reads_primary_and_secondary_windows() {
        let h = headers(&[
            ("x-codex-primary-used-percent", "40"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1700000000"),
            ("x-codex-secondary-used-percent", "95"),
            ("x-codex-secondary-window-minutes", "10080"),
        ]);
        let report = CodexUsage
            .parse_rate_limit_headers(&h, 7)
            .expect("windows present");
        assert_eq!(report.limits.len(), 2);
        assert_eq!(report.limits[0].label, "5 hour");
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.4));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
        assert_eq!(report.limits[1].label, "7 day");
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
    }

    #[test]
    fn endpoint_body_parses_windows_and_the_spark_meter() {
        let body = serde_json::json!({
            "rate_limit": {
                "primary_window": { "used_percent": 10.0, "limit_window_seconds": 18000, "reset_at": 1700000000 },
                "secondary_window": { "used_percent": 100.0, "limit_window_seconds": 604800 }
            },
            "additional_rate_limits": [
                { "metered_feature": "spark", "used_percent": 50.0, "limit_window_seconds": 18000 }
            ]
        });
        let report = parse_usage_body("codex", &body);
        assert_eq!(report.limits.len(), 3);
        assert_eq!(report.limits[0].label, "5 hour");
        assert_eq!(report.limits[1].status, Some(UsageStatus::Exhausted));
        assert_eq!(report.limits[2].id, "spark");
        assert_eq!(report.limits[2].amount.used_fraction, Some(0.5));
    }
}
