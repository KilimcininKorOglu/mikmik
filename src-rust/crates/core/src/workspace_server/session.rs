//! What a running session does with the workspace server.
//!
//! One place for the three networked steps, so the `workspace` subcommand and
//! the background triggers cannot drift into doing them differently: take the
//! providers, take the policy, and put the settings backup.
//!
//! Every one of them fails soft. A machine whose server is unreachable keeps
//! the accounts and the policy it already has and goes on working; nothing
//! here is allowed to stop a session from opening.

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::auth_store::AuthStore;
use crate::config::{Settings, WorkspaceSettings};

use super::client::{BackupWrite, WorkspaceClient};
use super::{policy, providers, sync};

/// How often the settings file is checked for a change.
///
/// Polling rather than a filesystem watcher: this needs no new dependency, and
/// a few seconds' delay before an upload is not a delay anyone notices.
const POLL_SECS: u64 = 5;

/// How long the writes have to stop before an upload.
///
/// An editor saving a file, or the app writing a settings change of its own,
/// often produces several writes in a row. Without this every one of them
/// would be an upload.
const DEBOUNCE_SECS: u64 = 15;

// The wait has to outlast a burst of writes, and it can only do that if it is
// longer than the gap between two looks at the file.
const _: () = assert!(DEBOUNCE_SECS > POLL_SECS);

/// The shortest timer an installation may ask for.
///
/// A one-minute timer would be an upload every minute for a file that changes
/// a few times a day.
pub const MIN_INTERVAL_MINUTES: u64 = 5;

/// The server this installation is signed in to, with its live session.
///
/// Answers `None` when no server is configured or no session is held, which
/// is the ordinary state for most installations rather than an error.
pub fn connect(settings: &Settings) -> Option<(WorkspaceSettings, WorkspaceClient)> {
    let workspace = settings.workspace.clone()?;
    let token = AuthStore::load()
        .workspace_session(workspace.base())
        .map(str::to_string)?;
    match WorkspaceClient::new(&workspace) {
        Ok(client) => Some((workspace, client.with_token(token))),
        Err(error) => {
            warn!(%error, "the workspace address is not usable");
            None
        }
    }
}

/// Take the providers this account is entitled to and write them in.
pub async fn pull_providers(client: &WorkspaceClient) -> anyhow::Result<providers::Applied> {
    let entitled = client.providers().await?;

    let mut settings = Settings::load_sync()?;
    let mut auth = AuthStore::load();
    let applied = providers::apply(&mut settings, &mut auth, client.base(), &entitled);
    if !applied.is_empty() {
        settings.save_sync()?;
        auth.save();
    }
    Ok(applied)
}

/// Upload this machine's settings, replacing the version the server holds.
///
/// The version is read now rather than remembered: a second machine may have
/// written since this one last looked.
pub async fn upload(client: &WorkspaceClient) -> anyhow::Result<BackupWrite> {
    let settings = Settings::load_sync()?;
    let payload = sync::build(&settings, &AuthStore::load(), client.base())?;
    let expected = client
        .backup()
        .await?
        .map(|stored| stored.version)
        .unwrap_or(0);
    Ok(client
        .put_backup(&serde_json::to_value(&payload)?, expected)
        .await?)
}

/// Do the startup work: providers first, then the policy.
///
/// Providers first because the policy may name a model, and a model is only
/// reachable through a provider this account has.
pub async fn pull_at_startup(client: &WorkspaceClient) {
    match pull_providers(client).await {
        Ok(applied) if !applied.is_empty() => debug!(
            written = applied.written.len(),
            withdrawn = applied.withdrawn.len(),
            "the company providers were refreshed"
        ),
        Ok(_) => {}
        Err(error) => warn!(%error, "the company providers were not refreshed"),
    }
    if let Err(error) = policy::refresh(client).await {
        warn!(%error, "the company policy was not refreshed");
    }
}

/// Run the triggers a session is configured for until it is cancelled.
///
/// Three of them share one loop rather than one task each: they all end in the
/// same upload, and two uploads racing would produce a conflict this machine
/// caused itself.
pub async fn run_triggers(client: WorkspaceClient, workspace: WorkspaceSettings, cancel: Cancel) {
    let path = Settings::global_settings_path();
    let mut last_seen = modified_at(&path);
    let mut pending_since: Option<std::time::Instant> = None;
    let mut next_timer = workspace
        .sync
        .interval_minutes
        .map(interval_of)
        .map(tokio::time::interval);

    let mut poll = tokio::time::interval(Duration::from_secs(POLL_SECS));
    loop {
        let timer_tick = async {
            match next_timer.as_mut() {
                Some(timer) => {
                    timer.tick().await;
                }
                // Nothing to wait for; leave this arm parked rather than
                // spinning the select.
                None => std::future::pending::<()>().await,
            }
        };

        let reason = tokio::select! {
            _ = cancel.cancelled() => return,
            _ = poll.tick() => {
                if !workspace.sync.on_change {
                    continue;
                }
                let seen = modified_at(&path);
                if seen != last_seen {
                    last_seen = seen;
                    // Restart the wait on every write, so a burst of them
                    // becomes one upload rather than several.
                    pending_since = Some(std::time::Instant::now());
                    continue;
                }
                match pending_since {
                    Some(since) if since.elapsed() >= Duration::from_secs(DEBOUNCE_SECS) => {
                        pending_since = None;
                        "a settings change"
                    }
                    _ => continue,
                }
            }
            _ = timer_tick => {
                // A change made by another process, or an editor writing the
                // file in a way the poll missed, is what the timer is for.
                pending_since = None;
                "the timer"
            }
        };

        match upload(&client).await {
            Ok(BackupWrite::Stored { version, .. }) => {
                debug!(version, "uploaded the settings backup after {reason}")
            }
            // Never silent: two machines hold different settings, and only the
            // person using them can say which is right.
            Ok(BackupWrite::Conflict { current_version }) => warn!(
                current_version,
                "another machine wrote the settings backup first; \
                 nothing was uploaded. Run `mikmik workspace restore` to see it."
            ),
            Err(error) => warn!(%error, "the settings backup was not uploaded after {reason}"),
        }
    }
}

/// What stops the trigger loop.
pub type Cancel = CancellationToken;

/// The timer period, never shorter than [`MIN_INTERVAL_MINUTES`].
fn interval_of(minutes: u64) -> Duration {
    Duration::from_secs(minutes.max(MIN_INTERVAL_MINUTES) * 60)
}

/// When the settings file was last written, or `None` if it is not there.
fn modified_at(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timer_below_the_floor_is_raised_to_it() {
        // A one-minute timer would be an upload every minute for a file that
        // changes a few times a day.
        assert_eq!(
            interval_of(1),
            Duration::from_secs(MIN_INTERVAL_MINUTES * 60)
        );
        assert_eq!(
            interval_of(0),
            Duration::from_secs(MIN_INTERVAL_MINUTES * 60)
        );
    }

    #[test]
    fn a_timer_above_the_floor_is_left_alone() {
        assert_eq!(interval_of(60), Duration::from_secs(3600));
    }

    #[test]
    fn a_missing_file_reads_as_no_timestamp_rather_than_panicking() {
        assert_eq!(modified_at(std::path::Path::new("/nowhere/at/all")), None);
    }

    #[test]
    fn an_installation_with_no_server_does_not_connect() {
        assert!(connect(&Settings::default()).is_none());
    }

    #[test]
    fn an_installation_with_no_session_does_not_connect() {
        // The address alone is not a login. Without a token every call would
        // answer 401, and a session must not spend its startup finding out.
        let settings = Settings {
            workspace: Some(WorkspaceSettings {
                url: "https://mikmik.firma.com".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(connect(&settings).is_none());
    }
}
