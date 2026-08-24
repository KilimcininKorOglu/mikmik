//! Provider definitions, groups, and who may use which provider.
//!
//! A provider is one account the organisation holds with a model vendor: its
//! name, wire format, base URL, key and model list. A user reaches it either
//! because it is assigned to them directly or because it is assigned to a
//! group they belong to.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::accounts::now_secs;
use crate::crypt::Sealer;
use crate::store::Store;

/// Tables this module owns.
///
/// `user_groups` rather than `groups`, because `GROUPS` is a SQLite keyword.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS providers (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    protocol    TEXT,
    api_base    TEXT,
    api_key     TEXT NOT NULL,
    models_json TEXT NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS user_groups (
    id   TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS memberships (
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    group_id TEXT NOT NULL REFERENCES user_groups(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, group_id)
);
CREATE TABLE IF NOT EXISTS assignments (
    provider_id  TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('user', 'group')),
    subject_id   TEXT NOT NULL,
    PRIMARY KEY (provider_id, subject_kind, subject_id)
);
CREATE INDEX IF NOT EXISTS idx_assignments_subject
    ON assignments(subject_kind, subject_id);
";

/// What an administrator writes when defining a provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderInput {
    pub name: String,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub api_base: Option<String>,
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
}

/// A provider as the administration surface sees it: no key.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub protocol: Option<String>,
    pub api_base: Option<String>,
    pub models: Vec<String>,
}

/// A provider as an entitled user receives it: with the key.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntitledProvider {
    pub name: String,
    pub protocol: Option<String>,
    pub api_base: Option<String>,
    pub api_key: String,
    pub models: Vec<String>,
}

/// A group.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
}

/// Who an assignment names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubjectKind {
    User,
    Group,
}

impl SubjectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
        }
    }
}

fn new_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("the OS random number generator failed: {e}"))?;
    Ok(hex::encode(bytes))
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// Define a provider. Its key is sealed before it reaches the table.
pub fn create_provider(
    store: &Store,
    sealer: &Sealer,
    input: &ProviderInput,
) -> anyhow::Result<String> {
    let name = input.name.trim();
    if name.is_empty() {
        anyhow::bail!("a provider needs a name");
    }
    if input.api_key.trim().is_empty() {
        anyhow::bail!("a provider needs an api key");
    }
    let id = new_id()?;
    let sealed = sealer.seal(&input.api_key)?;
    let models = serde_json::to_string(&input.models)?;

    store.with(|conn| {
        conn.execute(
            "INSERT INTO providers (id, name, protocol, api_base, api_key, models_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                name,
                input.protocol,
                input.api_base,
                sealed,
                models,
                now_secs()
            ],
        )
    })?;
    Ok(id)
}

/// Every provider, without any key.
pub fn list_providers(store: &Store) -> anyhow::Result<Vec<ProviderSummary>> {
    store.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT id, name, protocol, api_base, models_json FROM providers ORDER BY name",
        )?;
        let rows = statement
            .query_map([], |row| {
                let models: String = row.get(4)?;
                Ok(ProviderSummary {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    protocol: row.get(2)?,
                    api_base: row.get(3)?,
                    models: serde_json::from_str(&models).unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

/// Remove a provider. Its assignments go with it.
pub fn delete_provider(store: &Store, id: &str) -> anyhow::Result<bool> {
    let removed =
        store.with(|conn| conn.execute("DELETE FROM providers WHERE id = ?1", params![id]))?;
    Ok(removed > 0)
}

// ---------------------------------------------------------------------------
// Groups and membership
// ---------------------------------------------------------------------------

pub fn create_group(store: &Store, name: &str) -> anyhow::Result<String> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("a group needs a name");
    }
    let id = new_id()?;
    store.with(|conn| {
        conn.execute(
            "INSERT INTO user_groups (id, name) VALUES (?1, ?2)",
            params![id, name],
        )
    })?;
    Ok(id)
}

pub fn list_groups(store: &Store) -> anyhow::Result<Vec<Group>> {
    store.with(|conn| {
        let mut statement = conn.prepare("SELECT id, name FROM user_groups ORDER BY name")?;
        let rows = statement
            .query_map([], |row| {
                Ok(Group {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

pub fn delete_group(store: &Store, id: &str) -> anyhow::Result<bool> {
    let removed =
        store.with(|conn| conn.execute("DELETE FROM user_groups WHERE id = ?1", params![id]))?;
    Ok(removed > 0)
}

/// Put a user in a group. Doing it twice is not an error.
pub fn add_membership(store: &Store, user_id: &str, group_id: &str) -> anyhow::Result<()> {
    store.with(|conn| {
        conn.execute(
            "INSERT OR IGNORE INTO memberships (user_id, group_id) VALUES (?1, ?2)",
            params![user_id, group_id],
        )
    })?;
    Ok(())
}

/// Take a user out of a group.
pub fn remove_membership(store: &Store, user_id: &str, group_id: &str) -> anyhow::Result<bool> {
    let removed = store.with(|conn| {
        conn.execute(
            "DELETE FROM memberships WHERE user_id = ?1 AND group_id = ?2",
            params![user_id, group_id],
        )
    })?;
    Ok(removed > 0)
}

/// The groups a user belongs to.
pub fn groups_for_user(store: &Store, user_id: &str) -> anyhow::Result<Vec<Group>> {
    store.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT g.id, g.name FROM user_groups g
             JOIN memberships m ON m.group_id = g.id
             WHERE m.user_id = ?1 ORDER BY g.name",
        )?;
        let rows = statement
            .query_map(params![user_id], |row| {
                Ok(Group {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

// ---------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------

/// Give a provider to a user or a group. Doing it twice is not an error.
pub fn assign(
    store: &Store,
    provider_id: &str,
    kind: SubjectKind,
    subject_id: &str,
) -> anyhow::Result<()> {
    store.with(|conn| {
        conn.execute(
            "INSERT OR IGNORE INTO assignments (provider_id, subject_kind, subject_id)
             VALUES (?1, ?2, ?3)",
            params![provider_id, kind.as_str(), subject_id],
        )
    })?;
    Ok(())
}

/// Take a provider away from a user or a group.
pub fn unassign(
    store: &Store,
    provider_id: &str,
    kind: SubjectKind,
    subject_id: &str,
) -> anyhow::Result<bool> {
    let removed = store.with(|conn| {
        conn.execute(
            "DELETE FROM assignments
             WHERE provider_id = ?1 AND subject_kind = ?2 AND subject_id = ?3",
            params![provider_id, kind.as_str(), subject_id],
        )
    })?;
    Ok(removed > 0)
}

/// Every provider a user may use, with its key opened.
///
/// The union of what is assigned to them and what is assigned to any group
/// they belong to. A provider reachable both ways appears once, because the
/// query is a `DISTINCT` over provider rows rather than over assignments.
pub fn entitled_for_user(
    store: &Store,
    sealer: &Sealer,
    user_id: &str,
) -> anyhow::Result<Vec<EntitledProvider>> {
    /// One provider row as the table holds it, key still sealed.
    struct SealedRow {
        name: String,
        protocol: Option<String>,
        api_base: Option<String>,
        sealed_key: String,
        models_json: String,
    }

    let rows: Vec<SealedRow> = store.with(|conn| {
        let mut statement = conn.prepare(
            "SELECT DISTINCT p.name, p.protocol, p.api_base, p.api_key, p.models_json
             FROM providers p
             JOIN assignments a ON a.provider_id = p.id
             WHERE (a.subject_kind = 'user' AND a.subject_id = ?1)
                OR (a.subject_kind = 'group' AND a.subject_id IN (
                       SELECT group_id FROM memberships WHERE user_id = ?1))
             ORDER BY p.name",
        )?;
        let rows = statement
            .query_map(params![user_id], |row| {
                Ok(SealedRow {
                    name: row.get(0)?,
                    protocol: row.get(1)?,
                    api_base: row.get(2)?,
                    sealed_key: row.get(3)?,
                    models_json: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(EntitledProvider {
            name: row.name,
            protocol: row.protocol,
            api_base: row.api_base,
            api_key: sealer.open(&row.sealed_key)?,
            models: serde_json::from_str(&row.models_json).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Who a provider is assigned to, for the administration surface.
pub fn assignments_for_provider(
    store: &Store,
    provider_id: &str,
) -> anyhow::Result<Vec<(SubjectKind, String)>> {
    store.with(|conn| {
        let mut statement = conn
            .prepare("SELECT subject_kind, subject_id FROM assignments WHERE provider_id = ?1")?;
        let rows = statement
            .query_map(params![provider_id], |row| {
                let kind: String = row.get(0)?;
                Ok((
                    if kind == "group" {
                        SubjectKind::Group
                    } else {
                        SubjectKind::User
                    },
                    row.get::<_, String>(1)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

/// Whether a provider id exists.
pub fn provider_exists(store: &Store, id: &str) -> anyhow::Result<bool> {
    let found: Option<String> = store.with(|conn| {
        conn.query_row(
            "SELECT id FROM providers WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
    })?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";
    const PASSWORD: &str = "correct horse battery";

    fn fixture() -> (Store, Sealer) {
        (Store::open_in_memory().expect("store"), Sealer::new(SECRET))
    }

    fn provider(name: &str) -> ProviderInput {
        ProviderInput {
            name: name.to_string(),
            protocol: Some("openai".to_string()),
            api_base: Some("https://api.example".to_string()),
            api_key: format!("key-for-{name}"),
            models: vec!["gpt-x".to_string()],
        }
    }

    #[test]
    fn a_provider_key_is_never_stored_in_the_clear() {
        let (store, sealer) = fixture();
        create_provider(&store, &sealer, &provider("openai")).expect("created");

        let stored: String = store
            .with(|conn| conn.query_row("SELECT api_key FROM providers", [], |row| row.get(0)))
            .expect("query");
        assert!(!stored.contains("key-for-openai"), "the key is readable");
        assert_eq!(sealer.open(&stored).expect("opened"), "key-for-openai");
    }

    #[test]
    fn listing_providers_never_answers_with_a_key() {
        let (store, sealer) = fixture();
        create_provider(&store, &sealer, &provider("openai")).expect("created");

        let listed = list_providers(&store).expect("listed");
        let json = serde_json::to_string(&listed).expect("json");
        assert!(!json.contains("key-for-openai"), "a key leaked: {json}");
        assert_eq!(listed[0].name, "openai");
        assert_eq!(listed[0].models, vec!["gpt-x"]);
    }

    #[test]
    fn a_provider_needs_a_name_and_a_key() {
        let (store, sealer) = fixture();
        let mut nameless = provider("openai");
        nameless.name = "   ".to_string();
        assert!(create_provider(&store, &sealer, &nameless).is_err());

        let mut keyless = provider("openai");
        keyless.api_key = "  ".to_string();
        assert!(create_provider(&store, &sealer, &keyless).is_err());
    }

    #[test]
    fn a_direct_assignment_reaches_the_user() {
        let (store, sealer) = fixture();
        let user = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");

        assert!(entitled_for_user(&store, &sealer, &user)
            .expect("query")
            .is_empty());

        assign(&store, &id, SubjectKind::User, &user).expect("assigned");
        let entitled = entitled_for_user(&store, &sealer, &user).expect("query");
        assert_eq!(entitled.len(), 1);
        assert_eq!(entitled[0].api_key, "key-for-openai");
    }

    #[test]
    fn a_group_assignment_reaches_every_member() {
        let (store, sealer) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let bora = accounts::create_user(&store, "bora@firma.com", PASSWORD, false).expect("user");
        let backend = create_group(&store, "backend").expect("group");
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");

        assign(&store, &id, SubjectKind::Group, &backend).expect("assigned");
        add_membership(&store, &ayse, &backend).expect("member");

        assert_eq!(
            entitled_for_user(&store, &sealer, &ayse).expect("q").len(),
            1
        );
        assert!(entitled_for_user(&store, &sealer, &bora)
            .expect("q")
            .is_empty());
    }

    #[test]
    fn leaving_a_group_takes_its_providers_away() {
        let (store, sealer) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let backend = create_group(&store, "backend").expect("group");
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");
        assign(&store, &id, SubjectKind::Group, &backend).expect("assigned");
        add_membership(&store, &ayse, &backend).expect("member");

        assert!(remove_membership(&store, &ayse, &backend).expect("removed"));
        assert!(entitled_for_user(&store, &sealer, &ayse)
            .expect("q")
            .is_empty());
    }

    #[test]
    fn a_direct_assignment_survives_leaving_the_group() {
        // The two routes are independent; one must not cancel the other.
        let (store, sealer) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let backend = create_group(&store, "backend").expect("group");
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");

        assign(&store, &id, SubjectKind::Group, &backend).expect("assigned");
        assign(&store, &id, SubjectKind::User, &ayse).expect("assigned");
        add_membership(&store, &ayse, &backend).expect("member");
        remove_membership(&store, &ayse, &backend).expect("removed");

        assert_eq!(
            entitled_for_user(&store, &sealer, &ayse).expect("q").len(),
            1
        );
    }

    #[test]
    fn a_provider_reachable_by_two_routes_appears_once() {
        let (store, sealer) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let backend = create_group(&store, "backend").expect("group");
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");

        assign(&store, &id, SubjectKind::Group, &backend).expect("assigned");
        assign(&store, &id, SubjectKind::User, &ayse).expect("assigned");
        add_membership(&store, &ayse, &backend).expect("member");

        assert_eq!(
            entitled_for_user(&store, &sealer, &ayse).expect("q").len(),
            1
        );
    }

    #[test]
    fn unassigning_takes_the_provider_away() {
        let (store, sealer) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");
        assign(&store, &id, SubjectKind::User, &ayse).expect("assigned");

        assert!(unassign(&store, &id, SubjectKind::User, &ayse).expect("unassigned"));
        assert!(entitled_for_user(&store, &sealer, &ayse)
            .expect("q")
            .is_empty());
    }

    #[test]
    fn deleting_a_provider_takes_its_assignments_with_it() {
        let (store, sealer) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");
        assign(&store, &id, SubjectKind::User, &ayse).expect("assigned");

        assert!(delete_provider(&store, &id).expect("deleted"));
        let left: i64 = store
            .with(|conn| conn.query_row("SELECT COUNT(*) FROM assignments", [], |row| row.get(0)))
            .expect("query");
        assert_eq!(left, 0, "an assignment outlived its provider");
    }

    #[test]
    fn deleting_a_group_takes_its_memberships_with_it() {
        let (store, _) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let backend = create_group(&store, "backend").expect("group");
        add_membership(&store, &ayse, &backend).expect("member");

        assert!(delete_group(&store, &backend).expect("deleted"));
        assert!(groups_for_user(&store, &ayse).expect("q").is_empty());
    }

    #[test]
    fn two_groups_cannot_share_a_name() {
        let (store, _) = fixture();
        create_group(&store, "backend").expect("group");
        assert!(create_group(&store, "backend").is_err());
    }

    #[test]
    fn two_providers_cannot_share_a_name() {
        // The name becomes a key in the user's providers map, so a duplicate
        // would overwrite the other on the client.
        let (store, sealer) = fixture();
        create_provider(&store, &sealer, &provider("openai")).expect("created");
        assert!(create_provider(&store, &sealer, &provider("openai")).is_err());
    }

    #[test]
    fn assigning_twice_is_not_an_error() {
        let (store, sealer) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");

        assign(&store, &id, SubjectKind::User, &ayse).expect("assigned");
        assign(&store, &id, SubjectKind::User, &ayse).expect("assigned again");
        assert_eq!(assignments_for_provider(&store, &id).expect("q").len(), 1);
    }

    #[test]
    fn an_assignment_kind_outside_the_two_is_refused() {
        // The check constraint is what stops a typo becoming an assignment
        // that matches nothing and is never noticed.
        let (store, sealer) = fixture();
        let id = create_provider(&store, &sealer, &provider("openai")).expect("created");
        let written = store.with(|conn| {
            conn.execute(
                "INSERT INTO assignments (provider_id, subject_kind, subject_id)
                 VALUES (?1, 'team', 'x')",
                params![id],
            )
        });
        assert!(written.is_err(), "an unknown subject kind was stored");
    }

    #[test]
    fn deleting_a_user_takes_their_memberships_with_them() {
        let (store, _) = fixture();
        let ayse = accounts::create_user(&store, "ayse@firma.com", PASSWORD, false).expect("user");
        let backend = create_group(&store, "backend").expect("group");
        add_membership(&store, &ayse, &backend).expect("member");

        store
            .with(|conn| conn.execute("DELETE FROM users WHERE id = ?1", params![ayse]))
            .expect("deleted");
        let left: i64 = store
            .with(|conn| conn.query_row("SELECT COUNT(*) FROM memberships", [], |row| row.get(0)))
            .expect("query");
        assert_eq!(left, 0, "an orphaned membership outlived its user");
    }
}
