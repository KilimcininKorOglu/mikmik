//! Passwords, session tokens and the cookie they travel in.
//!
//! Two different one-way functions, for two different inputs. A password is
//! low-entropy and human-chosen, so it goes through argon2, which is slow on
//! purpose. A session token is 32 bytes from the OS, so guessing it is already
//! out of reach and a plain SHA-256 is enough to keep the stored form from
//! being usable.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Shortest password an account may be created with.
///
/// argon2 makes a guess expensive, not free, so the floor is about keeping a
/// four-character password out of the database rather than about entropy
/// arithmetic.
pub const MIN_PASSWORD_LEN: usize = 12;

/// Why a password was refused at account creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordError {
    TooShort { len: usize },
}

impl std::fmt::Display for PasswordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort { len } => write!(
                f,
                "password is {len} characters; at least {MIN_PASSWORD_LEN} are required"
            ),
        }
    }
}

impl std::error::Error for PasswordError {}

/// Hash a password for storage. Rejects one too short to be worth hashing.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        anyhow::bail!(PasswordError::TooShort { len });
    }
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing the password failed: {e}"))?;
    Ok(hash.to_string())
}

/// Check a password against a stored hash.
///
/// A malformed stored hash answers `false` rather than an error: the caller is
/// a login handler, and every failure there has to look the same from outside.
pub fn verify_password(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// A stored hash of a password nobody has, for the unknown-account path.
///
/// Verifying against this costs the same as verifying a real one, so the time
/// a login takes does not say whether the address exists.
pub fn decoy_hash() -> &'static str {
    // Generated once with argon2's defaults over 32 bytes from the OS, and
    // that input was discarded. It is not a secret: its only job is to be a
    // well-formed hash that no password matches.
    "$argon2id$v=19$m=19456,t=2,p=1$9PB5L3APewmLAW1mQD1Lig$PjuJDl0cUvIYx2+GoVtaggrozXSEJ0NWBWMLRznTeYo"
}

/// Length of a generated session token, in bytes before hex encoding.
const TOKEN_BYTES: usize = 32;

/// Draw a new session token from the OS.
///
/// From `getrandom` rather than a UUID: a v4 UUID fixes its version nibble and
/// two variant bits, so it carries less entropy than its length suggests. A
/// failure is returned rather than filled in from the clock or the pid,
/// because a predictable token reads as a working login while giving away
/// everything it protects.
pub fn new_session_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("the OS random number generator failed: {e}"))?;
    Ok(hex::encode(bytes))
}

/// The form of a session token that is stored.
///
/// A database file on its own then hands over no live session.
pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// The CSRF token that goes with a session.
///
/// Derived rather than stored: it is a keyed digest of the session token's
/// own digest, so it needs no table and cannot be produced by anyone who does
/// not already hold both the session and the server secret.
///
/// The session cookie is `HttpOnly`, so a page script cannot read it and
/// cannot forge this. A cross-site request carries the cookie only if the
/// browser ignores `SameSite=Strict`; this is the second lock on that door.
pub fn csrf_token(secret: &str, session_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mikmik-server-csrf-v1");
    hasher.update(secret.as_bytes());
    hasher.update(token_hash(session_token).as_bytes());
    hex::encode(hasher.finalize())
}

/// Name of the header the web interface sends its CSRF token in.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Compare two hex digests in constant time.
///
/// A short-circuiting `==` leaks the length of the matching prefix through
/// timing, which turns a search into a per-character one.
pub fn digest_matches(stored: &str, presented: &str) -> bool {
    let stored = stored.as_bytes();
    let presented = presented.as_bytes();
    if stored.len() != presented.len() {
        // Still run a comparison so the early return does not itself leak the
        // length, then discard the result.
        let _ = stored.ct_eq(stored);
        return false;
    }
    stored.ct_eq(presented).into()
}

/// Pull the bearer token out of an `Authorization` header value.
pub fn bearer_from_header(value: &str) -> Option<&str> {
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

/// Name of the cookie the web interface authenticates with.
pub const COOKIE_NAME: &str = "mikmik_session";

/// Pull the token out of a `Cookie` header value.
pub fn token_from_cookies(header: &str) -> Option<&str> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == COOKIE_NAME)
            .then(|| value.trim())
            .filter(|value| !value.is_empty())
    })
}

/// Whether the request reached the server over TLS.
///
/// The server never terminates TLS itself, so this is entirely what the
/// reverse proxy in front reports. The first entry wins, because a proxy chain
/// appends rather than replaces.
pub fn is_secure_request(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
}

/// Cookie attributes for the authenticated session.
///
/// `HttpOnly` keeps page scripts from reading the token back out, and
/// `SameSite=Strict` stops another site from driving the server through the
/// browser's ambient credentials.
///
/// `Secure` is added only when the request arrived over TLS. Setting it
/// unconditionally would stop the cookie being sent at all on a plain-HTTP LAN
/// deployment, and omitting it behind a proxy would let the token travel over
/// a plaintext downgrade.
pub fn session_cookie(token: &str, secure: bool, max_age_secs: i64) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age_secs}{secure}"
    )
}

/// The cookie that clears the session.
pub fn cleared_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{secure}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trips() {
        let hash = hash_password("correct horse battery").expect("hashed");
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("Correct horse battery", &hash));
        assert!(!verify_password("", &hash));
    }

    #[test]
    fn a_short_password_is_refused_with_its_length() {
        let error = hash_password("hunter2").expect_err("refused");
        assert!(
            error.to_string().contains("7 characters"),
            "the caller has to learn why, got: {error}"
        );
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        // A shared salt would let one rainbow table cover every account.
        let first = hash_password("correct horse battery").expect("hashed");
        let second = hash_password("correct horse battery").expect("hashed");
        assert_ne!(first, second);
    }

    #[test]
    fn a_malformed_stored_hash_answers_false() {
        assert!(!verify_password("anything", "not-a-hash"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn the_decoy_hash_parses_and_never_matches() {
        // It has to parse, or the unknown-account path would skip the argon2
        // work and answer faster than a real failure.
        assert!(PasswordHash::new(decoy_hash()).is_ok());
        assert!(!verify_password("correct horse battery", decoy_hash()));
    }

    #[test]
    fn two_tokens_differ_and_are_full_length() {
        let first = new_session_token().expect("token");
        let second = new_session_token().expect("token");
        assert_ne!(first, second);
        assert_eq!(first.len(), TOKEN_BYTES * 2, "hex of 32 bytes");
    }

    #[test]
    fn hashing_a_token_hides_it() {
        let token = new_session_token().expect("token");
        let hash = token_hash(&token);
        assert_ne!(hash, token);
        assert_eq!(hash, token_hash(&token), "hashing is stable");
    }

    #[test]
    fn digest_comparison_is_exact() {
        let hash = token_hash("abc");
        assert!(digest_matches(&hash, &hash));
        assert!(!digest_matches(&hash, &hash[..hash.len() - 1]));
        assert!(!digest_matches(&hash, &format!("{hash}0")));
        assert!(!digest_matches(&hash, ""));
    }

    #[test]
    fn a_csrf_token_is_bound_to_its_session_and_to_the_secret() {
        let one = new_session_token().expect("token");
        let two = new_session_token().expect("token");

        assert_eq!(csrf_token("secret-a", &one), csrf_token("secret-a", &one));
        assert_ne!(csrf_token("secret-a", &one), csrf_token("secret-a", &two));
        assert_ne!(csrf_token("secret-a", &one), csrf_token("secret-b", &one));
    }

    #[test]
    fn a_csrf_token_is_not_the_session_token() {
        let token = new_session_token().expect("token");
        let csrf = csrf_token("secret", &token);
        assert_ne!(csrf, token);
        assert_ne!(csrf, token_hash(&token));
    }

    #[test]
    fn a_bearer_header_yields_the_token() {
        assert_eq!(bearer_from_header("Bearer abc"), Some("abc"));
        assert_eq!(bearer_from_header("bearer abc"), Some("abc"));
        assert_eq!(bearer_from_header("Bearer  abc  "), Some("abc"));
        assert_eq!(bearer_from_header("Bearer "), None);
        assert_eq!(bearer_from_header("Basic abc"), None);
    }

    #[test]
    fn the_named_cookie_is_the_one_read() {
        assert_eq!(
            token_from_cookies(&format!("other=1; {COOKIE_NAME}=abc; more=2")),
            Some("abc")
        );
        assert_eq!(token_from_cookies("other=1"), None);
        assert_eq!(token_from_cookies(&format!("{COOKIE_NAME}=")), None);
    }

    #[test]
    fn the_cookie_is_locked_down() {
        let cookie = session_cookie("abc", false, 60);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=60"));
    }

    #[test]
    fn the_cookie_is_marked_secure_only_behind_tls() {
        assert!(!session_cookie("abc", false, 60).contains("Secure"));
        assert!(session_cookie("abc", true, 60).contains("; Secure"));
        assert!(!cleared_cookie(false).contains("Secure"));
        assert!(cleared_cookie(true).contains("; Secure"));
    }

    #[test]
    fn the_clearing_cookie_expires_immediately() {
        assert!(cleared_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn tls_is_detected_from_the_first_proxy_in_the_chain() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!is_secure_request(&headers));

        headers.insert("x-forwarded-proto", "https".parse().expect("valid header"));
        assert!(is_secure_request(&headers));

        headers.insert(
            "x-forwarded-proto",
            "http, https".parse().expect("valid header"),
        );
        assert!(!is_secure_request(&headers));
    }
}
