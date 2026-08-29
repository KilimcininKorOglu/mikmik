//! Account usage/limit types, ported from oh-my-pi `packages/ai/src/usage`.
//!
//! A [`UsageReport`] is one provider's snapshot of the signed-in account's
//! quota: one [`UsageLimit`] per meter/window (a 5-hour session window, a 7-day
//! account window, a Spark meter, and so on). Providers fill it either from the
//! rate-limit headers on a normal response or from a dedicated usage endpoint.

use serde::{Deserialize, Serialize};

/// How close a meter is to its cap, derived from the used fraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageStatus {
    /// No usage fraction was reported.
    Unknown,
    /// Below the warning threshold.
    Ok,
    /// At or above 90% of the cap.
    Warning,
    /// At or above the cap.
    Exhausted,
}

impl UsageStatus {
    /// Maps a used fraction (0.0..=1.0+) to a status, mirroring omp's
    /// `usageStatus`: `>=1.0` exhausted, `>=0.9` warning, otherwise ok, and
    /// unknown when no fraction was measured.
    pub fn from_fraction(used_fraction: Option<f64>) -> Self {
        match used_fraction {
            None => Self::Unknown,
            Some(f) if f >= 1.0 => Self::Exhausted,
            Some(f) if f >= 0.9 => Self::Warning,
            Some(_) => Self::Ok,
        }
    }
}

/// The unit a meter is counted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UsageUnit {
    /// A fraction/percentage of an opaque cap (the common case for rate limits).
    #[default]
    Percent,
    /// A request count.
    Requests,
    /// A token count.
    Tokens,
    /// A spend/credit amount.
    Credits,
    /// The unit was not reported.
    Unknown,
}

/// The time window a meter resets on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub id: String,
    pub label: String,
    /// Window length in milliseconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Unix timestamp (ms) when the window resets, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// A human-readable reset hint, when the provider gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_label: Option<String>,
}

/// The measured amount for one meter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UsageAmount {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_fraction: Option<f64>,
    pub unit: UsageUnit,
}

impl UsageAmount {
    /// A percent meter built from a used fraction (0.0..=1.0), the shape most
    /// rate-limit headers report.
    pub fn from_used_fraction(used_fraction: f64) -> Self {
        Self {
            used_fraction: Some(used_fraction),
            remaining_fraction: Some((1.0 - used_fraction).max(0.0)),
            unit: UsageUnit::Percent,
            ..Self::default()
        }
    }
}

/// One meter within a provider's usage report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageLimit {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<UsageWindow>,
    pub amount: UsageAmount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<UsageStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl UsageLimit {
    /// Sets `status` from the amount's used fraction when it is not already set.
    pub fn with_derived_status(mut self) -> Self {
        if self.status.is_none() {
            self.status = Some(UsageStatus::from_fraction(self.amount.used_fraction));
        }
        self
    }
}

/// One provider's snapshot of the signed-in account's quota.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub provider: String,
    /// Unix timestamp (ms) when this snapshot was taken.
    pub fetched_at: u64,
    pub limits: Vec<UsageLimit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl UsageReport {
    /// A report with the given limits, stamped now.
    pub fn new(provider: impl Into<String>, limits: Vec<UsageLimit>) -> Self {
        Self {
            provider: provider.into(),
            fetched_at: now_ms(),
            limits,
            notes: Vec::new(),
        }
    }
}

/// Current Unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parses a positive epoch timestamp into milliseconds, from a JSON number or a
/// numeric string. A value below 1e12 is read as seconds and scaled to ms, so a
/// seconds- or ms-based reset field both land on ms. Non-positive input is
/// `None`. Mirrors omp's `parsePositiveTimestamp`.
pub fn parse_positive_timestamp(value: &serde_json::Value) -> Option<u64> {
    let n = match value {
        serde_json::Value::Number(num) => num.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if !(n.is_finite() && n > 0.0) {
        return None;
    }
    let ms = if n < 1e12 { n * 1000.0 } else { n };
    Some(ms as u64)
}

/// Parses an ISO-8601 timestamp string into epoch milliseconds. Mirrors omp's
/// `parseIsoTimestamp`.
pub fn parse_iso_timestamp(value: &str) -> Option<u64> {
    let ms = chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()?
        .timestamp_millis();
    if ms > 0 {
        Some(ms as u64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_thresholds_bracket_warning_and_exhausted() {
        assert_eq!(UsageStatus::from_fraction(None), UsageStatus::Unknown);
        assert_eq!(UsageStatus::from_fraction(Some(0.5)), UsageStatus::Ok);
        assert_eq!(UsageStatus::from_fraction(Some(0.89)), UsageStatus::Ok);
        assert_eq!(UsageStatus::from_fraction(Some(0.9)), UsageStatus::Warning);
        assert_eq!(
            UsageStatus::from_fraction(Some(0.999)),
            UsageStatus::Warning
        );
        assert_eq!(
            UsageStatus::from_fraction(Some(1.0)),
            UsageStatus::Exhausted
        );
        assert_eq!(
            UsageStatus::from_fraction(Some(1.4)),
            UsageStatus::Exhausted
        );
    }

    #[test]
    fn seconds_timestamp_scales_to_milliseconds() {
        // A seconds value (< 1e12) is scaled; an ms value is kept as-is.
        let secs = serde_json::json!(1_700_000_000i64);
        let ms = serde_json::json!(1_700_000_000_000i64);
        assert_eq!(parse_positive_timestamp(&secs), Some(1_700_000_000_000));
        assert_eq!(parse_positive_timestamp(&ms), Some(1_700_000_000_000));
    }

    #[test]
    fn timestamp_accepts_a_numeric_string_and_rejects_non_positive() {
        assert_eq!(
            parse_positive_timestamp(&serde_json::json!("1700000000")),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_positive_timestamp(&serde_json::json!(0)), None);
        assert_eq!(parse_positive_timestamp(&serde_json::json!(-5)), None);
        assert_eq!(parse_positive_timestamp(&serde_json::json!("nope")), None);
    }

    #[test]
    fn iso_timestamp_parses_to_epoch_ms() {
        let ms = parse_iso_timestamp("2023-11-14T22:13:20Z").expect("valid rfc3339");
        assert_eq!(ms, 1_700_000_000_000);
        assert_eq!(parse_iso_timestamp("not-a-date"), None);
    }

    #[test]
    fn derived_status_fills_only_when_absent() {
        let limit = UsageLimit {
            id: "5h".into(),
            label: "5 hour".into(),
            scope: None,
            window: None,
            amount: UsageAmount::from_used_fraction(0.95),
            status: None,
            notes: Vec::new(),
        }
        .with_derived_status();
        assert_eq!(limit.status, Some(UsageStatus::Warning));
    }
}
