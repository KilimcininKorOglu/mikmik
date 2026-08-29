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
const SCHEMA: &[&str] = &[
    crate::accounts::SCHEMA,
    crate::providers::SCHEMA,
    crate::policy::SCHEMA,
    crate::backup::SCHEMA,
    crate::audit::SCHEMA,
];

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
        Self::migrate(conn)?;
        Ok(())
    }

    /// Bring a database opened from an earlier build up to the current schema.
    ///
    /// `CREATE TABLE IF NOT EXISTS` cannot add a column to a table that already
    /// exists, so a column added after a table shipped needs an `ALTER`. A
    /// fresh database already has the column from its `CREATE TABLE`, so the
    /// `ALTER` fails there with a duplicate-column error that is expected and
    /// swallowed; any other failure is real and returned.
    fn migrate(conn: &Connection) -> anyhow::Result<()> {
        match conn.execute(
            "ALTER TABLE providers ADD COLUMN kind TEXT NOT NULL DEFAULT 'llm'",
            [],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("duplicate column name") =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
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
    fn a_providers_table_without_kind_gains_it_with_an_llm_default() {
        // A database from a build before `kind` shipped: the table exists
        // without the column, so `CREATE TABLE IF NOT EXISTS` is a no-op and
        // only the migration adds it. The existing row must read back as `llm`.
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE providers (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL UNIQUE,
                 protocol TEXT,
                 api_base TEXT,
                 api_key TEXT NOT NULL,
                 models_json TEXT NOT NULL DEFAULT '[]',
                 created_at INTEGER NOT NULL
             );",
        )
        .expect("old table");
        conn.execute(
            "INSERT INTO providers (id, name, api_key, created_at) VALUES ('1', 'openai', 'x', 0)",
            [],
        )
        .expect("row");

        Store::migrate(&conn).expect("migration adds the column");

        let kind: String = conn
            .query_row("SELECT kind FROM providers WHERE id = '1'", [], |row| {
                row.get(0)
            })
            .expect("kind readable");
        assert_eq!(kind, "llm");

        // Running it again over a table that already has the column is a no-op,
        // not an error, so opening an up-to-date database keeps working.
        Store::migrate(&conn).expect("second run is a no-op");
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
