// Concrete search providers, one module per backend.
//
// Each implements the `SearchProvider` trait from the parent `provider`
// module, mapping its native wire response onto a `SearchResponse`.

pub mod anthropic;
pub mod brave;
pub mod duckduckgo;
pub mod exa;
pub mod firecrawl;
pub mod gemini;
pub mod jina;
pub mod kagi;
pub mod kimi;
pub mod parallel;
pub mod searxng;
pub mod synthetic;
pub mod tavily;
pub mod tinyfish;
pub mod xai;
pub mod zai;

use crate::web::search::types::SearchProviderId;

/// The API key for a search backend: the stored credential first, then the
/// backend's environment variable.
///
/// Rule: any key the app needs must be enterable from the TUI, so the stored
/// value (written by the key-entry flow into `auth.json` under the backend's
/// id) always wins; the env var stays as a headless fallback. Mirrors omp's
/// `authStorage.getApiKey(id) ?? env`.
pub(crate) fn stored_or_env_key(id: SearchProviderId, env_var: &str) -> Option<String> {
    let stored = mikmik_core::AuthStore::load().api_key_for(id.as_str());
    let env = std::env::var(env_var).ok();
    pick_key(stored, env)
}

/// Choose the key to use: a non-empty stored value first, else a non-empty
/// environment value. Splitting the decision from the I/O keeps the precedence
/// rule testable without touching the auth store or process environment.
fn pick_key(stored: Option<String>, env: Option<String>) -> Option<String> {
    stored
        .filter(|k| !k.is_empty())
        .or_else(|| env.filter(|k| !k.is_empty()))
}

/// Age in seconds from an ISO date string, or `None` when it does not parse.
///
/// Mirrors omp's `dateToAgeSeconds`: `(now - date) / 1000`. Accepts an RFC 3339
/// timestamp or a bare `YYYY-MM-DD` date (taken at UTC midnight).
pub(crate) fn date_to_age_seconds(date: &str) -> Option<f64> {
    let millis = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(date) {
        dt.timestamp_millis()
    } else {
        let day = chrono::NaiveDate::parse_from_str(date.get(..10)?, "%Y-%m-%d").ok()?;
        day.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis()
    };
    let now = chrono::Utc::now().timestamp_millis();
    Some(((now - millis) / 1000) as f64)
}

/// Minimal percent-encoding for a URL query-parameter value.
pub(crate) fn urlencode(s: &str) -> String {
    let mut encoded = String::new();
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => encoded.push(ch),
            ' ' => encoded.push('+'),
            _ => {
                for byte in ch.to_string().as_bytes() {
                    encoded.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{pick_key, urlencode};

    #[test]
    fn urlencode_maps_spaces_and_escapes_reserved_bytes() {
        assert_eq!(urlencode("rust ownership"), "rust+ownership");
        assert_eq!(urlencode("a/b?c"), "a%2Fb%3Fc");
        assert_eq!(urlencode("crate.io~1_x-y"), "crate.io~1_x-y");
    }

    #[test]
    fn stored_key_wins_over_env_and_empty_is_skipped() {
        // A stored key takes precedence over the env var.
        assert_eq!(
            pick_key(Some("stored".into()), Some("env".into())).as_deref(),
            Some("stored")
        );
        // An empty stored value falls through to the env var.
        assert_eq!(
            pick_key(Some(String::new()), Some("env".into())).as_deref(),
            Some("env")
        );
        // Env fallback when nothing is stored.
        assert_eq!(pick_key(None, Some("env".into())).as_deref(), Some("env"));
        // Neither present, or both empty, yields nothing.
        assert_eq!(pick_key(None, None), None);
        assert_eq!(pick_key(Some(String::new()), Some(String::new())), None);
    }
}
