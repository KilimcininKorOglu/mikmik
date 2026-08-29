//! Account usage/limit reporting, ported from oh-my-pi `packages/ai/src/usage`.
//!
//! Each provider implements [`UsageProvider`] to report the signed-in account's
//! quota, from response headers ([`UsageProvider::parse_rate_limit_headers`])
//! or a usage endpoint ([`UsageProvider::fetch_usage`]). [`usage_provider_for`]
//! resolves the reporter for a provider id.

mod anthropic;
mod codex;
mod cursor;
mod github_copilot;
mod google_antigravity;
mod kimi;
mod live;
mod minimax_code;
mod ollama_cloud;
mod opencode_go;
mod provider;
mod registry;
mod synthetic;
mod types;
mod umans;
mod xai_oauth;
mod zai;

pub use live::{record_headers, take_live_report};
pub use provider::{UsageFetchContext, UsageProvider};
pub use registry::usage_provider_for;
pub use types::{
    now_ms, parse_iso_timestamp, parse_positive_timestamp, UsageAmount, UsageLimit, UsageReport,
    UsageStatus, UsageUnit, UsageWindow,
};
