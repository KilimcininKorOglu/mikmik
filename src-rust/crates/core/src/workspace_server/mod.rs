//! The organisation's configuration server, as this installation sees it.
//!
//! An organisation runs one server. This installation logs in to it, receives
//! the providers that account is entitled to and the settings policy the
//! organisation enforces, and keeps its own settings backed up there so a
//! rebuilt machine comes back with everything on it.
//!
//! The address lives in `settings.json` under `workspace`, and only ever in
//! the user's global file. The session token lives in `auth.json`, which is
//! written `0o600`.
//!
//! Named `workspace_server` rather than `workspace`, which is already the
//! named directory roots a session can reach.

pub mod client;
pub mod policy;
pub mod providers;
pub mod session;
pub mod sync;

pub use client::{
    Account, BackupWrite, EntitledProvider, Group, Identity, PolicyFetch, Session, StoredBackup,
    WorkspaceClient, WorkspaceError,
};
