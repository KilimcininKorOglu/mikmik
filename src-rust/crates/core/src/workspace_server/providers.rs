//! Writing the organisation's providers into this installation.
//!
//! A provider the server hands out becomes an ordinary account: an entry in
//! `settings.json` under `providers`, and its key in `auth.json`. What marks
//! it is `managed_by`, which names the server it came from.
//!
//! That mark is what makes the rest possible. A managed account stays out of
//! the user's settings backup, `workspace logout` removes exactly those and
//! nothing else, and `/workspace` can list them apart from the accounts the
//! user added themselves.
//!
//! Nothing here touches the network. The caller fetches, and calls this only
//! when it has an answer; a server that could not be reached leaves every
//! account exactly as it was.

use crate::auth_store::{AuthStore, StoredCredential};
use crate::config::{ProviderConfig, Settings};

use super::client::EntitledProvider;

/// What applying the organisation's providers changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Accounts written or refreshed.
    pub written: Vec<String>,
    /// Accounts dropped because the entitlement is gone.
    pub withdrawn: Vec<String>,
    /// Accounts the server offered and this installation kept its own version
    /// of, because the user had already configured an account by that name.
    pub refused: Vec<String>,
}

impl Applied {
    pub fn is_empty(&self) -> bool {
        self.written.is_empty() && self.withdrawn.is_empty() && self.refused.is_empty()
    }
}

/// Every account this server manages, in a stable order.
pub fn managed_by(settings: &Settings, server: &str) -> Vec<String> {
    let server = normalise(server);
    let mut names: Vec<String> = settings
        .providers
        .iter()
        .filter(|(_, config)| {
            config
                .managed_by
                .as_deref()
                .map(normalise)
                .is_some_and(|owner| owner == server)
        })
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names
}

/// Write what the server handed out, and drop what it no longer hands out.
///
/// An account the user configured themselves is never overwritten, even when
/// the server offers one by the same name. The organisation may hand out a
/// credential; it may not silently replace the one the user is already using
/// under a name they chose.
pub fn apply(
    settings: &mut Settings,
    auth: &mut AuthStore,
    server: &str,
    entitled: &[EntitledProvider],
) -> Applied {
    let server = normalise(server).to_string();
    let mut result = Applied::default();

    for provider in entitled {
        let name = provider.name.trim();
        if name.is_empty() {
            continue;
        }
        match settings.providers.get(name) {
            Some(existing) if existing.managed_by.is_none() => {
                result.refused.push(name.to_string());
                continue;
            }
            _ => {}
        }

        settings.providers.insert(
            name.to_string(),
            ProviderConfig {
                api_base: provider.api_base.clone(),
                enabled: true,
                protocol: provider.protocol.clone(),
                models: provider.models.clone(),
                managed_by: Some(server.clone()),
                ..Default::default()
            },
        );
        auth.credentials.insert(
            name.to_string(),
            StoredCredential::ApiKey {
                key: provider.api_key.clone(),
            },
        );
        result.written.push(name.to_string());
    }

    // Anything this server used to manage and no longer offers goes, key and
    // all. An entitlement that was taken away has to stop working here too.
    for name in managed_by(settings, &server) {
        if entitled.iter().any(|provider| provider.name.trim() == name) {
            continue;
        }
        settings.providers.remove(&name);
        auth.credentials.remove(&name);
        result.withdrawn.push(name);
    }

    result.written.sort();
    result.refused.sort();
    result
}

/// Drop every account this server manages, with its key.
///
/// What `workspace logout` calls. The user's own accounts are untouched,
/// because they carry no `managed_by`.
pub fn forget(settings: &mut Settings, auth: &mut AuthStore, server: &str) -> Vec<String> {
    let names = managed_by(settings, server);
    for name in &names {
        settings.providers.remove(name);
        auth.credentials.remove(name);
    }
    names
}

/// The form two addresses are compared in.
fn normalise(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "https://mikmik.firma.com";

    fn offered(name: &str) -> EntitledProvider {
        EntitledProvider {
            name: name.to_string(),
            protocol: Some("openai".to_string()),
            api_base: Some("https://api.firma.com/v1".to_string()),
            api_key: format!("key-for-{name}"),
            models: vec!["gpt-x".to_string()],
        }
    }

    fn fixture() -> (Settings, AuthStore) {
        (Settings::default(), AuthStore::default())
    }

    fn stored_key(auth: &AuthStore, name: &str) -> Option<String> {
        match auth.credentials.get(name) {
            Some(StoredCredential::ApiKey { key }) => Some(key.clone()),
            _ => None,
        }
    }

    #[test]
    fn an_offered_provider_becomes_an_account_with_its_key() {
        let (mut settings, mut auth) = fixture();
        let result = apply(&mut settings, &mut auth, SERVER, &[offered("firma-openai")]);

        assert_eq!(result.written, vec!["firma-openai"]);
        let account = settings.providers.get("firma-openai").expect("an account");
        assert_eq!(account.protocol.as_deref(), Some("openai"));
        assert_eq!(
            account.api_base.as_deref(),
            Some("https://api.firma.com/v1")
        );
        assert!(account.enabled);
        assert_eq!(
            stored_key(&auth, "firma-openai").as_deref(),
            Some("key-for-firma-openai")
        );
    }

    #[test]
    fn a_managed_account_says_which_server_it_came_from() {
        // Three things read this: the backup filter, `workspace logout`, and
        // the listing that keeps a user from editing an entry the next pull
        // overwrites.
        let (mut settings, mut auth) = fixture();
        apply(&mut settings, &mut auth, SERVER, &[offered("firma-openai")]);

        assert_eq!(
            settings.providers["firma-openai"].managed_by.as_deref(),
            Some(SERVER)
        );
    }

    #[test]
    fn an_account_the_user_added_carries_no_mark() {
        let (mut settings, _) = fixture();
        settings
            .providers
            .insert("mine".to_string(), ProviderConfig::default());
        assert!(settings.providers["mine"].managed_by.is_none());
        assert!(managed_by(&settings, SERVER).is_empty());
    }

    #[test]
    fn the_key_never_lands_in_the_settings_file() {
        // `settings.json` is an ordinary file. Only `auth.json` is 0o600.
        let (mut settings, mut auth) = fixture();
        apply(&mut settings, &mut auth, SERVER, &[offered("firma-openai")]);

        let json = serde_json::to_string(&settings).expect("serialise");
        assert!(
            !json.contains("key-for-firma-openai"),
            "the key reached settings.json: {json}"
        );
    }

    #[test]
    fn an_account_the_user_configured_is_never_overwritten() {
        // The organisation may hand out a credential. It may not replace the
        // one the user is already using under a name they chose.
        let (mut settings, mut auth) = fixture();
        settings.providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_base: Some("https://mine.example".to_string()),
                ..Default::default()
            },
        );

        let result = apply(&mut settings, &mut auth, SERVER, &[offered("openai")]);

        assert_eq!(result.refused, vec!["openai"]);
        assert!(result.written.is_empty());
        assert_eq!(
            settings.providers["openai"].api_base.as_deref(),
            Some("https://mine.example")
        );
        assert!(stored_key(&auth, "openai").is_none());
    }

    #[test]
    fn a_second_pull_refreshes_rather_than_duplicates() {
        let (mut settings, mut auth) = fixture();
        apply(&mut settings, &mut auth, SERVER, &[offered("firma-openai")]);

        let mut rotated = offered("firma-openai");
        rotated.api_key = "the-rotated-key".to_string();
        let result = apply(&mut settings, &mut auth, SERVER, &[rotated]);

        assert_eq!(result.written, vec!["firma-openai"]);
        assert!(result.withdrawn.is_empty());
        assert_eq!(settings.providers.len(), 1);
        assert_eq!(
            stored_key(&auth, "firma-openai").as_deref(),
            Some("the-rotated-key")
        );
    }

    #[test]
    fn an_entitlement_that_was_taken_away_stops_working_here() {
        let (mut settings, mut auth) = fixture();
        apply(
            &mut settings,
            &mut auth,
            SERVER,
            &[offered("kept"), offered("withdrawn")],
        );

        let result = apply(&mut settings, &mut auth, SERVER, &[offered("kept")]);

        assert_eq!(result.withdrawn, vec!["withdrawn"]);
        assert!(!settings.providers.contains_key("withdrawn"));
        assert!(
            stored_key(&auth, "withdrawn").is_none(),
            "the key outlived the entitlement"
        );
        assert!(settings.providers.contains_key("kept"));
    }

    #[test]
    fn withdrawing_never_touches_the_users_own_accounts() {
        let (mut settings, mut auth) = fixture();
        settings
            .providers
            .insert("mine".to_string(), ProviderConfig::default());
        auth.credentials.insert(
            "mine".to_string(),
            StoredCredential::ApiKey {
                key: "my-own-key".to_string(),
            },
        );

        apply(&mut settings, &mut auth, SERVER, &[offered("firma-openai")]);
        apply(&mut settings, &mut auth, SERVER, &[]);

        assert!(settings.providers.contains_key("mine"));
        assert_eq!(stored_key(&auth, "mine").as_deref(), Some("my-own-key"));
    }

    #[test]
    fn another_servers_accounts_are_left_alone() {
        // Two addresses is not the supported case, but withdrawing on behalf
        // of a server that never offered an account would be wrong whatever
        // put it there.
        let (mut settings, mut auth) = fixture();
        settings.providers.insert(
            "elsewhere".to_string(),
            ProviderConfig {
                managed_by: Some("https://other.firma.com".to_string()),
                ..Default::default()
            },
        );

        apply(&mut settings, &mut auth, SERVER, &[]);
        assert!(settings.providers.contains_key("elsewhere"));
    }

    #[test]
    fn a_trailing_slash_is_not_a_different_server() {
        let (mut settings, mut auth) = fixture();
        apply(
            &mut settings,
            &mut auth,
            "https://mikmik.firma.com/",
            &[offered("firma-openai")],
        );

        assert_eq!(managed_by(&settings, SERVER), vec!["firma-openai"]);
    }

    #[test]
    fn logging_out_removes_the_managed_accounts_and_their_keys() {
        let (mut settings, mut auth) = fixture();
        settings
            .providers
            .insert("mine".to_string(), ProviderConfig::default());
        apply(
            &mut settings,
            &mut auth,
            SERVER,
            &[offered("firma-openai"), offered("firma-anthropic")],
        );

        let gone = forget(&mut settings, &mut auth, SERVER);

        assert_eq!(gone, vec!["firma-anthropic", "firma-openai"]);
        assert!(stored_key(&auth, "firma-openai").is_none());
        assert!(
            settings.providers.contains_key("mine"),
            "logging out took the user's own account with it"
        );
    }

    #[test]
    fn an_offer_with_no_name_is_ignored() {
        // An empty key in the providers map is unreachable and would sit there
        // looking like a configured account.
        let (mut settings, mut auth) = fixture();
        let result = apply(&mut settings, &mut auth, SERVER, &[offered("  ")]);

        assert!(result.is_empty());
        assert!(settings.providers.is_empty());
    }
}
