//! At-rest body encryption: AES-256-GCM over message bodies.
//!
//! Key derivation (documented contract, ticket 05):
//!
//! ```text
//! key = SHA-256(UTF-8 bytes of JIUYUE_MASTER_KEY)   // 32 bytes → AES-256
//! ```
//!
//! `body_enc` layout stored in Postgres:
//!
//! ```text
//! [ 12-byte random nonce || AES-256-GCM ciphertext(+16-byte tag) ]
//! ```
//!
//! A fresh random nonce is drawn per encryption; the nonce is *prepended* to
//! the ciphertext so decryption is self-contained. `key_id = "v1"` tags this
//! scheme in the messages table so keys can rotate later without re-reading
//! old rows.
//!
//! Secrecy discipline: plaintext bodies and key material are never logged —
//! error paths surface only the operation that failed.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, anyhow};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Nonce length in bytes required by AES-256-GCM.
const NONCE_LEN: usize = 12;

/// Key-generation tag written to `messages.key_id` for this cipher scheme.
pub const KEY_ID_V1: &str = "v1";

/// AES-256-GCM cipher derived from the deployment master key.
#[derive(Clone)]
pub struct BodyCipher {
    cipher: Aes256Gcm,
}

impl BodyCipher {
    /// Derives the cipher from raw master-key material (`JIUYUE_MASTER_KEY`).
    ///
    /// Infallible by construction: SHA-256 always yields exactly the 32-byte
    /// key AES-256 requires.
    pub fn new(master_key_material: &str) -> Self {
        let digest = Sha256::digest(master_key_material.as_bytes());
        // Invariant: a SHA-256 digest is exactly 32 bytes, so `from_slice`
        // cannot panic here.
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&digest));
        Self { cipher }
    }

    /// Encrypts `plaintext`, returning `nonce || ciphertext` for storage.
    pub fn encrypt(&self, plaintext: &str) -> anyhow::Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_bytes())
            .map_err(|err| anyhow!("body encryption failed: {err}"))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypts a `nonce || ciphertext` blob produced by [`Self::encrypt`].
    pub fn decrypt(&self, blob: &[u8]) -> anyhow::Result<String> {
        let (nonce_bytes, ciphertext) = blob
            .split_at_checked(NONCE_LEN)
            .ok_or_else(|| anyhow!("encrypted body shorter than nonce"))?;
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| anyhow!("body decryption failed"))?;
        String::from_utf8(plaintext).context("decrypted body is not valid UTF-8")
    }

    /// The key-generation tag persisted alongside encrypted bodies.
    pub fn key_id(&self) -> &'static str {
        KEY_ID_V1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_unicode_body() {
        let cipher = BodyCipher::new("test-master-key");
        let body = "在吗？ secret ✓";
        let blob = cipher.encrypt(body).expect("encrypt");
        assert_ne!(
            blob,
            body.as_bytes(),
            "ciphertext must differ from plaintext"
        );
        assert_eq!(cipher.decrypt(&blob).expect("decrypt"), body);
    }

    #[test]
    fn fresh_nonce_produces_distinct_ciphertexts() {
        let cipher = BodyCipher::new("test-master-key");
        let first = cipher.encrypt("same").expect("encrypt 1");
        let second = cipher.encrypt("same").expect("encrypt 2");
        assert_ne!(first, second, "random nonce must change the blob");
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let cipher = BodyCipher::new("test-master-key");
        let mut blob = cipher.encrypt("hello").expect("encrypt");
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        assert!(cipher.decrypt(&blob).is_err());
    }

    #[test]
    fn truncated_blob_is_rejected_without_panic() {
        let cipher = BodyCipher::new("test-master-key");
        assert!(cipher.decrypt(&[0u8; 5]).is_err());
    }
}
