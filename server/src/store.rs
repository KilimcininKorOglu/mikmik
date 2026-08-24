//! SQLite storage.
//!
//! One connection behind a mutex rather than a pool. Every row this server
//! holds is small and every query is short, so the contention a pool would
//! relieve does not exist yet, and a single connection removes a dependency
//! and a configuration knob.
//!
//! Every method here is synchronous on purpose. The guard is `!Send`, so a
//! method that awaited while holding it would not compile inside an axum
//! handler; keeping the whole surface synchronous means that can never be
//! written by accident.

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::Connection;

/// Every module's tables, applied in order at open.
///
/// Each statement is `IF NOT EXISTS`, so opening an existing database is a
/// no-op and there is no migration step to run or forget. A module owns its
/// own `SCHEMA` constant and is listed here.
const SCHEMA: &[&str] = &[crate::accounts::SCHEMA];

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (or create) the database at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory database, for tests.
    #[allow(dead_code)]
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn prepare(conn: &Connection) -> anyhow::Result<()> {
        // Every table added from here on references another one, so an
        // orphaned membership or assignment must not survive a deleted user.
        //
        // Redundant against today's build: `libsqlite3-sys` compiles the
        // bundled SQLite with `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`. Stated here
        // anyway, because that default is a transitive build script's flag and
        // linking a system SQLite instead would silently turn it off.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // WAL lets a reader run while a writer commits, which matters as soon
        // as the web interface polls while a client uploads.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        for statements in SCHEMA {
            conn.execute_batch(statements)?;
        }
        Ok(())
    }

    /// Run `f` against the connection.
    ///
    /// Callers stay inside the closure, which is what keeps the guard from
    /// escaping into an async context.
    #[allow(dead_code)]
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        let conn = self.conn.lock();
        Ok(f(&conn)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragma(store: &Store, name: &str) -> String {
        store
            .with(|conn| conn.pragma_query_value(None, name, |row| row.get::<_, String>(0)))
            .expect("pragma readable")
    }

    #[test]
    fn opening_creates_the_file_and_its_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("server.sqlite");
        let _store = Store::open(&path).expect("open");
        assert!(path.exists(), "the database file was not created");
    }

    #[test]
    fn opening_the_same_file_twice_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("server.sqlite");
        let _first = Store::open(&path).expect("first open");
        let _second = Store::open(&path).expect("second open");
    }

    #[test]
    fn foreign_keys_are_enforced() {
        // Asserts the property, not the pragma line: the bundled build already
        // defaults this on, so removing that line does not fail here. What
        // this catches is a switch to a system SQLite, where the default is
        // off and every later table's references would become decoration.
        let store = Store::open_in_memory().expect("open");
        let enabled: i64 = store
            .with(|conn| conn.pragma_query_value(None, "foreign_keys", |row| row.get(0)))
            .expect("pragma readable");
        assert_eq!(enabled, 1, "foreign keys are not enforced");
    }

    #[test]
    fn a_file_database_runs_in_wal_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("server.sqlite");
        let store = Store::open(&path).expect("open");
        assert_eq!(pragma(&store, "journal_mode").to_lowercase(), "wal");
    }
}
