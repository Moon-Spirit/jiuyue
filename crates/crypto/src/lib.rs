//! JiuYue client-side E2EE engine — thin Olm wrapper over [`vodozemac`].
//!
//! PRODUCTION-PATH NOTE (deliberate): this crate is the designated upgrade
//! path for real end-to-end encryption inside the CLIENTS (Tauri Rust core).
//! The M3 server deliberately does NOT depend on it — the server stays an
//! opaque relay/persist layer for ciphertexts and public key bundles. Until
//! the client milestone wires this in, most items are intentionally unused,
//! hence the crate-level `#![allow(dead_code)]`.
//!
//! Semantics follow `docs/research/2026-08-24-e2ee-crypto-landscape.md`:
//! X3DH-style asynchronous session establishment from (identity key,
//! one-time key), Double Ratchet sessions thereafter, and ENCRYPTED pickles
//! for at-rest persistence (vodozemac 0.10 exposes plain serde pickles, so
//! the sealing AES-256-GCM layer lives here).
//!
//! Wire conventions mirrored by `jiuyue_protocol::E2eeMsg`: message types
//! follow the libolm numbering (`0` = pre-key, `1` = normal) and all key /
//! message material travels as base64 strings.

#![allow(dead_code)]

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rand::RngCore;
use vodozemac::{
    Curve25519PublicKey,
    olm::{Account, InboundCreationResult, OlmMessage, SessionConfig},
};

/// libolm wire message type: pre-key messages establish a session.
pub const MESSAGE_TYPE_PRE_KEY: i32 = 0;
/// libolm wire message type: normal ratcheted messages.
pub const MESSAGE_TYPE_NORMAL: i32 = 1;

/// Errors surfaced by the wrapper. vodozemac's error types are flattened
/// into a display string — callers only branch on "bad input" vs "failed".
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("malformed base64 key or message material")]
    BadMaterial,
    #[error("crypto operation failed: {0}")]
    Operation(String),
}

impl CryptoError {
    fn op<E: std::fmt::Display>(err: E) -> Self {
        Self::Operation(err.to_string())
    }
}

/// One generated one-time key, ready for upload to the key server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneTimeKey {
    pub key_id: String,
    pub public_key: String,
}

/// An Olm account: the long-lived cryptographic identity of one device.
pub struct OlmAccount {
    inner: Account,
}

impl Default for OlmAccount {
    fn default() -> Self {
        Self::new()
    }
}

impl OlmAccount {
    /// Fresh account with random identity keys.
    pub fn new() -> Self {
        Self {
            inner: Account::new(),
        }
    }

    /// Base64 Curve25519 identity key — the "IK" published in key bundles.
    pub fn identity_key(&self) -> String {
        self.inner.curve25519_key().to_base64()
    }

    /// Base64 Ed25519 fingerprint key — safety-number material.
    pub fn fingerprint(&self) -> String {
        self.inner.ed25519_key().to_base64()
    }

    /// Generates `count` one-time keys, marks them published locally and
    /// returns their public halves for upload.
    pub fn generate_one_time_keys(&mut self, count: usize) -> Vec<OneTimeKey> {
        self.inner.generate_one_time_keys(count);
        let keys = self
            .inner
            .one_time_keys()
            .into_iter()
            .map(|(key_id, public)| OneTimeKey {
                key_id: format!("{key_id:?}"),
                public_key: public.to_base64(),
            })
            .collect();
        self.inner.mark_keys_as_published();
        keys
    }

    /// Sealed pickle for at-rest persistence: AES-256-GCM over the serde
    /// serialization of vodozemac's `AccountPickle`, keyed by `pickle_key`;
    /// output is `base64(nonce || ciphertext)`.
    pub fn pickle(&self, pickle_key: &[u8; 32]) -> Result<String, CryptoError> {
        let serialized = serde_json::to_vec(&self.inner.pickle()).map_err(CryptoError::op)?;
        seal(pickle_key, &serialized)
    }

    /// Restores an account previously sealed by [`OlmAccount::pickle`].
    pub fn unpickle(pickle_key: &[u8; 32], blob: &str) -> Result<Self, CryptoError> {
        let serialized = open(pickle_key, blob)?;
        let pickle = serde_json::from_slice(&serialized).map_err(CryptoError::op)?;
        Ok(Self {
            inner: Account::from_pickle(pickle),
        })
    }

    /// Establishes an outbound session toward `their_identity_key` using one
    /// of their published one-time keys (both base64).
    pub fn create_outbound_session(
        &self,
        their_identity_key: &str,
        their_one_time_key: &str,
    ) -> Result<OlmSession, CryptoError> {
        let identity = parse_curve_key(their_identity_key)?;
        let one_time_key = parse_curve_key(their_one_time_key)?;
        let session = self
            .inner
            .create_outbound_session(SessionConfig::version_1(), identity, one_time_key)
            .map_err(CryptoError::op)?;
        Ok(OlmSession { inner: session })
    }

    /// Consumes an inbound PRE-KEY message: creates the matching session AND
    /// decrypts the payload in one step. Returns `(session, plaintext)`.
    pub fn create_inbound_and_decrypt(
        &mut self,
        their_identity_key: &str,
        message_type: i32,
        ciphertext: &str,
    ) -> Result<(OlmSession, String), CryptoError> {
        let identity = parse_curve_key(their_identity_key)?;
        let message = olm_message_from_parts(message_type, ciphertext)?;
        let OlmMessage::PreKey(pre_key) = &message else {
            return Err(CryptoError::Operation(
                "inbound session creation requires a pre-key message (type 0)".to_owned(),
            ));
        };
        let result: InboundCreationResult = self
            .inner
            .create_inbound_session(SessionConfig::version_1(), identity, pre_key)
            .map_err(CryptoError::op)?;
        let plaintext = utf8(result.plaintext)?;
        Ok((
            OlmSession {
                inner: result.session,
            },
            plaintext,
        ))
    }
}

/// One end of an established Olm (Double Ratchet) channel.
pub struct OlmSession {
    inner: vodozemac::olm::Session,
}

impl OlmSession {
    /// Encrypts a plaintext; returns `(libolm message type, base64 ciphertext)`.
    pub fn encrypt(&mut self, plaintext: &str) -> Result<(i32, String), CryptoError> {
        let message = self.inner.encrypt(plaintext).map_err(CryptoError::op)?;
        let (message_type, ciphertext) = message.to_parts();
        let message_type = i32::try_from(message_type).map_err(|_| CryptoError::BadMaterial)?;
        Ok((message_type, BASE64.encode(ciphertext)))
    }

    /// Decrypts `(message_type, base64 ciphertext)` into a UTF-8 plaintext.
    pub fn decrypt(&mut self, message_type: i32, ciphertext: &str) -> Result<String, CryptoError> {
        let message = olm_message_from_parts(message_type, ciphertext)?;
        utf8(self.inner.decrypt(&message).map_err(CryptoError::op)?)
    }
}

fn parse_curve_key(raw: &str) -> Result<Curve25519PublicKey, CryptoError> {
    Curve25519PublicKey::from_base64(raw).map_err(CryptoError::op)
}

fn olm_message_from_parts(message_type: i32, ciphertext: &str) -> Result<OlmMessage, CryptoError> {
    if !(MESSAGE_TYPE_PRE_KEY..=MESSAGE_TYPE_NORMAL).contains(&message_type) {
        return Err(CryptoError::Operation(format!(
            "unknown olm message type {message_type}"
        )));
    }
    let raw = BASE64
        .decode(ciphertext)
        .map_err(|_| CryptoError::BadMaterial)?;
    OlmMessage::from_parts(
        usize::try_from(message_type).map_err(|_| CryptoError::BadMaterial)?,
        &raw,
    )
    .map_err(|_| CryptoError::BadMaterial)
}

fn utf8(bytes: Vec<u8>) -> Result<String, CryptoError> {
    String::from_utf8(bytes)
        .map_err(|_| CryptoError::Operation("decrypted plaintext is not UTF-8".to_owned()))
}

// --- AES-256-GCM sealing helpers (encrypted pickle persistence) -------------

fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<String, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(CryptoError::op)?;
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .map_err(CryptoError::op)?;
    let mut blob = Vec::with_capacity(12 + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(BASE64.encode(blob))
}

fn open(key: &[u8; 32], blob: &str) -> Result<Vec<u8>, CryptoError> {
    let raw = BASE64.decode(blob).map_err(|_| CryptoError::BadMaterial)?;
    if raw.len() < 13 {
        return Err(CryptoError::BadMaterial);
    }
    let (nonce_bytes, ciphertext) = raw.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(CryptoError::op)?;
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(CryptoError::op)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];

    #[test]
    fn fresh_accounts_have_distinct_identity_and_fingerprint() {
        let a = OlmAccount::new();
        let b = OlmAccount::new();
        assert_ne!(a.identity_key(), b.identity_key());
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn encrypted_pickle_roundtrip_restores_the_same_identity() {
        let account = OlmAccount::new();
        let blob = account.pickle(&KEY).expect("pickle succeeds");
        assert_ne!(
            blob,
            account.identity_key(),
            "the blob is sealed, never bare key material"
        );
        let restored = OlmAccount::unpickle(&KEY, &blob).expect("unpickle succeeds");
        assert_eq!(restored.identity_key(), account.identity_key());
        assert_eq!(restored.fingerprint(), account.fingerprint());
    }

    #[test]
    fn unpickling_with_the_wrong_key_fails_cleanly() {
        let account = OlmAccount::new();
        let blob = account.pickle(&KEY).expect("pickle succeeds");
        assert!(OlmAccount::unpickle(&[8u8; 32], &blob).is_err());
        assert!(OlmAccount::unpickle(&KEY, "not-base64!").is_err());
    }

    #[test]
    fn one_time_keys_generate_with_public_halves_and_replenish() {
        let mut account = OlmAccount::new();
        let first = account.generate_one_time_keys(3);
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|k| !k.public_key.is_empty()));
        assert!(first.iter().all(|k| !k.key_id.is_empty()));

        let second = account.generate_one_time_keys(2);
        assert_eq!(second.len(), 2);
        for reused in &first {
            assert!(
                !second.iter().any(|k| k.public_key == reused.public_key),
                "regenerated keys must be fresh"
            );
        }
    }

    #[test]
    fn full_exchange_prekey_establishment_then_ratchet_both_directions() {
        let alice = OlmAccount::new();
        let mut bob = OlmAccount::new();

        let otks = bob.generate_one_time_keys(1);
        let bob_otk = otks[0].public_key.clone();

        let mut alice_session = alice
            .create_outbound_session(&bob.identity_key(), &bob_otk)
            .expect("outbound session");
        let (msg_type, ciphertext) = alice_session.encrypt("first secret").expect("encrypt");
        assert_eq!(
            msg_type, MESSAGE_TYPE_PRE_KEY,
            "a fresh outbound session sends pre-key messages"
        );

        let (mut bob_session, received) = bob
            .create_inbound_and_decrypt(&alice.identity_key(), msg_type, &ciphertext)
            .expect("inbound establishment");
        assert_eq!(received, "first secret");

        // Bob replies; his session was established inbound, so replies are
        // normal ratchet messages alice can decrypt on the same session.
        let (reply_type, reply_ciphertext) = bob_session.encrypt("reply").expect("reply encrypt");
        assert_eq!(reply_type, MESSAGE_TYPE_NORMAL);
        assert_eq!(
            alice_session
                .decrypt(reply_type, &reply_ciphertext)
                .expect("decrypt reply"),
            "reply"
        );

        // The ratchet keeps turning in both directions.
        let (second_type, second_ciphertext) =
            alice_session.encrypt("second").expect("second encrypt");
        assert_eq!(
            bob_session
                .decrypt(second_type, &second_ciphertext)
                .expect("decrypt second"),
            "second"
        );
    }

    #[test]
    fn garbage_material_is_rejected_without_panicking() {
        let alice = OlmAccount::new();
        let mut bob = OlmAccount::new();
        let otks = bob.generate_one_time_keys(1);
        let mut session = alice
            .create_outbound_session(&bob.identity_key(), &otks[0].public_key)
            .expect("outbound session");

        assert!(
            session
                .decrypt(MESSAGE_TYPE_NORMAL, "not-a-message")
                .is_err()
        );
        assert!(session.decrypt(99, "whatever").is_err());

        let mut stranger = OlmAccount::new();
        assert!(
            stranger
                .create_inbound_and_decrypt(&alice.identity_key(), MESSAGE_TYPE_PRE_KEY, "junk")
                .is_err()
        );
        assert!(
            alice
                .create_outbound_session(&bob.identity_key(), "not-a-key")
                .is_err()
        );
    }
}
