//! Encryption for the values the database holds in the clear nowhere.
//!
//! Provider keys and settings backups are sealed with a key derived from
//! `MIKMIK_SERVER_SECRET`. The server can read them, which is the decision the
//! organisation made; what this buys is that a copied `.sqlite` file, a stray
//! backup or a disk image is not enough on its own.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

/// Marks the format, so a later change can be told apart from this one.
const PREFIX: &str = "v1";

/// Nonce width for XChaCha20-Poly1305.
///
/// 24 bytes is wide enough that drawing one at random per record has no
/// meaningful chance of repeating, so nothing has to remember which nonces
/// were already spent.
const NONCE_BYTES: usize = 24;

/// The sealing key.
pub struct Sealer {
    cipher: XChaCha20Poly1305,
}

impl Sealer {
    /// Derive the key from the configured secret.
    ///
    /// SHA-256 rather than a password KDF: the secret is 32 characters or more
    /// of operator-generated material, not a human-chosen password, so
    /// stretching it would only cost start-up time.
    pub fn new(secret: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"mikmik-server-at-rest-v1");
        hasher.update(secret.as_bytes());
        let key = hasher.finalize();
        Self {
            cipher: XChaCha20Poly1305::new(&key),
        }
    }

    /// Seal a value for storage.
    pub fn seal(&self, plaintext: &str) -> anyhow::Result<String> {
        let mut nonce_bytes = [0u8; NONCE_BYTES];
        getrandom::getrandom(&mut nonce_bytes)
            .map_err(|e| anyhow::anyhow!("the OS random number generator failed: {e}"))?;
        let nonce = XNonce::from_slice(&nonce_bytes);

        let sealed = self
            .cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("sealing the value failed"))?;

        Ok(format!(
            "{PREFIX}:{}:{}",
            hex::encode(nonce_bytes),
            hex::encode(sealed)
        ))
    }

    /// Open a stored value.
    ///
    /// Fails on a wrong key, a truncated record or a tampered one, because the
    /// authentication tag covers the whole ciphertext.
    pub fn open(&self, stored: &str) -> anyhow::Result<String> {
        let mut parts = stored.splitn(3, ':');
        let version = parts.next().unwrap_or_default();
        if version != PREFIX {
            anyhow::bail!("stored value is not in the {PREFIX} format");
        }
        let nonce_hex = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("stored value has no nonce"))?;
        let body_hex = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("stored value has no body"))?;

        let nonce_bytes = hex::decode(nonce_hex)?;
        if nonce_bytes.len() != NONCE_BYTES {
            anyhow::bail!("stored value has a {}-byte nonce", nonce_bytes.len());
        }
        let body = hex::decode(body_hex)?;

        let opened = self
            .cipher
            .decrypt(XNonce::from_slice(&nonce_bytes), body.as_slice())
            .map_err(|_| {
                anyhow::anyhow!("opening the value failed; the secret may have changed")
            })?;
        Ok(String::from_utf8(opened)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_value_round_trips() {
        let sealer = Sealer::new(SECRET);
        let sealed = sealer.seal("sk-not-a-real-key").expect("sealed");
        assert_eq!(sealer.open(&sealed).expect("opened"), "sk-not-a-real-key");
    }

    #[test]
    fn the_sealed_form_does_not_contain_the_value() {
        let sealer = Sealer::new(SECRET);
        let sealed = sealer.seal("sk-not-a-real-key").expect("sealed");
        assert!(!sealed.contains("sk-not-a-real-key"));
        assert!(sealed.starts_with("v1:"));
    }

    #[test]
    fn sealing_the_same_value_twice_gives_two_records() {
        // A fixed nonce would let two equal keys be recognised as equal in the
        // database without opening either.
        let sealer = Sealer::new(SECRET);
        let first = sealer.seal("the same value").expect("sealed");
        let second = sealer.seal("the same value").expect("sealed");
        assert_ne!(first, second);
    }

    #[test]
    fn another_secret_cannot_open_it() {
        let sealed = Sealer::new(SECRET)
            .seal("sk-not-a-real-key")
            .expect("sealed");
        let other = Sealer::new("fedcba9876543210fedcba9876543210");
        assert!(other.open(&sealed).is_err());
    }

    #[test]
    fn a_tampered_record_is_refused() {
        let sealer = Sealer::new(SECRET);
        let sealed = sealer.seal("sk-not-a-real-key").expect("sealed");

        // Flip the last hex digit of the body.
        let mut bytes: Vec<char> = sealed.chars().collect();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == '0' { '1' } else { '0' };
        let tampered: String = bytes.into_iter().collect();

        assert!(sealer.open(&tampered).is_err(), "a forged record opened");
    }

    #[test]
    fn a_malformed_record_is_refused_rather_than_panicking() {
        let sealer = Sealer::new(SECRET);
        for bad in [
            "",
            "v1",
            "v1:",
            "v1::",
            "v2:aa:bb",
            "v1:zz:bb",
            "plain text",
        ] {
            assert!(sealer.open(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_short_nonce_is_refused() {
        let sealer = Sealer::new(SECRET);
        assert!(sealer.open("v1:00112233:aabb").is_err());
    }

    #[test]
    fn an_empty_value_round_trips() {
        let sealer = Sealer::new(SECRET);
        let sealed = sealer.seal("").expect("sealed");
        assert_eq!(sealer.open(&sealed).expect("opened"), "");
    }
}
