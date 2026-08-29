//! Anthropic (Claude) usage, ported from oh-my-pi `usage/claude.ts`.
//!
//! Two sources: the `anthropic-ratelimit-unified-*` headers on a normal
//! response (free), and the `/api/oauth/usage` endpoint (an OAuth request with
//! richer per-model buckets).

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{
    now_ms, parse_positive_timestamp, UsageAmount, UsageLimit, UsageReport, UsageWindow,
};
use async_trait::async_trait;
use reqwest::header::HeaderMap;

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;
const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/api/oauth";

pub struct AnthropicUsage;

/// One unified rate-limit window as it appears in headers and buckets.
struct WindowSpec {
    /// The `{window}` token in `anthropic-ratelimit-unified-{window}-*`.
    header_token: &'static str,
    /// Stable meter id.
    id: &'static str,
    /// Human-readable label.
    label: &'static str,
    /// Window length in milliseconds.
    duration_ms: u64,
}

const WINDOWS: &[WindowSpec] = &[
    WindowSpec {
        header_token: "5h",
        id: "5h",
        label: "5 hour",
        duration_ms: 5 * HOUR_MS,
    },
    WindowSpec {
        header_token: "7d",
        id: "7d",
        label: "7 day",
        duration_ms: 7 * DAY_MS,
    },
    WindowSpec {
        header_token: "7d_oi",
        id: "7d_oi",
        label: "7 day (extra)",
        duration_ms: 7 * DAY_MS,
    },
];

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// Reads a fraction that may arrive as 0..1 or as a 0..100 percent. A value
/// above 1.5 is treated as a percent and scaled down, so both endpoint shapes
/// land on a 0..1 fraction.
fn fraction_from(value: f64) -> f64 {
    if value > 1.5 {
        (value / 100.0).min(1.0)
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn window_limit(spec: &WindowSpec, used_fraction: f64, resets_at: Option<u64>) -> UsageLimit {
    UsageLimit {
        id: spec.id.to_string(),
        label: spec.label.to_string(),
        scope: None,
        window: Some(UsageWindow {
            id: spec.id.to_string(),
            label: spec.label.to_string(),
            duration_ms: Some(spec.duration_ms),
            resets_at,
            reset_label: None,
        }),
        amount: UsageAmount::from_used_fraction(used_fraction),
        status: None,
        notes: Vec::new(),
    }
    .with_derived_status()
}

/// Parses one endpoint bucket (`{ utilization, resets_at }`) into a limit.
fn bucket_limit(spec: &WindowSpec, bucket: &serde_json::Value) -> Option<UsageLimit> {
    let util = bucket.get("utilization").and_then(|v| v.as_f64())?;
    let resets_at = bucket.get("resets_at").and_then(parse_positive_timestamp);
    Some(window_limit(spec, fraction_from(util), resets_at))
}

#[async_trait]
impl UsageProvider for AnthropicUsage {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn parse_rate_limit_headers(&self, headers: &HeaderMap, now_ms: u64) -> Option<UsageReport> {
        let _ = now_ms;
        let mut limits = Vec::new();
        for spec in WINDOWS {
            let util_name = format!(
                "anthropic-ratelimit-unified-{}-utilization",
                spec.header_token
            );
            let Some(util) =
                header_str(headers, &util_name).and_then(|s| s.trim().parse::<f64>().ok())
            else {
                continue;
            };
            let reset_name = format!("anthropic-ratelimit-unified-{}-reset", spec.header_token);
            let resets_at = header_str(headers, &reset_name)
                .map(|s| serde_json::Value::String(s.trim().to_string()))
                .as_ref()
                .and_then(parse_positive_timestamp);
            limits.push(window_limit(spec, fraction_from(util), resets_at));
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
        let Some(token) = ctx.access_token.as_deref() else {
            return Ok(None);
        };
        let base = ctx.base_url.as_deref().unwrap_or(DEFAULT_ENDPOINT);
        let url = format!("{}/usage", base.trim_end_matches('/'));
        let resp = ctx
            .client
            .get(&url)
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Anthropic usage endpoint returned {}", resp.status());
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(Some(parse_usage_body(self.id(), &body)))
    }
}

/// Parses the `/api/oauth/usage` body: the named buckets plus the generic
/// `limits[]` entries. Split out so a fixture can exercise it without a request.
fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let mut limits = Vec::new();
    // Named buckets. `seven_day_opus`/`seven_day_sonnet` share the 7-day window.
    let named: &[(&str, &WindowSpec)] = &[
        ("five_hour", &WINDOWS[0]),
        ("seven_day", &WINDOWS[1]),
        ("seven_day_opus", &WINDOWS[2]),
        ("seven_day_sonnet", &WINDOWS[2]),
    ];
    for (key, spec) in named {
        if let Some(bucket) = body.get(*key) {
            if bucket.is_object() {
                if let Some(mut limit) = bucket_limit(spec, bucket) {
                    // Disambiguate the two 7d_oi buckets by their key.
                    if *key == "seven_day_opus" {
                        limit.id = "7d_opus".into();
                        limit.label = "7 day (Opus)".into();
                    } else if *key == "seven_day_sonnet" {
                        limit.id = "7d_sonnet".into();
                        limit.label = "7 day (Sonnet)".into();
                    }
                    limits.push(limit);
                }
            }
        }
    }
    // Generic entries: `{ kind, percent, resets_at, scope.model.display_name }`.
    if let Some(entries) = body.get("limits").and_then(|v| v.as_array()) {
        for entry in entries {
            let Some(percent) = entry.get("percent").and_then(|v| v.as_f64()) else {
                continue;
            };
            let resets_at = entry.get("resets_at").and_then(parse_positive_timestamp);
            let label = entry
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name"))
                .and_then(|v| v.as_str())
                .or_else(|| entry.get("kind").and_then(|v| v.as_str()))
                .unwrap_or("limit")
                .to_string();
            let id = entry
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or(&label)
                .to_string();
            limits.push(
                UsageLimit {
                    id,
                    label,
                    scope: None,
                    window: resets_at.map(|r| UsageWindow {
                        id: "generic".into(),
                        label: "reset".into(),
                        duration_ms: None,
                        resets_at: Some(r),
                        reset_label: None,
                    }),
                    amount: UsageAmount::from_used_fraction(fraction_from(percent)),
                    status: None,
                    notes: Vec::new(),
                }
                .with_derived_status(),
            );
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
    fn header_parse_builds_a_limit_per_present_window() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "1700000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.95"),
        ]);
        let report = AnthropicUsage
            .parse_rate_limit_headers(&h, 123)
            .expect("some windows present");
        assert_eq!(report.fetched_at, 123);
        assert_eq!(report.limits.len(), 2);
        let five = &report.limits[0];
        assert_eq!(five.id, "5h");
        assert_eq!(five.amount.used_fraction, Some(0.42));
        assert_eq!(
            five.window.as_ref().unwrap().resets_at,
            Some(1_700_000_000_000)
        );
        assert_eq!(five.status, Some(UsageStatus::Ok));
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
    }

    #[test]
    fn no_rate_limit_headers_yields_none() {
        let h = headers(&[("content-type", "application/json")]);
        assert!(AnthropicUsage.parse_rate_limit_headers(&h, 0).is_none());
    }

    #[test]
    fn endpoint_body_parses_named_buckets_and_generic_limits() {
        let body = serde_json::json!({
            "five_hour": { "utilization": 0.5, "resets_at": 1700000000 },
            "seven_day": { "utilization": 91.0, "resets_at": 1700600000 },
            "limits": [
                { "kind": "opus", "percent": 100.0, "resets_at": 1700600000,
                  "scope": { "model": { "display_name": "Claude Opus" } } }
            ]
        });
        let report = parse_usage_body("anthropic", &body);
        assert_eq!(report.limits.len(), 3);
        // 5h fraction kept as-is.
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.5));
        // 7d percent 91 scaled to 0.91 → warning.
        assert_eq!(report.limits[1].amount.used_fraction, Some(0.91));
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
        // generic entry labeled by model display name, 100% → exhausted.
        assert_eq!(report.limits[2].label, "Claude Opus");
        assert_eq!(report.limits[2].status, Some(UsageStatus::Exhausted));
    }
}
