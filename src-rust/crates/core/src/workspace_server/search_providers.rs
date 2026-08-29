//! Writing the organisation's web-search keys into this installation.
//!
//! A web-search provider is not a model account. It has no `settings.json`
//! entry the way an LLM provider does; the search tool reads its key straight
//! from `auth.json` under the provider id (`tavily`, `brave`, ...). So an
//! entitlement here writes one thing: an `auth.json` credential, marked with
//! the server it came from.
//!
//! That mark is what the rest depends on. A managed search key is dropped by
//! `workspace logout`, withdrawn when the entitlement goes, and told apart
//! from a key the user entered by hand, which is never overwritten.
//!
//! Nothing here touches the network. The caller fetches and calls this only
//! when it has an answer.

use crate::auth_store::{AuthStore, StoredCredential};

use super::client::EntitledProvider;
use super::providers::{normalise, Applied};

/// Whether a stored credential is one the user owns rather than this server.
///
/// A key the user typed carries no `managed_by`. Any other credential shape,
/// an OAuth token filed under the same id, is the user's too and must never be
/// replaced by a search entitlement.
fn is_user_owned(credential: &StoredCredential, server: &str) -> bool {
    // This server's own key is refreshable, not the user's. Everything else, a
    // key the user typed, an OAuth token, or a key another server manages, is
    // left alone.
    match credential {
        StoredCredential::ApiKey {
            managed_by: Some(owner),
            ..
        } => normalise(owner) != server,
        _ => true,
    }
}

/// Every search key this server manages, in a stable order.
pub fn managed(auth: &AuthStore, server: &str) -> Vec<String> {
    let server = normalise(server);
    let mut names: Vec<String> = auth
        .credentials
        .iter()
        .filter_map(|(name, credential)| match credential {
            StoredCredential::ApiKey {
                managed_by: Some(owner),
                ..
            } if normalise(owner) == server => Some(name.clone()),
            _ => None,
        })
        .collect();
    names.sort();
    names
}

/// Write the search keys the server handed out, and drop what it no longer
/// hands out.
///
/// `entitled` is the web-search subset of the entitlement list. A key the user
/// entered under the same id is never overwritten: the organisation may hand
/// out a search key, but it may not replace the one the user is already using.
pub fn apply(auth: &mut AuthStore, server: &str, entitled: &[&EntitledProvider]) -> Applied {
    let server = normalise(server).to_string();
    let mut result = Applied::default();

    for provider in entitled {
        let name = provider.name.trim();
        if name.is_empty() {
            continue;
        }
        if auth
            .credentials
            .get(name)
            .is_some_and(|credential| is_user_owned(credential, &server))
        {
            result.refused.push(name.to_string());
            continue;
        }

        auth.credentials.insert(
            name.to_string(),
            StoredCredential::ApiKey {
                key: provider.api_key.clone(),
                managed_by: Some(server.clone()),
            },
        );
        result.written.push(name.to_string());
    }

    // A search key this server used to manage and no longer offers goes. An
    // entitlement that was taken away has to stop working here too.
    for name in managed(auth, &server) {
        if entitled.iter().any(|provider| provider.name.trim() == name) {
            continue;
        }
        auth.credentials.remove(&name);
        result.withdrawn.push(name);
    }

    result.written.sort();
    result.refused.sort();
    result
}

/// Drop every search key this server manages.
///
/// What `workspace logout` calls. A key the user entered is untouched, because
/// it carries no `managed_by`.
pub fn forget(auth: &mut AuthStore, server: &str) -> Vec<String> {
    let names = managed(auth, server);
    for name in &names {
        auth.credentials.remove(name);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "https://mikmik.firma.com";

    fn offered(name: &str) -> EntitledProvider {
        EntitledProvider {
            name: name.to_string(),
            protocol: None,
            api_base: None,
            api_key: format!("key-for-{name}"),
            models: Vec::new(),
            kind: Some("web_search".to_string()),
        }
    }

    fn stored(auth: &AuthStore, name: &str) -> Option<(String, Option<String>)> {
        match auth.credentials.get(name) {
            Some(StoredCredential::ApiKey { key, managed_by }) => {
                Some((key.clone(), managed_by.clone()))
            }
            _ => None,
        }
    }

    #[test]
    fn an_offered_search_key_lands_in_auth_with_the_server_mark() {
        let mut auth = AuthStore::default();
        let result = apply(&mut auth, SERVER, &[&offered("tavily")]);

        assert_eq!(result.written, vec!["tavily"]);
        assert_eq!(
            stored(&auth, "tavily"),
            Some(("key-for-tavily".to_string(), Some(SERVER.to_string())))
        );
    }

    #[test]
    fn the_search_key_never_reaches_the_settings_file() {
        // A search entitlement writes only auth.json; it has no settings entry
        // and this test guards that it stays that way.
        let mut auth = AuthStore::default();
        apply(&mut auth, SERVER, &[&offered("tavily")]);
        assert!(auth.credentials.contains_key("tavily"));
    }

    #[test]
    fn a_key_the_user_entered_is_refused_not_overwritten() {
        let mut auth = AuthStore::default();
        auth.credentials
            .insert("tavily".to_string(), StoredCredential::api_key("my-own"));

        let result = apply(&mut auth, SERVER, &[&offered("tavily")]);

        assert_eq!(result.refused, vec!["tavily"]);
        assert!(result.written.is_empty());
        assert_eq!(
            stored(&auth, "tavily"),
            Some(("my-own".to_string(), None)),
            "the user's own key was replaced"
        );
    }

    #[test]
    fn a_withdrawn_entitlement_takes_its_key_away() {
        let mut auth = AuthStore::default();
        apply(&mut auth, SERVER, &[&offered("tavily")]);

        let result = apply(&mut auth, SERVER, &[]);

        assert_eq!(result.withdrawn, vec!["tavily"]);
        assert!(!auth.credentials.contains_key("tavily"));
    }

    #[test]
    fn logout_removes_only_the_managed_keys() {
        let mut auth = AuthStore::default();
        apply(&mut auth, SERVER, &[&offered("tavily")]);
        auth.credentials
            .insert("brave".to_string(), StoredCredential::api_key("my-brave"));

        let gone = forget(&mut auth, SERVER);

        assert_eq!(gone, vec!["tavily"]);
        assert!(!auth.credentials.contains_key("tavily"));
        assert_eq!(
            stored(&auth, "brave"),
            Some(("my-brave".to_string(), None)),
            "the user's own key was removed by logout"
        );
    }

    #[test]
    fn managed_lists_only_this_servers_keys() {
        let mut auth = AuthStore::default();
        apply(&mut auth, SERVER, &[&offered("tavily"), &offered("exa")]);
        auth.credentials
            .insert("brave".to_string(), StoredCredential::api_key("my-brave"));

        assert_eq!(managed(&auth, SERVER), vec!["exa", "tavily"]);
    }
}
