//! A process-wide slot for the most recent usage report parsed from a normal
//! response's rate-limit headers.
//!
//! The client writes it whenever a response carries parseable rate-limit
//! headers ([`record_headers`]); the TUI drains it ([`take_live_report`]) into
//! the sidebar. This is the free, universal feed: it needs no extra request and
//! rides on whatever the active account already returns. Last write wins, which
//! is correct for a one-session TUI showing the active account.

use super::registry::usage_provider_for;
use super::types::{now_ms, UsageReport};
use reqwest::header::HeaderMap;
use std::sync::{Mutex, OnceLock};

fn slot() -> &'static Mutex<Option<UsageReport>> {
    static SLOT: OnceLock<Mutex<Option<UsageReport>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Parses `headers` with `provider_id`'s reporter and stores the result when it
/// carries usable rate-limit fields. A provider without a reporter or headers is
/// a no-op.
pub fn record_headers(provider_id: &str, headers: &HeaderMap) {
    let Some(reporter) = usage_provider_for(provider_id) else {
        return;
    };
    let Some(report) = reporter.parse_rate_limit_headers(headers, now_ms()) else {
        return;
    };
    if let Ok(mut guard) = slot().lock() {
        *guard = Some(report);
    }
}

/// Takes the latest header-parsed report, leaving the slot empty until the next
/// response writes one.
pub fn take_live_report() -> Option<UsageReport> {
    slot().lock().ok().and_then(|mut guard| guard.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_take_round_trips_and_empties() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.5".parse().unwrap(),
        );
        record_headers("anthropic", &headers);
        let report = take_live_report().expect("a report was recorded");
        assert_eq!(report.provider, "anthropic");
        assert!(
            take_live_report().is_none(),
            "the slot empties after a take"
        );
    }

    #[test]
    fn a_provider_without_a_reporter_is_a_no_op() {
        take_live_report();
        record_headers("no-such-provider", &HeaderMap::new());
        assert!(take_live_report().is_none());
    }
}
