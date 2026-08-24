//! What every handler shares.

use crate::store::Store;

pub struct AppState {
    pub store: Store,
    /// How long a session lives, in seconds.
    pub session_ttl_secs: i64,
}

impl AppState {
    pub fn new(store: Store, session_ttl_secs: i64) -> Self {
        Self {
            store,
            session_ttl_secs,
        }
    }
}
