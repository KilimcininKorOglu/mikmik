//! What every handler shares.

use crate::crypt::Sealer;
use crate::store::Store;

pub struct AppState {
    pub store: Store,
    /// Derives the CSRF token that goes with a session.
    pub secret: String,
    /// Opens and seals the values the database holds encrypted.
    pub sealer: Sealer,
    /// How long a session lives, in seconds.
    pub session_ttl_secs: i64,
}

impl AppState {
    pub fn new(store: Store, secret: &str, session_ttl_secs: i64) -> Self {
        Self {
            store,
            secret: secret.to_string(),
            sealer: Sealer::new(secret),
            session_ttl_secs,
        }
    }
}
