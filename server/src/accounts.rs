//! Users and their sessions.

use rusqlite::{params, OptionalExtension};

use crate::auth;
use crate::store::Store;

/// Tables this module owns.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    disabled_at   INTEGER
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
";

/// An account, as every caller outside this module sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub email: String,
    pub is_admin: bool,
    pub disabled: bool,
}

/// Fold an address to the one form the database stores.
///
/// Without this, two accounts could differ only by case and the second would
/// be a second identity for the same person.
pub fn normalise_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Seconds since the Unix epoch.
///
/// `SystemTime` rather than `Instant` on purpose: these values are written to
/// the database and compared across restarts, which a monotonic clock cannot
/// do.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// A new opaque identifier.
fn new_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("the OS random number generator failed: {e}"))?;
    Ok(hex::encode(bytes))
}

/// Create an account. Answers its id.
pub fn create_user(
    store: &Store,
    email: &str,
    password: &str,
    is_admin: bool,
) -> anyhow::Result<String> {
    let email = normalise_email(email);
    if email.is_empty() {
        anyhow::bail!("an account needs an email address");
    }
    let hash = auth::hash_password(password)?;
    let id = new_id()?;
    store.with(|conn| {
        conn.execute(
            "INSERT INTO users (id, email, password_hash, is_admin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, email, hash, is_admin as i64, now_secs()],
        )
    })?;
    Ok(id)
}

/// Look up an account by address, whatever case it was typed in.
pub fn find_by_email(store: &Store, email: &str) -> anyhow::Result<Option<(User, String)>> {
    let email = normalise_email(email);
    store.with(|conn| {
        conn.query_row(
            "SELECT id, email, password_hash, is_admin, disabled_at
             FROM users WHERE email = ?1",
            params![email],
            |row| {
                Ok((
                    User {
                        id: row.get(0)?,
                        email: row.get(1)?,
                        is_admin: row.get::<_, i64>(3)? != 0,
                        disabled: row.get::<_, Option<i64>>(4)?.is_some(),
                    },
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
    })
}

/// Whether any account exists yet.
pub fn any_user_exists(store: &Store) -> anyhow::Result<bool> {
    let count: i64 =
        store.with(|conn| conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0)))?;
    Ok(count > 0)
}

/// Record a session and answer the token the caller must present.
///
/// Only the token's hash is stored, so this is the one moment the token
/// exists in a readable form.
pub fn open_session(store: &Store, user_id: &str, ttl_secs: i64) -> anyhow::Result<String> {
    let token = auth::new_session_token()?;
    let now = now_secs();
    store.with(|conn| {
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![auth::token_hash(&token), user_id, now, now + ttl_secs],
        )
    })?;
    Ok(token)
}

/// Resolve a presented token to the account it belongs to.
///
/// Answers `None` for a token that is unknown, expired, or belongs to a
/// disabled account. An expired row is deleted on the way past, so a session
/// nobody uses again does not sit in the table for ever.
pub fn session_user(store: &Store, token: &str) -> anyhow::Result<Option<User>> {
    let hash = auth::token_hash(token);
    let now = now_secs();

    let found: Option<(String, User, i64)> = store.with(|conn| {
        conn.query_row(
            "SELECT s.token_hash, s.expires_at, u.id, u.email, u.is_admin, u.disabled_at
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1",
            params![hash],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    User {
                        id: row.get(2)?,
                        email: row.get(3)?,
                        is_admin: row.get::<_, i64>(4)? != 0,
                        disabled: row.get::<_, Option<i64>>(5)?.is_some(),
                    },
                    row.get::<_, i64>(1)?,
                ))
            },
        )
        .optional()
    })?;

    let Some((stored_hash, user, expires_at)) = found else {
        return Ok(None);
    };

    // The lookup above already matched on the hash, so this only guards
    // against a future change that widens the query. It costs one comparison.
    if !auth::digest_matches(&stored_hash, &hash) {
        return Ok(None);
    }
    if expires_at <= now {
        close_session(store, token)?;
        return Ok(None);
    }
    if user.disabled {
        return Ok(None);
    }
    Ok(Some(user))
}

/// Drop a session.
pub fn close_session(store: &Store, token: &str) -> anyhow::Result<()> {
    let hash = auth::token_hash(token);
    store.with(|conn| conn.execute("DELETE FROM sessions WHERE token_hash = ?1", params![hash]))?;
    Ok(())
}

/// Delete every session whose time has passed.
pub fn sweep_expired_sessions(store: &Store) -> anyhow::Result<usize> {
    let now = now_secs();
    let removed = store
        .with(|conn| conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", params![now]))?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("store")
    }

    const PASSWORD: &str = "correct horse battery";

    #[test]
    fn an_account_is_found_by_its_address_in_any_case() {
        let store = store();
        create_user(&store, "Ayse@Firma.com", PASSWORD, false).expect("created");

        for typed in ["ayse@firma.com", "AYSE@FIRMA.COM", "  Ayse@Firma.com  "] {
            let found = find_by_email(&store, typed).expect("query");
            assert!(found.is_some(), "{typed} did not resolve to the account");
        }
    }

    #[test]
    fn the_same_address_cannot_be_taken_twice() {
        let store = store();
        create_user(&store, "ayse@firma.com", PASSWORD, false).expect("created");
        let second = create_user(&store, "AYSE@firma.com", PASSWORD, false);
        assert!(second.is_err(), "a second identity for one person was made");
    }

    #[test]
    fn an_account_needs_an_address() {
        let store = store();
        assert!(create_user(&store, "   ", PASSWORD, false).is_err());
    }

    #[test]
    fn the_stored_password_is_not_the_password() {
        let store = store();
        create_user(&store, "ayse@firma.com", PASSWORD, false).expect("created");
        let (_, hash) = find_by_email(&store, "ayse@firma.com")
            .expect("query")
            .expect("found");
        assert!(!hash.contains(PASSWORD));
        assert!(auth::verify_password(PASSWORD, &hash));
    }

    #[test]
    fn a_session_resolves_to_its_account() {
        let store = store();
        let id = create_user(&store, "ayse@firma.com", PASSWORD, true).expect("created");
        let token = open_session(&store, &id, 3600).expect("session");

        let user = session_user(&store, &token).expect("query").expect("live");
        assert_eq!(user.id, id);
        assert!(user.is_admin);
    }

    #[test]
    fn the_session_table_never_holds_the_token() {
        let store = store();
        let id = create_user(&store, "ayse@firma.com", PASSWORD, false).expect("created");
        let token = open_session(&store, &id, 3600).expect("session");

        let stored: String = store
            .with(|conn| conn.query_row("SELECT token_hash FROM sessions", [], |row| row.get(0)))
            .expect("query");
        assert_ne!(stored, token, "a database copy would hand over the session");
        assert_eq!(stored, auth::token_hash(&token));
    }

    #[test]
    fn an_expired_session_resolves_to_nothing_and_is_removed() {
        let store = store();
        let id = create_user(&store, "ayse@firma.com", PASSWORD, false).expect("created");
        let token = open_session(&store, &id, -1).expect("session");

        assert!(session_user(&store, &token).expect("query").is_none());
        let left: i64 = store
            .with(|conn| conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0)))
            .expect("query");
        assert_eq!(left, 0, "the expired row was left behind");
    }

    #[test]
    fn an_unknown_token_resolves_to_nothing() {
        let store = store();
        assert!(session_user(&store, "not-a-token")
            .expect("query")
            .is_none());
    }

    #[test]
    fn a_disabled_account_has_no_live_session() {
        let store = store();
        let id = create_user(&store, "ayse@firma.com", PASSWORD, false).expect("created");
        let token = open_session(&store, &id, 3600).expect("session");
        store
            .with(|conn| {
                conn.execute(
                    "UPDATE users SET disabled_at = ?1 WHERE id = ?2",
                    params![now_secs(), id],
                )
            })
            .expect("disabled");

        assert!(session_user(&store, &token).expect("query").is_none());
    }

    #[test]
    fn closing_a_session_ends_it() {
        let store = store();
        let id = create_user(&store, "ayse@firma.com", PASSWORD, false).expect("created");
        let token = open_session(&store, &id, 3600).expect("session");

        close_session(&store, &token).expect("closed");
        assert!(session_user(&store, &token).expect("query").is_none());
    }

    #[test]
    fn deleting_an_account_takes_its_sessions_with_it() {
        // The cascade only fires with foreign keys on, which is why the store
        // sets the pragma.
        let store = store();
        let id = create_user(&store, "ayse@firma.com", PASSWORD, false).expect("created");
        open_session(&store, &id, 3600).expect("session");
        store
            .with(|conn| conn.execute("DELETE FROM users WHERE id = ?1", params![id]))
            .expect("deleted");

        let left: i64 = store
            .with(|conn| conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0)))
            .expect("query");
        assert_eq!(left, 0, "an orphaned session outlived its account");
    }

    #[test]
    fn the_first_account_is_recognised_as_the_first() {
        let store = store();
        assert!(!any_user_exists(&store).expect("query"));
        create_user(&store, "ayse@firma.com", PASSWORD, true).expect("created");
        assert!(any_user_exists(&store).expect("query"));
    }

    #[test]
    fn sweeping_removes_only_the_expired() {
        let store = store();
        let id = create_user(&store, "ayse@firma.com", PASSWORD, false).expect("created");
        let live = open_session(&store, &id, 3600).expect("session");
        open_session(&store, &id, -1).expect("session");

        assert_eq!(sweep_expired_sessions(&store).expect("swept"), 1);
        assert!(session_user(&store, &live).expect("query").is_some());
    }
}
