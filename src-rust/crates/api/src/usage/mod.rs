//! Account usage/limit reporting, ported from oh-my-pi `packages/ai/src/usage`.
//!
//! Each provider implements [`UsageProvider`] to report the signed-in account's
//! quota, from response headers ([`UsageProvider::parse_rate_limit_headers`])
//! or a usage endpoint ([`UsageProvider::fetch_usage`]). [`usage_provider_for`]
//! resolves the reporter for a provider id.

mod anthropic;
mod codex;
mod provider;
mod registry;
mod types;

pub use provider::{UsageFetchContext, UsageProvider};
pub use registry::usage_provider_for;
pub use types::{
    now_ms, parse_iso_timestamp, parse_positive_timestamp, UsageAmount, UsageLimit, UsageReport,
    UsageStatus, UsageUnit, UsageWindow,
};
