//! The audit log.
//!
//! Every action that changes something, and every attempt to reach something
//! that needs a credential, leaves a row. An organisation that hands out API
//! keys has to be able to answer "who took what, and when".
//!
//! A failed write fails the request. An audit log that silently drops entries
//! is worse than none, because it reads as a complete record; the insert runs
//! on the same connection as the work, so a failure here means the database is
//! unhealthy and the answer should say so.

use rusqlite::params;
use serde::Serialize;

use crate::accounts::now_secs;
use crate::store::Store;

/// The table this module owns.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS audit_log (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       INTEGER NOT NULL,
    actor_id TEXT,
    subject  TEXT,
    action   TEXT NOT NULL,
    target   TEXT,
    detail   TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_at ON audit_log(at DESC, id DESC);
";

/// What happened.
///
/// Named as `area.verb`, so a reader can filter by prefix and a new action in
/// an existing area does not need a new naming rule.
pub mod action {
    pub const LOGIN_OK: &str = "login.ok";
    pub const LOGIN_REFUSED: &str = "login.refused";
    pub const LOGOUT: &str = "logout";
    pub const PROVIDERS_FETCH: &str = "providers.fetch";
    pub const POLICY_FETCH: &str = "policy.fetch";
    pub const SETTINGS_READ: &str = "settings.read";
    pub const SETTINGS_WRITE: &str = "settings.write";
    pub const SETTINGS_CONFLICT: &str = "settings.conflict";
    pub const SETTINGS_CLEAR: &str = "settings.clear";
    pub const ADMIN_USER_CREATE: &str = "admin.user.create";
    pub const ADMIN_PROVIDER_CREATE: &str = "admin.provider.create";
    pub const ADMIN_PROVIDER_DELETE: &str = "admin.provider.delete";
    pub const ADMIN_GROUP_CREATE: &str = "admin.group.create";
    pub const ADMIN_GROUP_DELETE: &str = "admin.group.delete";
    pub const ADMIN_MEMBERSHIP_ADD: &str = "admin.membership.add";
    pub const ADMIN_MEMBERSHIP_REMOVE: &str = "admin.membership.remove";
    pub const ADMIN_ASSIGNMENT_ADD: &str = "admin.assignment.add";
    pub const ADMIN_ASSIGNMENT_REMOVE: &str = "admin.assignment.remove";
    pub const ADMIN_POLICY_WRITE: &str = "admin.policy.write";
    pub const ADMIN_POLICY_CLEAR: &str = "admin.policy.clear";
}

/// One recorded action.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    pub at: i64,
    pub actor_id: Option<String>,
    /// The address a login was attempted with, when there is no account behind
    /// it. Without this a refused login names nobody at all.
    pub subject: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
}

/// What a caller passes to record one action.
///
/// A struct rather than six positional arguments, because five of them are
/// `Option<&str>` and a call site that swapped two would compile.
#[derive(Debug, Default, Clone, Copy)]
pub struct Record<'a> {
    pub actor_id: Option<&'a str>,
    pub subject: Option<&'a str>,
    pub action: &'a str,
    pub target: Option<&'a str>,
    pub detail: Option<&'a str>,
}

/// Write one row.
///
/// Nothing here is a place for a secret. The caller passes an identifier or a
/// short description; a password, a token or an API key must never reach
/// `detail`, and `no_recorded_field_carries_a_secret` checks that the call
/// sites hold to it.
pub fn record(store: &Store, entry: Record<'_>) -> anyhow::Result<()> {
    store.with(|conn| {
        conn.execute(
            "INSERT INTO audit_log (at, actor_id, subject, action, target, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                now_secs(),
                entry.actor_id,
                entry.subject,
                entry.action,
                entry.target,
                entry.detail
            ],
        )
    })?;
    Ok(())
}

/// The most recent entries first.
///
/// `before` continues a listing: pass the smallest id from the previous page.
pub fn list(store: &Store, limit: usize, before: Option<i64>) -> anyhow::Result<Vec<Entry>> {
    let limit = limit.clamp(1, 500) as i64;
    store.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, at, actor_id, subject, action, target, detail
             FROM audit_log
             WHERE (?1 IS NULL OR id < ?1)
             ORDER BY id DESC
             LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![before, limit], |row| {
                Ok(Entry {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    actor_id: row.get(2)?,
                    subject: row.get(3)?,
                    action: row.get(4)?,
                    target: row.get(5)?,
                    detail: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("store")
    }

    fn write(store: &Store, action: &str) {
        record(
            store,
            Record {
                action,
                ..Record::default()
            },
        )
        .expect("recorded");
    }

    #[test]
    fn an_empty_log_lists_nothing() {
        assert!(list(&store(), 50, None).expect("query").is_empty());
    }

    #[test]
    fn the_newest_entry_comes_first() {
        let store = store();
        write(&store, "first");
        write(&store, "second");

        let entries = list(&store, 50, None).expect("query");
        assert_eq!(entries[0].action, "second");
        assert_eq!(entries[1].action, "first");
    }

    #[test]
    fn a_refused_login_names_the_address_it_was_tried_with() {
        // Without the subject, a refused login records nobody, which is the
        // one entry an administrator most wants to read.
        let store = store();
        record(
            &store,
            Record {
                subject: Some("nobody@firma.com"),
                action: action::LOGIN_REFUSED,
                ..Record::default()
            },
        )
        .expect("recorded");

        let entry = &list(&store, 50, None).expect("query")[0];
        assert_eq!(entry.subject.as_deref(), Some("nobody@firma.com"));
        assert!(entry.actor_id.is_none());
    }

    #[test]
    fn a_listing_is_bounded_and_continues_from_where_it_stopped() {
        let store = store();
        for index in 0..10 {
            write(&store, &format!("action-{index}"));
        }

        let first = list(&store, 4, None).expect("query");
        assert_eq!(first.len(), 4);
        assert_eq!(first[0].action, "action-9");

        let smallest = first.last().expect("a row").id;
        let second = list(&store, 4, Some(smallest)).expect("query");
        assert_eq!(second.len(), 4);
        assert_eq!(second[0].action, "action-5");
    }

    #[test]
    fn a_nonsense_limit_is_clamped_rather_than_obeyed() {
        // A caller asking for zero would otherwise read an empty log and
        // conclude nothing happened.
        let store = store();
        write(&store, "one");
        assert_eq!(list(&store, 0, None).expect("query").len(), 1);
        assert_eq!(list(&store, usize::MAX, None).expect("query").len(), 1);
    }

    #[test]
    fn ids_never_repeat_after_a_delete() {
        // AUTOINCREMENT rather than a plain rowid: a reused id would make
        // `before` skip entries it had not shown.
        let store = store();
        write(&store, "one");
        let first = list(&store, 1, None).expect("query")[0].id;
        store
            .with(|conn| conn.execute("DELETE FROM audit_log", []))
            .expect("deleted");
        write(&store, "two");
        let second = list(&store, 1, None).expect("query")[0].id;
        assert!(second > first, "an id was reused");
    }
}
