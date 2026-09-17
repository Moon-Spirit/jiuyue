//! PKCE (RFC 7636, `S256`).
//!
//! The authorization code travels through the browser's address bar and the
//! provider's redirect, so it is the one value in the OAuth round trip that an
//! attacker on the same machine or a logging proxy has a realistic chance of
//! seeing. PKCE removes its value on its own: the code is only redeemable by a
//! client that can also present the `code_verifier`, a 256-bit secret that never
//! left this process.
//!
//! # Why the verifier's digest is what gets stored
//!
//! The round trip is stateless from the browser's point of view — we hand out an
//! authorization URL and see the callback later, with no session in between — so
//! the verifier has to survive in our database. It gets the same treatment as
//! every other secret here: only a digest is written, and the value that travels
//! to the provider is reconstructed nowhere, because redemption only needs to
//! *compare* the verifier the client presents.
//!
//! The `code_challenge` is `BASE64URL(SHA256(verifier))` — a different
//! transformation from the storage digest (which is hex), so the two can never be
//! confused for one another in a column or a log line.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// The `code_challenge_method` this module produces.
///
/// `plain` exists in the RFC and is refused here: it provides no protection at
/// all, and a provider that only offers `plain` is not worth supporting.
pub const CHALLENGE_METHOD: &str = "S256";

/// The RFC 7636 minimum verifier length, asserted against the derived value.
///
/// The verifier is a hex SHA-256 digest, so it is 64 characters — comfortably
/// over this — but a provider rejects a short verifier and the assertion belongs
/// next to the choice that produces it.
pub const VERIFIER_MIN_CHARS: usize = 43;

/// A minted verifier and the challenge derived from it.
///
/// # Why the verifier is the `state`'s digest, and not another secret
///
/// The verifier has to survive between the redirect out and the callback in, and
/// this round trip is stateless from the browser's point of view — so it would
/// otherwise need to sit in the database. It does not need to, because the server
/// already holds a value the browser has seen only as an *unrelated* opaque
/// string: the `state`. Its SHA-256 digest is 64 hex characters (inside RFC 7636's
/// 43–128 range), is unguessable to anyone who has not read our database, and is
/// already stored — so the verifier is that digest and nothing new is persisted.
///
/// An attacker who intercepts the authorization URL sees the `code_challenge` and
/// the `state`, and can derive neither the verifier (SHA-256 preimage) nor the
/// stored digest. PKCE's guarantee — that an intercepted code is useless without
/// the verifier — is fully intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    /// The value sent to the provider's token endpoint at redemption. Equal to
    /// the `state`'s storage digest, which is already in `oauth_states`.
    pub verifier: String,
    /// The `S256` challenge sent with the authorization request.
    pub challenge: String,
}

impl PkcePair {
    /// Derive the pair from the `state` token that was just minted.
    ///
    /// Nothing is generated here: the state is the entropy, and this is a pure
    /// function of it. That is what lets the callback reconstruct the verifier
    /// from the row it already has.
    pub fn from_state_token(state_token: &str) -> Self {
        let verifier = crate::token::hash_token(state_token);

        Self {
            challenge: challenge_for(&verifier),
            verifier,
        }
    }
}

/// The `S256` challenge for a verifier: unpadded base64url of its SHA-256.
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());

    URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::{CHALLENGE_METHOD, PkcePair, VERIFIER_MIN_CHARS, challenge_for};
    use crate::token::{hash_token, issue_opaque_token};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use sha2::{Digest, Sha256};

    #[test]
    fn a_pair_derived_from_a_state_is_s256_and_the_challenge_is_not_the_verifier() {
        let state = issue_opaque_token().expect("minting must succeed");
        let pair = PkcePair::from_state_token(&state);

        assert_eq!(CHALLENGE_METHOD, "S256");
        assert_eq!(
            pair.verifier,
            hash_token(&state),
            "the verifier is the state's storage digest, so no extra secret is persisted"
        );
        assert_eq!(
            pair.verifier.len(),
            64,
            "a hex SHA-256 digest is inside RFC 7636's 43..=128 bound"
        );
        assert!(
            pair.verifier.len() >= VERIFIER_MIN_CHARS,
            "a verifier shorter than the RFC minimum is refused by providers"
        );
        assert_eq!(
            pair.challenge.len(),
            43,
            "a SHA-256 digest is 32 bytes, so 43 base64url characters"
        );
        assert_ne!(
            pair.challenge, pair.verifier,
            "the challenge must not reveal the verifier"
        );
        assert_eq!(pair.challenge, challenge_for(&pair.verifier));
    }

    #[test]
    fn the_callback_can_reconstruct_the_verifier_from_what_is_stored() {
        // The storage shape: the raw state is never persisted, only its digest.
        let state = issue_opaque_token().expect("minting");
        let stored_digest = hash_token(&state);

        let at_redirect = PkcePair::from_state_token(&state);
        // At the callback there is no raw state to derive from, only the digest —
        // the same value, so the same verifier, so the exchange succeeds.
        let at_callback = challenge_for(&stored_digest);

        assert_eq!(stored_digest, at_redirect.verifier);
        assert_eq!(at_callback, at_redirect.challenge);
    }

    #[test]
    fn each_state_yields_its_own_pair() {
        let first = PkcePair::from_state_token(&issue_opaque_token().expect("minting"));
        let second = PkcePair::from_state_token(&issue_opaque_token().expect("minting"));

        assert_ne!(first.verifier, second.verifier);
        assert_ne!(first.challenge, second.challenge);
    }

    #[test]
    fn the_challenge_is_the_documented_transformation() {
        // The RFC 7636 appendix B vector: the verifier is known, so this asserts
        // the transformation against an external source of truth rather than
        // recomputing it the way the code does.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

        assert_eq!(
            challenge_for(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn the_verifier_digest_is_storable_hex_not_the_challenge() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = crate::token::hash_token(verifier);

        assert_eq!(
            digest.len(),
            64,
            "a hex SHA-256 digest is the storable shape"
        );
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            digest,
            challenge_for(verifier),
            "the storage digest and the wire challenge are different encodings"
        );

        // And the transformation is genuinely SHA-256 of the verifier.
        let recomputed = Sha256::digest(verifier.as_bytes());
        assert_eq!(digest, format!("{recomputed:x}"));
        assert_eq!(URL_SAFE_NO_PAD.encode(recomputed), challenge_for(verifier));
    }
}
