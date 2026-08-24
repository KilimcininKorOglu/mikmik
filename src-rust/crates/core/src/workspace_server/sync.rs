//! The user's own settings backup.
//!
//! What goes up is this machine's configuration minus everything that belongs
//! to somewhere else: the organisation's providers, the paths that name this
//! filesystem, and the relay token that authorises driving this machine.
//!
//! The user's own provider keys do go up, because a backup that restores
//! provider definitions without their keys restores nothing usable on the day
//! the machine is rebuilt. That was the decision the design was written
//! around, and it is why the server seals the blob at rest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth_store::{AuthStore, StoredCredential, WORKSPACE_ACCOUNT};
use crate::config::Settings;

use super::providers::managed_by;

/// The payload format. Bumped only when an older client could not read a
/// newer backup, so a restore can say so rather than half-applying.
pub const PAYLOAD_VERSION: u32 = 1;

/// What one machine uploads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload {
    pub version: u32,
    /// The settings file, with the excluded fields taken out.
    pub settings: Value,
    /// The user's own credentials, keyed by account name.
    ///
    /// Never the organisation's: the server already holds those, and handing
    /// them back would let a user keep a key after the entitlement is gone.
    #[serde(default)]
    pub credentials: BTreeMap<String, StoredCredential>,
}

/// Fields whose values name this machine rather than this user.
///
/// Restoring them onto a rebuilt or a second machine would point the session
/// at directories that are not there.
const MACHINE_PATHS: &[&str] = &["workspace_paths", "additional_dirs", "project_dir"];

/// Build the payload for this machine.
///
/// Everything excluded is excluded because it belongs to somewhere else:
///
/// - a provider the organisation manages, because the server already holds it
///   and a restored copy would outlive the entitlement;
/// - the paths above, because they name this filesystem;
/// - `remoteControl.token`, because it authorises driving this machine. The
///   url stays: knowing which relay to use is configuration, and the token is
///   what a second machine must be given deliberately;
/// - the workspace session itself, which is this machine's and expires.
pub fn build(settings: &Settings, auth: &AuthStore, server: &str) -> anyhow::Result<Payload> {
    let managed = managed_by(settings, server);

    let mut trimmed = settings.clone();
    for name in &managed {
        trimmed.providers.remove(name);
    }
    trimmed.config.workspace_paths.clear();
    trimmed.config.additional_dirs.clear();
    trimmed.config.project_dir = None;
    if let Some(remote) = trimmed.remote_control.as_mut() {
        remote.token.clear();
    }

    let credentials = auth
        .credentials
        .iter()
        .filter(|(name, _)| name.as_str() != WORKSPACE_ACCOUNT)
        .filter(|(name, _)| !managed.contains(name))
        .map(|(name, credential)| (name.clone(), credential.clone()))
        .collect();

    Ok(Payload {
        version: PAYLOAD_VERSION,
        settings: serde_json::to_value(&trimmed)?,
        credentials,
    })
}

/// What a restore would change, before anything is written.
#[derive(Debug, Clone, Default)]
pub struct Restore {
    /// The settings the backup carries, ready to merge.
    pub settings: Settings,
    /// Credentials for accounts this machine does not hold.
    pub credentials: BTreeMap<String, StoredCredential>,
    /// Fields the backup names that this machine will not take from it.
    pub refused: Vec<String>,
}

/// Read a payload into what it would restore.
///
/// A backup written by a newer client is refused whole rather than
/// half-applied: the fields it added are exactly the ones this client would
/// silently drop.
pub fn read(payload: &Value) -> anyhow::Result<Restore> {
    let payload: Payload = serde_json::from_value(payload.clone())?;
    if payload.version > PAYLOAD_VERSION {
        anyhow::bail!(
            "this backup was written by a newer version of mikmik (format {}, this reads {}); \
             upgrade before restoring it",
            payload.version,
            PAYLOAD_VERSION
        );
    }

    let settings: Settings = serde_json::from_value(payload.settings.clone())?;
    let refused = refused_keys(&payload.settings);
    Ok(Restore {
        settings,
        credentials: payload.credentials,
        refused,
    })
}

/// The keys a backup names that a restore drops anyway.
///
/// A backup made by an older client, or edited on the server, could still
/// carry one of the excluded fields. Naming them beats silently ignoring
/// them: the user is being asked to trust this restore.
fn refused_keys(settings: &Value) -> Vec<String> {
    let mut refused = Vec::new();
    if let Some(config) = settings.get("config").and_then(Value::as_object) {
        for key in MACHINE_PATHS {
            if config
                .get(*key)
                .is_some_and(|value| !value.is_null() && value != &Value::Array(Vec::new()))
            {
                refused.push(format!("config.{key}"));
            }
        }
    }
    if settings
        .pointer("/remoteControl/token")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty())
    {
        refused.push("remoteControl.token".to_string());
    }
    refused.sort();
    refused
}

/// Apply a restore that the user has approved.
///
/// The machine-specific fields the backup happened to carry are dropped here
/// as well as named in [`Restore::refused`], so a backup edited on the server
/// cannot reach them by a different route than the one that was checked.
///
/// A credential is never overwritten. A machine already holding an account is
/// using it; the backup is a way to get accounts back, not to replace them.
pub fn apply(restore: &Restore, settings: &mut Settings, auth: &mut AuthStore) {
    let managed: BTreeMap<String, crate::config::ProviderConfig> = settings
        .providers
        .iter()
        .filter(|(_, config)| config.managed_by.is_some())
        .map(|(name, config)| (name.clone(), config.clone()))
        .collect();

    let mut restored = restore.settings.clone();
    restored.config.workspace_paths.clear();
    restored.config.additional_dirs.clear();
    restored.config.project_dir = None;
    if let Some(remote) = restored.remote_control.as_mut() {
        remote.token.clear();
    }
    // The organisation's own settings stay whatever this machine was told, not
    // whatever another machine happened to have when it uploaded.
    restored.workspace = settings.workspace.clone();

    *settings = restored;
    settings.providers.extend(managed);

    for (name, credential) in &restore.credentials {
        if name == WORKSPACE_ACCOUNT || settings.providers.get(name).is_some_and(is_managed) {
            continue;
        }
        auth.credentials
            .entry(name.clone())
            .or_insert_with(|| credential.clone());
    }
}

fn is_managed(config: &crate::config::ProviderConfig) -> bool {
    config.managed_by.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProviderConfig, RemoteControlSettings, WorkspaceSettings};

    const SERVER: &str = "https://mikmik.firma.com";

    fn key(value: &str) -> StoredCredential {
        StoredCredential::ApiKey {
            key: value.to_string(),
        }
    }

    /// The key stored under `name`, if the account holds a plain one.
    ///
    /// `StoredCredential` carries an OAuth token shape too and is not
    /// comparable, so the tests read the one field they are about.
    fn plain<'a>(
        credentials: impl IntoIterator<Item = (&'a String, &'a StoredCredential)>,
        name: &str,
    ) -> Option<String> {
        match credentials
            .into_iter()
            .find(|(key, _)| key.as_str() == name)
            .map(|(_, credential)| credential)
        {
            Some(StoredCredential::ApiKey { key }) => Some(key.clone()),
            _ => None,
        }
    }

    fn machine() -> (Settings, AuthStore) {
        let mut settings = Settings {
            workspace: Some(WorkspaceSettings {
                url: SERVER.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        settings
            .providers
            .insert("mine".to_string(), ProviderConfig::default());
        settings.providers.insert(
            "firma-openai".to_string(),
            ProviderConfig {
                managed_by: Some(SERVER.to_string()),
                ..Default::default()
            },
        );
        settings.config.workspace_paths = vec!["/home/ayse/work".into()];
        settings.config.additional_dirs = vec!["/home/ayse/notes".into()];
        settings.config.project_dir = Some("/home/ayse/work/thing".into());
        settings.remote_control = Some(RemoteControlSettings {
            url: "https://relay.firma.com".to_string(),
            token: "a".repeat(32),
            label: Some("workstation".to_string()),
        });

        let mut auth = AuthStore::default();
        auth.credentials
            .insert("mine".to_string(), key("my-own-key"));
        auth.credentials
            .insert("firma-openai".to_string(), key("the-organisations-key"));
        auth.credentials.insert(
            WORKSPACE_ACCOUNT.to_string(),
            StoredCredential::WorkspaceSession {
                url: SERVER.to_string(),
                token: "the-session-token".to_string(),
                expires: 0,
            },
        );
        (settings, auth)
    }

    fn built() -> Payload {
        let (settings, auth) = machine();
        build(&settings, &auth, SERVER).expect("payload")
    }

    #[test]
    fn the_users_own_account_and_key_go_up() {
        // Without the key, a restored machine has a provider definition it
        // cannot use, which is the whole point of the backup.
        let payload = built();
        assert!(payload.settings["providers"]["mine"].is_object());
        assert_eq!(
            plain(&payload.credentials, "mine").as_deref(),
            Some("my-own-key")
        );
    }

    #[test]
    fn the_organisations_provider_does_not() {
        // The server already holds it, and a restored copy would outlive the
        // entitlement it came from.
        let payload = built();
        assert!(payload.settings["providers"].get("firma-openai").is_none());
        assert!(!payload.credentials.contains_key("firma-openai"));

        let json = serde_json::to_string(&payload).expect("serialise");
        assert!(
            !json.contains("the-organisations-key"),
            "the organisation's key went back up: {json}"
        );
    }

    #[test]
    fn the_paths_that_name_this_filesystem_do_not() {
        let payload = built();
        for key in MACHINE_PATHS {
            let value = &payload.settings["config"][key];
            assert!(
                value.is_null() || value.as_array().is_some_and(|items| items.is_empty()),
                "config.{key} went up as {value}"
            );
        }
    }

    #[test]
    fn the_relay_url_goes_up_and_its_token_does_not() {
        // Which relay to use is configuration. The token authorises driving
        // this machine, so a second machine has to be given it deliberately.
        let payload = built();
        assert_eq!(
            payload.settings["remoteControl"]["url"],
            "https://relay.firma.com"
        );
        assert_eq!(payload.settings["remoteControl"]["token"], "");

        let json = serde_json::to_string(&payload).expect("serialise");
        assert!(!json.contains(&"a".repeat(32)), "the relay token went up");
    }

    #[test]
    fn the_workspace_session_does_not_go_up() {
        let payload = built();
        assert!(!payload.credentials.contains_key(WORKSPACE_ACCOUNT));

        let json = serde_json::to_string(&payload).expect("serialise");
        assert!(!json.contains("the-session-token"), "{json}");
    }

    #[test]
    fn a_payload_round_trips() {
        let payload = built();
        let restore = read(&serde_json::to_value(&payload).expect("serialise")).expect("readable");
        assert!(restore.settings.providers.contains_key("mine"));
        assert_eq!(
            plain(&restore.credentials, "mine").as_deref(),
            Some("my-own-key")
        );
        assert!(restore.refused.is_empty(), "{:?}", restore.refused);
    }

    #[test]
    fn a_backup_from_a_newer_client_is_refused_whole() {
        // The fields it added are exactly the ones this client would drop.
        let mut payload = built();
        payload.version = PAYLOAD_VERSION + 1;
        let error = read(&serde_json::to_value(&payload).expect("serialise")).expect_err("refused");
        assert!(error.to_string().contains("upgrade"), "{error}");
    }

    #[test]
    fn a_backup_carrying_an_excluded_field_names_it() {
        // It could have been written by an older client or edited on the
        // server. Naming it beats dropping it in silence.
        let mut payload = built();
        payload.settings["config"]["workspace_paths"] = serde_json::json!(["/somewhere/else"]);
        payload.settings["remoteControl"]["token"] = serde_json::json!("borrowed-token");

        let restore = read(&serde_json::to_value(&payload).expect("serialise")).expect("readable");
        assert_eq!(
            restore.refused,
            vec!["config.workspace_paths", "remoteControl.token"]
        );
    }

    #[test]
    fn an_excluded_field_is_dropped_as_well_as_named() {
        // Otherwise a backup edited on the server reaches it by the route that
        // was not checked.
        let mut payload = built();
        payload.settings["config"]["workspace_paths"] = serde_json::json!(["/somewhere/else"]);
        payload.settings["remoteControl"]["token"] = serde_json::json!("borrowed-token");
        let restore = read(&serde_json::to_value(&payload).expect("serialise")).expect("readable");

        let (mut settings, mut auth) = machine();
        apply(&restore, &mut settings, &mut auth);

        assert!(settings.config.workspace_paths.is_empty());
        assert_eq!(
            settings
                .remote_control
                .as_ref()
                .map(|remote| remote.token.as_str()),
            Some("")
        );
    }

    #[test]
    fn a_restore_keeps_the_organisations_providers() {
        // They came from this machine's own login. A backup taken before the
        // entitlement existed must not remove them.
        let restore = read(&serde_json::to_value(built()).expect("serialise")).expect("readable");
        let (mut settings, mut auth) = machine();

        apply(&restore, &mut settings, &mut auth);

        assert!(settings.providers.contains_key("firma-openai"));
        assert!(settings.providers.contains_key("mine"));
    }

    #[test]
    fn a_restore_does_not_repoint_the_workspace_server() {
        // The address this machine logged in to is the one it holds a token
        // for; a backup from elsewhere must not move it.
        let mut payload = built();
        payload.settings["workspace"]["url"] = serde_json::json!("https://elsewhere.example");
        let restore = read(&serde_json::to_value(&payload).expect("serialise")).expect("readable");

        let (mut settings, mut auth) = machine();
        apply(&restore, &mut settings, &mut auth);

        assert_eq!(
            settings.workspace.map(|workspace| workspace.url),
            Some(SERVER.to_string())
        );
    }

    #[test]
    fn a_restore_never_replaces_a_credential_this_machine_holds() {
        // The machine is using it. A restore is a way to get accounts back,
        // not to swap the one in front of you.
        let restore = read(&serde_json::to_value(built()).expect("serialise")).expect("readable");
        let (mut settings, mut auth) = machine();
        auth.credentials
            .insert("mine".to_string(), key("the-one-in-use"));

        apply(&restore, &mut settings, &mut auth);

        assert_eq!(
            plain(&auth.credentials, "mine").as_deref(),
            Some("the-one-in-use")
        );
    }

    #[test]
    fn a_restore_onto_a_bare_machine_brings_the_account_back() {
        let restore = read(&serde_json::to_value(built()).expect("serialise")).expect("readable");
        let (mut settings, mut auth) = (Settings::default(), AuthStore::default());

        apply(&restore, &mut settings, &mut auth);

        assert!(settings.providers.contains_key("mine"));
        assert_eq!(
            plain(&auth.credentials, "mine").as_deref(),
            Some("my-own-key")
        );
    }

    #[test]
    fn a_restore_never_writes_the_workspace_session() {
        // The payload cannot carry one, but a backup edited on the server
        // could, and it would be a token for an account nobody proved.
        let mut payload = built();
        payload.credentials.insert(
            WORKSPACE_ACCOUNT.to_string(),
            StoredCredential::WorkspaceSession {
                url: "https://attacker.example".to_string(),
                token: "planted".to_string(),
                expires: u64::MAX,
            },
        );
        let restore = read(&serde_json::to_value(&payload).expect("serialise")).expect("readable");

        let (mut settings, mut auth) = (Settings::default(), AuthStore::default());
        apply(&restore, &mut settings, &mut auth);

        assert!(!auth.credentials.contains_key(WORKSPACE_ACCOUNT));
    }

    #[test]
    fn a_restore_never_gives_back_a_key_the_organisation_manages() {
        // A backup edited on the server could name one, and it would sit under
        // an account this machine treats as the organisation's.
        let mut payload = built();
        payload
            .credentials
            .insert("firma-openai".to_string(), key("planted-key"));
        let restore = read(&serde_json::to_value(&payload).expect("serialise")).expect("readable");

        let (mut settings, mut auth) = machine();
        auth.credentials.remove("firma-openai");
        apply(&restore, &mut settings, &mut auth);

        assert!(!auth.credentials.contains_key("firma-openai"));
    }
}
