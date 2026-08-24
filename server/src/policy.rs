//! The settings policy an organisation pushes to every installation.
//!
//! One row. A client fetches it, caches it, and merges it over its own
//! settings, so whatever the policy names, the user cannot override.
//!
//! That is exactly why the policy may not name everything. A key that decides
//! what runs on the developer's machine is refused here, at the moment an
//! administrator writes it, rather than silently dropped by the client. An
//! administrator who is told "no" can ask why; one whose policy is ignored
//! believes it applied.

use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::accounts::now_secs;
use crate::store::Store;

/// The table this module owns. `CHECK (id = 1)` keeps it to one row.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS policy (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    blob       TEXT NOT NULL,
    checksum   TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
";

/// Keys a policy may never set, whether at the top level or inside `config`.
///
/// Every one of them names something the client would run, fetch or connect
/// to. A policy server that could set them would be a way to execute code on
/// every machine in the organisation, which is a larger power than "the
/// company decides the default model".
///
/// Both spellings of each key are listed, because `Settings` uses camelCase
/// while `Config` is snake_case on the wire, and a deny-list that knew only
/// one of them would let the other through.
pub const REFUSED_KEYS: &[&str] = &[
    "hooks",
    "mcpServers",
    "mcp_servers",
    "formatter",
    "lspServers",
    "lsp_servers",
    "skills",
    "acpAgents",
    "acp_agents",
    "remoteControl",
    "remote_control",
    "workspace",
];

/// The permission mode a policy may not choose.
///
/// An organisation may make permissions stricter. Turning them off for
/// everyone from a server is the one direction that cannot be undone by the
/// person sitting at the machine.
const REFUSED_PERMISSION_MODE: &str = "bypassPermissions";

/// A stored policy and the checksum that identifies this version of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub settings: Value,
    pub checksum: String,
}

/// Refuse a policy that names something the client would run.
pub fn check_allowed(settings: &Value) -> anyhow::Result<()> {
    let Some(map) = settings.as_object() else {
        anyhow::bail!("a policy has to be a JSON object");
    };

    let mut scopes = vec![map];
    if let Some(nested) = map.get("config").and_then(Value::as_object) {
        scopes.push(nested);
    }

    for scope in &scopes {
        for key in REFUSED_KEYS {
            if scope.contains_key(*key) {
                anyhow::bail!(
                    "a policy may not set `{key}`, because it names something the \
                     client would run, fetch or connect to"
                );
            }
        }
        for key in ["permissionMode", "permission_mode"] {
            if scope.get(key).and_then(Value::as_str) == Some(REFUSED_PERMISSION_MODE) {
                anyhow::bail!(
                    "a policy may not set `{key}` to `{REFUSED_PERMISSION_MODE}`; \
                     it may make permissions stricter, not turn them off"
                );
            }
        }
    }
    Ok(())
}

/// Compute the checksum that identifies a policy version.
///
/// Stable across key order, so a client is not told the policy changed when an
/// administrator's editor merely reordered the file. Nothing here sorts: with
/// `preserve_order` off, `serde_json` backs an object with a `BTreeMap`, so
/// keys come out sorted whatever order they went in. Turning that feature on,
/// here or in a dependency, would make this order-dependent, which is what
/// `key_order_does_not_change_the_checksum` catches.
pub fn checksum(settings: &Value) -> String {
    let canonical = serde_json::to_string(settings).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Write the policy, replacing whatever was there.
pub fn set(store: &Store, settings: &Value) -> anyhow::Result<String> {
    check_allowed(settings)?;
    let sum = checksum(settings);
    let blob = serde_json::to_string(settings)?;
    store.with(|conn| {
        conn.execute(
            "INSERT INTO policy (id, blob, checksum, updated_at) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET blob = ?1, checksum = ?2, updated_at = ?3",
            params![blob, sum, now_secs()],
        )
    })?;
    Ok(sum)
}

/// Read the policy. Answers `None` when none was ever written.
pub fn get(store: &Store) -> anyhow::Result<Option<Policy>> {
    let row: Option<(String, String)> = store.with(|conn| {
        conn.query_row(
            "SELECT blob, checksum FROM policy WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
    })?;

    let Some((blob, checksum)) = row else {
        return Ok(None);
    };
    Ok(Some(Policy {
        settings: serde_json::from_str(&blob)?,
        checksum,
    }))
}

/// Remove the policy.
pub fn clear(store: &Store) -> anyhow::Result<bool> {
    let removed = store.with(|conn| conn.execute("DELETE FROM policy WHERE id = 1", []))?;
    Ok(removed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> Store {
        Store::open_in_memory().expect("store")
    }

    #[test]
    fn nothing_is_stored_until_something_is_written() {
        assert!(get(&store()).expect("query").is_none());
    }

    #[test]
    fn a_policy_round_trips_with_its_checksum() {
        let store = store();
        let settings = json!({ "config": { "model": "claude-opus-4-6" } });
        let written = set(&store, &settings).expect("stored");

        let read = get(&store).expect("query").expect("stored");
        assert_eq!(read.settings, settings);
        assert_eq!(read.checksum, written);
    }

    #[test]
    fn writing_again_replaces_rather_than_adds() {
        let store = store();
        set(&store, &json!({ "a": 1 })).expect("stored");
        set(&store, &json!({ "a": 2 })).expect("stored");

        let rows: i64 = store
            .with(|conn| conn.query_row("SELECT COUNT(*) FROM policy", [], |row| row.get(0)))
            .expect("query");
        assert_eq!(rows, 1);
        assert_eq!(
            get(&store).expect("q").expect("stored").settings,
            json!({ "a": 2 })
        );
    }

    #[test]
    fn key_order_does_not_change_the_checksum() {
        // Otherwise every client would be told the policy changed whenever an
        // administrator's editor reordered the file.
        let first: Value =
            serde_json::from_str(r#"{"b": 1, "a": {"d": 2, "c": 3}}"#).expect("json");
        let second: Value =
            serde_json::from_str(r#"{"a": {"c": 3, "d": 2}, "b": 1}"#).expect("json");
        assert_eq!(checksum(&first), checksum(&second));
    }

    #[test]
    fn a_changed_value_changes_the_checksum() {
        assert_ne!(checksum(&json!({ "a": 1 })), checksum(&json!({ "a": 2 })));
    }

    #[test]
    fn array_order_still_matters() {
        // Sorting object keys must not reach into arrays: the order of an
        // array is part of its value.
        assert_ne!(
            checksum(&json!({ "a": [1, 2] })),
            checksum(&json!({ "a": [2, 1] }))
        );
    }

    #[test]
    fn every_refused_key_is_refused_at_the_top_level() {
        for key in REFUSED_KEYS {
            let settings = json!({ *key: {} });
            let error = check_allowed(&settings).expect_err("refused");
            assert!(
                error.to_string().contains(*key),
                "{key} was refused without naming itself: {error}"
            );
        }
    }

    #[test]
    fn every_refused_key_is_refused_inside_config() {
        // `Settings::effective_config` folds the nested block into the same
        // place, so a check that watched only the top level would miss it.
        for key in REFUSED_KEYS {
            let settings = json!({ "config": { *key: {} } });
            assert!(
                check_allowed(&settings).is_err(),
                "{key} passed inside `config`"
            );
        }
    }

    #[test]
    fn a_policy_may_not_turn_permissions_off() {
        for key in ["permissionMode", "permission_mode"] {
            let settings = json!({ key: "bypassPermissions" });
            let error = check_allowed(&settings).expect_err("refused");
            assert!(error.to_string().contains("stricter"), "{error}");

            let nested = json!({ "config": { key: "bypassPermissions" } });
            assert!(check_allowed(&nested).is_err(), "{key} passed in `config`");
        }
    }

    #[test]
    fn a_policy_may_make_permissions_stricter() {
        assert!(check_allowed(&json!({ "permissionMode": "plan" })).is_ok());
        assert!(check_allowed(&json!({ "config": { "permission_mode": "ask" } })).is_ok());
    }

    #[test]
    fn an_ordinary_policy_is_accepted() {
        let settings = json!({
            "config": { "model": "claude-opus-4-6", "auto_compact": true },
            "theme": "dark"
        });
        assert!(check_allowed(&settings).is_ok());
    }

    #[test]
    fn a_refused_policy_is_never_stored() {
        let store = store();
        assert!(set(&store, &json!({ "hooks": {} })).is_err());
        assert!(
            get(&store).expect("query").is_none(),
            "it was stored anyway"
        );
    }

    #[test]
    fn a_policy_that_is_not_an_object_is_refused() {
        for bad in [json!([]), json!("text"), json!(1), json!(null)] {
            assert!(check_allowed(&bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn clearing_removes_it() {
        let store = store();
        set(&store, &json!({ "a": 1 })).expect("stored");
        assert!(clear(&store).expect("cleared"));
        assert!(get(&store).expect("query").is_none());
        assert!(
            !clear(&store).expect("cleared"),
            "clearing twice reported a removal"
        );
    }

    #[test]
    fn a_second_row_cannot_be_written() {
        // The single-row constraint is what keeps `get` from having to pick.
        let store = store();
        set(&store, &json!({ "a": 1 })).expect("stored");
        let written = store.with(|conn| {
            conn.execute(
                "INSERT INTO policy (id, blob, checksum, updated_at) VALUES (2, '{}', 'x', 0)",
                [],
            )
        });
        assert!(written.is_err(), "a second policy row was stored");
    }
}
