//! Maps a provider id to its [`UsageProvider`] implementation.

use super::anthropic::AnthropicUsage;
use super::codex::CodexUsage;
use super::provider::UsageProvider;
use super::zai::ZaiUsage;
use std::sync::Arc;

/// Returns the usage reporter for a provider id, or `None` when that provider
/// has no usage support yet. New providers are added one arm at a time.
pub fn usage_provider_for(provider_id: &str) -> Option<Arc<dyn UsageProvider>> {
    match provider_id {
        "anthropic" => Some(Arc::new(AnthropicUsage)),
        "codex" => Some(Arc::new(CodexUsage)),
        "zai" => Some(Arc::new(ZaiUsage)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_provider_resolves_and_unknown_is_none() {
        assert!(usage_provider_for("anthropic").is_some());
        assert!(usage_provider_for("no-such-provider").is_none());
    }
}
