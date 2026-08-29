//! Ollama Cloud usage, ported from oh-my-pi `usage/ollama.ts`.
//!
//! Endpoint only: `GET https://ollama.com/api/usage` (Bearer) returns
//! `limits.session` and `limits.weekly`, each a normalized 0..1 `usage`
//! fraction of the 5-hour / 7-day allowance plus per-model `request_count`s.
//! Local `ollama` has no quota, so only `ollama-cloud` registers a reporter.

use super::provider::{UsageFetchContext, UsageProvider};
use super::types::{now_ms, UsageAmount, UsageLimit, UsageReport, UsageWindow};
use async_trait::async_trait;

const USAGE_URL: &str = "https://ollama.com/api/usage";

/// `(json key, window/limit id, label, duration in ms)`, one per meter.
const WINDOWS: &[(&str, &str, &str, u64)] = &[
    ("session", "5h", "Ollama 5 Hour", 5 * 60 * 60 * 1000),
    ("weekly", "7d", "Ollama 7 Day", 7 * 24 * 60 * 60 * 1000),
];

pub struct OllamaCloudUsage;

/// Render up to four top model consumers as `name xN`, joined by commas.
fn top_consumers(models: &serde_json::Value) -> Option<String> {
    let parts: Vec<String> = models
        .as_array()?
        .iter()
        .filter_map(|m| {
            let name = m.get("name")?.as_str()?;
            let count = m.get("request_count").and_then(|v| v.as_i64()).unwrap_or(0);
            Some(format!("{name} x{count}"))
        })
        .take(4)
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn build_limit(
    id: &str,
    label: &str,
    duration_ms: u64,
    used_fraction: f64,
    consumers: Option<String>,
) -> UsageLimit {
    let mut notes = Vec::new();
    if let Some(c) = consumers {
        notes.push(format!("Top consumers: {c}"));
    }
    UsageLimit {
        id: id.to_string(),
        label: label.to_string(),
        scope: None,
        window: Some(UsageWindow {
            id: id.to_string(),
            label: label.to_string(),
            duration_ms: Some(duration_ms),
            resets_at: None,
            reset_label: None,
        }),
        amount: UsageAmount::from_used_fraction(used_fraction.clamp(0.0, 1.0)),
        status: None,
        notes,
    }
    .with_derived_status()
}

fn parse_usage_body(provider: &str, body: &serde_json::Value) -> UsageReport {
    let mut limits = Vec::new();
    if let Some(root) = body.get("limits") {
        for (key, id, label, duration_ms) in WINDOWS {
            let Some(seg) = root.get(key) else {
                continue;
            };
            let Some(usage) = seg.get("usage").and_then(|v| v.as_f64()) else {
                continue;
            };
            if !usage.is_finite() {
                continue;
            }
            let consumers = seg.get("models").and_then(top_consumers);
            limits.push(build_limit(id, label, *duration_ms, usage, consumers));
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
impl UsageProvider for OllamaCloudUsage {
    fn id(&self) -> &str {
        "ollama-cloud"
    }

    async fn fetch_usage(&self, ctx: &UsageFetchContext) -> anyhow::Result<Option<UsageReport>> {
        let Some(token) = ctx.api_key.as_deref().or(ctx.access_token.as_deref()) else {
            return Ok(None);
        };
        let resp = ctx
            .client
            .get(USAGE_URL)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Ollama usage endpoint returned {}", resp.status());
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
    fn body_parses_session_and_weekly_fractions() {
        let body = serde_json::json!({
            "limits": {
                "session": { "usage": 0.03, "models": [{ "name": "glm-5.3-flash", "request_count": 54 }] },
                "weekly": { "usage": 0.95, "models": [{ "name": "glm-5.3-flash", "request_count": 458 }] }
            }
        });
        let report = parse_usage_body("ollama-cloud", &body);
        assert_eq!(report.limits.len(), 2);
        assert_eq!(report.limits[0].id, "5h");
        assert_eq!(report.limits[0].amount.used_fraction, Some(0.03));
        assert_eq!(
            report.limits[0].window.as_ref().unwrap().duration_ms,
            Some(5 * 60 * 60 * 1000)
        );
        assert!(report.limits[0]
            .notes
            .iter()
            .any(|n| n.contains("glm-5.3-flash x54")));
        // 0.95 used -> warning.
        assert_eq!(report.limits[1].status, Some(UsageStatus::Warning));
    }

    #[test]
    fn a_full_fraction_reads_exhausted_and_clamps() {
        let body = serde_json::json!({ "limits": { "session": { "usage": 1.4 } } });
        let report = parse_usage_body("ollama-cloud", &body);
        assert_eq!(report.limits.len(), 1);
        assert_eq!(report.limits[0].amount.used_fraction, Some(1.0));
        assert_eq!(report.limits[0].status, Some(UsageStatus::Exhausted));
    }

    #[test]
    fn a_body_without_limits_yields_an_empty_report() {
        let report = parse_usage_body("ollama-cloud", &serde_json::json!({}));
        assert!(report.limits.is_empty());
    }
}
