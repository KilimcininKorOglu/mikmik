//! Runtime configuration, all from the environment so the container needs no
//! config file.

use std::path::PathBuf;

use tracing::warn;

/// Shortest secret the server will start with.
///
/// This value is not a password to guess at a login prompt. It is the key the
/// stored provider keys and settings blobs are encrypted with, and the key the
/// session tokens are derived from, so a weak one means a stolen database file
/// is readable and a session token is forgeable.
pub const MIN_SECRET_LEN: usize = 32;

/// Why a configured secret was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    Missing,
    TooShort { len: usize },
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "no server secret configured; set MIKMIK_SERVER_SECRET to at least \
                 {MIN_SECRET_LEN} characters"
            ),
            Self::TooShort { len } => write!(
                f,
                "server secret is {len} characters; at least {MIN_SECRET_LEN} are required, \
                 because this secret encrypts every stored API key and derives every \
                 session token"
            ),
        }
    }
}

impl std::error::Error for SecretError {}

/// Accept a secret only if it is long enough to be worth having.
pub fn validate_secret(secret: &str) -> Result<&str, SecretError> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(SecretError::Missing);
    }
    let len = secret.chars().count();
    if len < MIN_SECRET_LEN {
        return Err(SecretError::TooShort { len });
    }
    Ok(secret)
}

/// Everything the process needs to start.
pub struct Config {
    /// Validated at startup so a weak secret stops the process before it
    /// listens. Nothing reads the value until sessions and encryption arrive.
    #[allow(dead_code)]
    pub secret: String,
    pub bind: String,
    pub db_path: PathBuf,
    /// How long a session lives, in seconds.
    pub session_ttl_secs: i64,
}

/// Default session lifetime: 30 days.
///
/// Long enough that a developer is not logging in every morning, short enough
/// that an abandoned laptop stops reaching the server within a month.
pub const DEFAULT_SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Default listen address.
///
/// Port 8420 is this project's reserved block. The container publishes it on
/// loopback, because the server does not terminate TLS.
pub const DEFAULT_BIND: &str = "0.0.0.0:8420";

/// Default database location inside the container's writable volume.
pub const DEFAULT_DB: &str = "mikmik-server.sqlite";

pub fn bind_from_env() -> String {
    std::env::var("MIKMIK_SERVER_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string())
}

impl Config {
    /// Read the environment, refusing to build on a weak secret.
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var("MIKMIK_SERVER_SECRET").unwrap_or_default();
        let secret = validate_secret(&raw)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .to_string();

        let db_path = match std::env::var("MIKMIK_SERVER_DB") {
            Ok(raw) if !raw.trim().is_empty() => PathBuf::from(raw.trim()),
            _ => PathBuf::from(DEFAULT_DB),
        };

        Ok(Self {
            secret,
            bind: bind_from_env(),
            db_path,
            session_ttl_secs: env_positive_i64(
                "MIKMIK_SERVER_SESSION_TTL_SECS",
                DEFAULT_SESSION_TTL_SECS,
            ),
        })
    }
}

/// Read a positive integer from the environment, warning rather than failing
/// on nonsense, because a mistyped lifetime should not stop the server.
pub fn env_positive_i64(name: &str, default: i64) -> i64 {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().parse::<i64>() {
            Ok(value) if value > 0 => value,
            _ => {
                warn!(name, value = %raw, "ignoring unparseable duration; using the default");
                default
            }
        },
        Err(_) => default,
    }
}

/// Address a health check should dial.
///
/// The bind address may be a wildcard, which is not connectable, so the host
/// part is rewritten to loopback while the port is kept.
pub fn health_check_target(bind: &str) -> String {
    match bind.rsplit_once(':') {
        Some((host, port)) if host.is_empty() || host == "0.0.0.0" || host == "[::]" => {
            format!("127.0.0.1:{port}")
        }
        _ => bind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_secret_is_rejected() {
        assert_eq!(validate_secret(""), Err(SecretError::Missing));
        assert_eq!(validate_secret("   "), Err(SecretError::Missing));
    }

    #[test]
    fn a_short_secret_is_rejected_with_its_length() {
        assert_eq!(
            validate_secret("hunter2"),
            Err(SecretError::TooShort { len: 7 })
        );
    }

    #[test]
    fn a_secret_of_exactly_the_minimum_is_accepted() {
        let secret = "a".repeat(MIN_SECRET_LEN);
        assert_eq!(validate_secret(&secret), Ok(secret.as_str()));
    }

    #[test]
    fn the_rejection_message_says_what_the_secret_protects() {
        let message = SecretError::TooShort { len: 4 }.to_string();
        assert!(
            message.contains("encrypts every stored API key"),
            "the operator has to understand the stake, got: {message}"
        );
    }

    #[test]
    fn a_wildcard_bind_is_dialled_on_loopback() {
        assert_eq!(health_check_target("0.0.0.0:8420"), "127.0.0.1:8420");
        assert_eq!(health_check_target("[::]:8420"), "127.0.0.1:8420");
        assert_eq!(health_check_target(":8420"), "127.0.0.1:8420");
    }

    #[test]
    fn a_concrete_bind_is_dialled_as_given() {
        assert_eq!(health_check_target("127.0.0.1:9000"), "127.0.0.1:9000");
        assert_eq!(health_check_target("10.0.0.5:8420"), "10.0.0.5:8420");
    }
}
