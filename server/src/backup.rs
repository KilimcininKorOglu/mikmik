//! Each user's own settings backup.
//!
//! One row per account. The client uploads what it has, and restores it on a
//! new machine. The blob is sealed, because the decision was that a user's own
//! provider keys ride along with it.
//!
//! Every write carries the version it expects to replace. Two machines syncing
//! the same account is the normal case, not the exception, so a write that
//! would overwrite something the client has not seen is refused rather than
//! applied.

use rusqlite::{params, OptionalExtension};
use serde_json::Value;

use crate::accounts::now_secs;
use crate::crypt::Sealer;
use crate::policy::checksum;
use crate::store::Store;

/// The table this module owns.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS user_settings (
    user_id    TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    blob       TEXT NOT NULL,
    version    INTEGER NOT NULL,
    checksum   TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
";

/// A stored backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub settings: Value,
    pub version: i64,
    pub checksum: String,
}

/// Why a write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The version the store actually holds.
    pub current_version: i64,
}

/// Read a user's backup. Answers `None` when they have never uploaded.
pub fn get(store: &Store, sealer: &Sealer, user_id: &str) -> anyhow::Result<Option<Backup>> {
    let row: Option<(String, i64, String)> = store.with(|conn| {
        conn.query_row(
            "SELECT blob, version, checksum FROM user_settings WHERE user_id = ?1",
            params![user_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
    })?;

    let Some((sealed, version, checksum)) = row else {
        return Ok(None);
    };
    Ok(Some(Backup {
        settings: serde_json::from_str(&sealer.open(&sealed)?)?,
        version,
        checksum,
    }))
}

/// The version a user's backup is at. Zero when they have never uploaded.
pub fn current_version(store: &Store, user_id: &str) -> anyhow::Result<i64> {
    let version: Option<i64> = store.with(|conn| {
        conn.query_row(
            "SELECT version FROM user_settings WHERE user_id = ?1",
            params![user_id],
            |row| row.get(0),
        )
        .optional()
    })?;
    Ok(version.unwrap_or(0))
}

/// Store a backup, replacing the version the caller says it saw.
///
/// `expected_version` is 0 for the first upload. A mismatch answers the
/// conflict rather than writing, so a second machine cannot delete a change it
/// never had.
pub fn put(
    store: &Store,
    sealer: &Sealer,
    user_id: &str,
    settings: &Value,
    expected_version: i64,
) -> anyhow::Result<Result<Backup, Conflict>> {
    if !settings.is_object() {
        anyhow::bail!("a settings backup has to be a JSON object");
    }

    let current = current_version(store, user_id)?;
    if current != expected_version {
        return Ok(Err(Conflict {
            current_version: current,
        }));
    }

    let next = current + 1;
    let sum = checksum(settings);
    let sealed = sealer.seal(&serde_json::to_string(settings)?)?;

    // The version is part of the `WHERE`, so two writers racing on the same
    // expected version cannot both succeed: the second updates no row.
    let written = store.with(|conn| {
        if current == 0 {
            conn.execute(
                "INSERT INTO user_settings (user_id, blob, version, checksum, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![user_id, sealed, next, sum, now_secs()],
            )
        } else {
            conn.execute(
                "UPDATE user_settings SET blob = ?2, version = ?3, checksum = ?4, updated_at = ?5
                 WHERE user_id = ?1 AND version = ?6",
                params![user_id, sealed, next, sum, now_secs(), current],
            )
        }
    })?;

    if written == 0 {
        return Ok(Err(Conflict {
            current_version: current_version(store, user_id)?,
        }));
    }

    Ok(Ok(Backup {
        settings: settings.clone(),
        version: next,
        checksum: sum,
    }))
}

/// Remove a user's backup.
pub fn clear(store: &Store, user_id: &str) -> anyhow::Result<bool> {
    let removed = store.with(|conn| {
        conn.execute(
            "DELETE FROM user_settings WHERE user_id = ?1",
            params![user_id],
        )
    })?;
    Ok(removed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts;
    use serde_json::json;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";
    const PASSWORD: &str = "correct horse battery";

    fn fixture() -> (Store, Sealer, String) {
        let store = Store::open_in_memory().expect("store");
        let user = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        (store, Sealer::new(SECRET), user)
    }

    #[test]
    fn nothing_is_stored_until_something_is_uploaded() {
        let (store, sealer, user) = fixture();
        assert!(get(&store, &sealer, &user).expect("query").is_none());
        assert_eq!(current_version(&store, &user).expect("query"), 0);
    }

    #[test]
    fn the_first_upload_is_version_one() {
        let (store, sealer, user) = fixture();
        let stored = put(&store, &sealer, &user, &json!({ "a": 1 }), 0)
            .expect("stored")
            .expect("no conflict");
        assert_eq!(stored.version, 1);
    }

    #[test]
    fn a_backup_round_trips() {
        let (store, sealer, user) = fixture();
        let settings = json!({ "config": { "model": "claude-opus-4-6" } });
        put(&store, &sealer, &user, &settings, 0)
            .expect("stored")
            .expect("no conflict");

        let read = get(&store, &sealer, &user).expect("query").expect("stored");
        assert_eq!(read.settings, settings);
        assert_eq!(read.version, 1);
    }

    #[test]
    fn the_stored_blob_is_never_readable() {
        // The backup carries the user's own provider keys, so the row must not
        // be readable from a database copy alone.
        let (store, sealer, user) = fixture();
        put(
            &store,
            &sealer,
            &user,
            &json!({ "providers": { "openai": { "api_key": "key-in-the-backup" } } }),
            0,
        )
        .expect("stored")
        .expect("no conflict");

        let raw: String = store
            .with(|conn| conn.query_row("SELECT blob FROM user_settings", [], |row| row.get(0)))
            .expect("query");
        assert!(!raw.contains("key-in-the-backup"), "the backup is readable");
    }

    #[test]
    fn each_upload_moves_the_version_on() {
        let (store, sealer, user) = fixture();
        put(&store, &sealer, &user, &json!({ "a": 1 }), 0)
            .expect("s")
            .expect("ok");
        let second = put(&store, &sealer, &user, &json!({ "a": 2 }), 1)
            .expect("s")
            .expect("ok");
        assert_eq!(second.version, 2);
        assert_eq!(
            get(&store, &sealer, &user)
                .expect("q")
                .expect("stored")
                .settings,
            json!({ "a": 2 })
        );
    }

    #[test]
    fn a_stale_write_is_refused_and_names_the_current_version() {
        // The second machine still thinks the backup is at version 1.
        let (store, sealer, user) = fixture();
        put(&store, &sealer, &user, &json!({ "a": 1 }), 0)
            .expect("s")
            .expect("ok");
        put(&store, &sealer, &user, &json!({ "a": 2 }), 1)
            .expect("s")
            .expect("ok");

        let conflict = put(&store, &sealer, &user, &json!({ "a": 3 }), 1)
            .expect("no error")
            .expect_err("a conflict");
        assert_eq!(conflict.current_version, 2);
    }

    #[test]
    fn a_refused_write_changes_nothing() {
        let (store, sealer, user) = fixture();
        put(&store, &sealer, &user, &json!({ "a": 1 }), 0)
            .expect("s")
            .expect("ok");
        let _ = put(&store, &sealer, &user, &json!({ "a": 99 }), 7).expect("no error");

        let read = get(&store, &sealer, &user).expect("q").expect("stored");
        assert_eq!(read.settings, json!({ "a": 1 }));
        assert_eq!(read.version, 1);
    }

    #[test]
    fn a_first_upload_that_expects_a_version_is_refused() {
        // A client claiming to replace version 3 when nothing is stored has
        // the wrong account or a stale cache; either way it must not write.
        let (store, sealer, user) = fixture();
        let conflict = put(&store, &sealer, &user, &json!({ "a": 1 }), 3)
            .expect("no error")
            .expect_err("a conflict");
        assert_eq!(conflict.current_version, 0);
    }

    #[test]
    fn one_account_cannot_read_another() {
        let (store, sealer, ayse) = fixture();
        let bora = accounts::create_user(&store, "bora@firma.com", PASSWORD, false).expect("user");
        put(&store, &sealer, &ayse, &json!({ "a": 1 }), 0)
            .expect("s")
            .expect("ok");

        assert!(get(&store, &sealer, &bora).expect("query").is_none());
    }

    #[test]
    fn a_backup_that_is_not_an_object_is_refused() {
        let (store, sealer, user) = fixture();
        for bad in [json!([]), json!("text"), json!(1), json!(null)] {
            assert!(
                put(&store, &sealer, &user, &bad, 0).is_err(),
                "{bad} passed"
            );
        }
    }

    #[test]
    fn deleting_an_account_takes_its_backup_with_it() {
        let (store, sealer, user) = fixture();
        put(&store, &sealer, &user, &json!({ "a": 1 }), 0)
            .expect("s")
            .expect("ok");
        store
            .with(|conn| conn.execute("DELETE FROM users WHERE id = ?1", params![user]))
            .expect("deleted");

        let left: i64 = store
            .with(|conn| conn.query_row("SELECT COUNT(*) FROM user_settings", [], |row| row.get(0)))
            .expect("query");
        assert_eq!(left, 0, "an orphaned backup outlived its account");
    }

    #[test]
    fn clearing_removes_it_and_resets_the_version() {
        let (store, sealer, user) = fixture();
        put(&store, &sealer, &user, &json!({ "a": 1 }), 0)
            .expect("s")
            .expect("ok");

        assert!(clear(&store, &user).expect("cleared"));
        assert_eq!(current_version(&store, &user).expect("query"), 0);
        assert!(!clear(&store, &user).expect("cleared"));
    }

    #[test]
    fn a_secret_change_makes_a_backup_unreadable_rather_than_wrong() {
        // Losing the secret must fail loudly, not answer with something that
        // parses into settings the user never wrote.
        let (store, sealer, user) = fixture();
        put(&store, &sealer, &user, &json!({ "a": 1 }), 0)
            .expect("s")
            .expect("ok");

        let other = Sealer::new("fedcba9876543210fedcba9876543210");
        assert!(get(&store, &other, &user).is_err());
    }
}
