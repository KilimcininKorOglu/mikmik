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

/// Turns a run of writes into one upload.
///
/// Held apart from the loop it runs in so it can be driven with times of the
/// test's choosing. A burst of writes has to become one upload, and that is
/// not something a loop with a real clock in it can be asked about.
#[derive(Debug)]
struct Debounce {
    /// The timestamp the settings file carried when it was last looked at.
    last_seen: Option<std::time::SystemTime>,
    /// When the most recent write was noticed. `None` means nothing is
    /// waiting.
    pending_since: Option<std::time::Instant>,
}

impl Debounce {
    fn new(last_seen: Option<std::time::SystemTime>) -> Self {
        Self {
            last_seen,
            pending_since: None,
        }
    }

    /// Answer whether the writes have stopped for long enough to upload.
    ///
    /// Every write restarts the wait, so two writes in a row produce one
    /// upload rather than two.
    fn poll(&mut self, seen: Option<std::time::SystemTime>, now: std::time::Instant) -> bool {
        if seen != self.last_seen {
            self.last_seen = seen;
            self.pending_since = Some(now);
            return false;
        }
        match self.pending_since {
            Some(since) if now.duration_since(since) >= Duration::from_secs(DEBOUNCE_SECS) => {
                self.pending_since = None;
                true
            }
            _ => false,
        }
    }

    /// Forget a wait in progress, because something else has just uploaded.
    fn clear(&mut self) {
        self.pending_since = None;
    }
}

/// Run the triggers a session is configured for until it is cancelled.
///
/// Three of them share one loop rather than one task each: they all end in the
/// same upload, and two uploads racing would produce a conflict this machine
/// caused itself.
pub async fn run_triggers(client: WorkspaceClient, workspace: WorkspaceSettings, cancel: Cancel) {
    let path = Settings::global_settings_path();
    let mut debounce = Debounce::new(modified_at(&path));
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
                if !debounce.poll(modified_at(&path), std::time::Instant::now()) {
                    continue;
                }
                "a settings change"
            }
            _ = timer_tick => {
                // A change made by another process, or an editor writing the
                // file in a way the poll missed, is what the timer is for.
                debounce.clear();
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

    /// A timestamp `seconds` after the base, standing in for one write.
    fn written_at(base: std::time::SystemTime, seconds: u64) -> Option<std::time::SystemTime> {
        base.checked_add(Duration::from_secs(seconds))
    }

    #[test]
    fn two_writes_in_a_row_become_one_upload() {
        // An editor save, or the app writing a settings change of its own,
        // produces several writes within seconds. Without this every one of
        // them would be an upload.
        let base = std::time::SystemTime::UNIX_EPOCH;
        let start = std::time::Instant::now();
        let mut debounce = Debounce::new(written_at(base, 0));

        // First write noticed: the wait starts, nothing is uploaded.
        assert!(!debounce.poll(written_at(base, 10), start));
        // Second write, one poll later: the wait restarts.
        let second = start + Duration::from_secs(POLL_SECS);
        assert!(!debounce.poll(written_at(base, 20), second));

        // The original wait would have run out here. It must not fire, because
        // the second write pushed it back.
        let would_have_fired = start + Duration::from_secs(DEBOUNCE_SECS);
        assert!(
            !debounce.poll(written_at(base, 20), would_have_fired),
            "the second write did not restart the wait"
        );

        // Once the writes have stopped for long enough, exactly one upload.
        let settled = second + Duration::from_secs(DEBOUNCE_SECS);
        assert!(debounce.poll(written_at(base, 20), settled));
        assert!(
            !debounce.poll(written_at(base, 20), settled + Duration::from_secs(3600)),
            "one settled change uploaded twice"
        );
    }

    #[test]
    fn a_file_that_never_changes_never_uploads() {
        let base = std::time::SystemTime::UNIX_EPOCH;
        let start = std::time::Instant::now();
        let mut debounce = Debounce::new(written_at(base, 0));

        for tick in 0..20 {
            let now = start + Duration::from_secs(POLL_SECS * tick);
            assert!(!debounce.poll(written_at(base, 0), now), "tick {tick}");
        }
    }

    #[test]
    fn the_timer_cancels_a_wait_it_has_already_covered() {
        // The timer uploads whatever is on disk, so a wait still running would
        // upload the same file again a moment later.
        let base = std::time::SystemTime::UNIX_EPOCH;
        let start = std::time::Instant::now();
        let mut debounce = Debounce::new(written_at(base, 0));

        assert!(!debounce.poll(written_at(base, 10), start));
        debounce.clear();

        let settled = start + Duration::from_secs(DEBOUNCE_SECS);
        assert!(
            !debounce.poll(written_at(base, 10), settled),
            "the timer uploaded and the wait uploaded the same change again"
        );
    }

    #[tokio::test]
    async fn a_server_that_cannot_be_reached_changes_no_local_configuration() {
        // Whatever is already configured has to keep working. The pull writes
        // nothing until the server has answered.
        //
        // `tokio::sync::Mutex` rather than the standard one, because the guard
        // is held across the `.await` below and a `std` guard is `!Send`.
        static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = HOME_LOCK.lock().await;

        let tmp = tempfile::tempdir().expect("temp dir");
        let saved = std::env::var_os("MIKMIK_HOME");
        std::env::set_var("MIKMIK_HOME", tmp.path());

        // Port 1 needs root to bind, so nothing is listening there.
        const SERVER: &str = "http://127.0.0.1:1";
        let unreachable = WorkspaceSettings {
            url: SERVER.to_string(),
            ..Default::default()
        };
        // A company provider already on the machine. The failure this guards
        // against is reading "the server did not answer" as "nothing is
        // assigned any more" and withdrawing it.
        let settings = Settings {
            providers: [
                ("mine".to_string(), crate::config::ProviderConfig::default()),
                (
                    "firma-openai".to_string(),
                    crate::config::ProviderConfig {
                        managed_by: Some(SERVER.to_string()),
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            workspace: Some(unreachable.clone()),
            ..Default::default()
        };
        let write = settings.save_sync();
        let before = std::fs::read_to_string(Settings::global_settings_path());

        let client = WorkspaceClient::new(&unreachable).expect("client");
        let pulled = pull_providers(&client).await;

        let after = std::fs::read_to_string(Settings::global_settings_path());
        let reloaded = Settings::load_sync();

        match saved {
            Some(value) => std::env::set_var("MIKMIK_HOME", value),
            None => std::env::remove_var("MIKMIK_HOME"),
        }

        write.expect("the settings were written");
        assert!(pulled.is_err(), "an unreachable server answered providers");
        let reloaded = reloaded.expect("the settings still parse");
        assert!(
            reloaded.providers.contains_key("firma-openai"),
            "a failed pull withdrew a company provider"
        );
        assert!(
            reloaded.providers.contains_key("mine"),
            "a failed pull removed the user's own provider"
        );
        assert_eq!(
            before.expect("the file exists"),
            after.expect("the file still exists"),
            "a failed pull rewrote the settings file"
        );
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
