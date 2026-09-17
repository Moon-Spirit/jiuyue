//! Access tokens (signed JWT) and refresh tokens (opaque, hashed at rest).
//!
//! Two different jobs, two different tools:
//!
//! - The **access token** is a short-lived HS256 JWT. It is self-contained so
//!   verifying it is cheap, and it carries the Session id in `sid` so the session
//!   row can be re-checked (and revoked) on every request.
//! - The **refresh token** is 256 bits of OS randomness, base64url-encoded. It is
//!   *not* self-describing: only its SHA-256 digest is stored, so a database leak
//!   yields nothing a client could present. It is the handle the client trades
//!   for a new access token.
//!
//! The signing secret is required to be at least [`MIN_SECRET_BYTES`] long; a
//! short secret is rejected at construction rather than silently accepted.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::AuthError;

/// Minimum signing-secret length. 32 bytes = 256 bits, the HS256 key size.
pub const MIN_SECRET_BYTES: usize = 32;

/// Entropy in a refresh token, in bytes.
const REFRESH_TOKEN_BYTES: usize = 32;

/// Mint 256 bits of OS randomness as a base64url (unpadded) string.
///
/// Every opaque secret in this crate is the same shape — a refresh token, a
/// one-shot link, an OAuth `state`, a PKCE verifier, a limited-session token —
/// so they are all minted here rather than each growing its own copy of the same
/// four lines. The caller decides what to do with it (usually: put it in a URL or
/// an email, and store only [`hash_token`]).
pub fn issue_opaque_token() -> Result<String, AuthError> {
    let mut bytes = [0_u8; REFRESH_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(AuthError::Random)?;

    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Digest an opaque token for storage: lowercase hex SHA-256.
///
/// Matches the `^[0-9a-f]{64}$` checks on every secret column in the schema, so a
/// raw token can never be written where a digest belongs.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    format!("{digest:x}")
}

/// Claims carried by an access token.
///
/// Only identity and expiry — never anything secret, because a JWT is readable
/// by anyone who holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    /// Subject: the User's ULID.
    pub sub: String,
    /// Session id: the `sessions` row this token belongs to.
    pub sid: String,
    /// Issued at, seconds since the Unix epoch.
    pub iat: i64,
    /// Expiry, seconds since the Unix epoch.
    pub exp: i64,
}

/// A newly issued access token and how long it lasts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedAccess {
    /// The encoded JWT.
    pub token: String,
    /// Lifetime in whole seconds from issue.
    pub expires_in: u64,
}

/// Issues and verifies access tokens; mints and digests refresh tokens.
pub struct TokenIssuer {
    encoding: EncodingKey,
    decoding: DecodingKey,
    access_ttl: Duration,
    refresh_ttl: Duration,
}

impl TokenIssuer {
    /// Build an issuer from the shared secret and the two lifetimes.
    ///
    /// Fails when the secret is shorter than [`MIN_SECRET_BYTES`], so a weak or
    /// truncated secret cannot reach the signing path.
    pub fn new(
        secret: &str,
        access_ttl: Duration,
        refresh_ttl: Duration,
    ) -> Result<Self, AuthError> {
        if secret.len() < MIN_SECRET_BYTES {
            return Err(AuthError::Config(
                "JWT secret must be at least 32 bytes of entropy",
            ));
        }

        Ok(Self {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
            access_ttl,
            refresh_ttl,
        })
    }

    /// Sign an access token binding a user to a session.
    pub fn issue_access(&self, user_id: &str, session_id: &str) -> Result<IssuedAccess, AuthError> {
        let issued_at = now_seconds()?;
        let expires_in = self.access_ttl.as_secs();

        let claims = AccessClaims {
            sub: user_id.to_owned(),
            sid: session_id.to_owned(),
            iat: issued_at,
            exp: issued_at.saturating_add(i64::try_from(expires_in).unwrap_or(i64::MAX)),
        };

        let token =
            encode(&Header::default(), &claims, &self.encoding).map_err(AuthError::Token)?;

        Ok(IssuedAccess { token, expires_in })
    }

    /// Verify an access token's signature and expiry, returning its claims.
    ///
    /// Audience validation is off because these tokens carry no `aud`; expiry is
    /// required and checked (jsonwebtoken's default, plus a 60 s skew leeway).
    pub fn verify_access(&self, token: &str) -> Result<AccessClaims, AuthError> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_aud = false;

        let data =
            decode::<AccessClaims>(token, &self.decoding, &validation).map_err(AuthError::Token)?;

        Ok(data.claims)
    }

    /// The configured refresh-token lifetime.
    pub fn refresh_ttl(&self) -> Duration {
        self.refresh_ttl
    }

    /// Mint a fresh opaque refresh token (256 bits, base64url without padding).
    pub fn issue_refresh_token(&self) -> Result<String, AuthError> {
        issue_opaque_token()
    }

    /// Digest a refresh token for storage: lowercase hex SHA-256.
    ///
    /// The digest is what the `sessions.refresh_token_hash` check accepts, so a
    /// raw token can never be written to that column by accident.
    pub fn hash_refresh_token(token: &str) -> String {
        hash_token(token)
    }

    /// Mint a fresh opaque one-shot link token (256 bits, base64url without padding).
    ///
    /// Verification and password-reset links carry the same shape of secret as a
    /// refresh token — pure randomness with no structure to guess — and are
    /// minted the same way. Only the digest is ever stored, so the link in the
    /// email is the only copy of the value.
    pub fn issue_link_token(&self) -> Result<String, AuthError> {
        issue_opaque_token()
    }

    /// Digest a one-shot link token for storage: lowercase hex SHA-256.
    ///
    /// Matches `account_tokens.token_hash`'s `^[0-9a-f]{64}$` CHECK.
    pub fn hash_link_token(token: &str) -> String {
        hash_token(token)
    }
}

/// Current wall-clock time as whole seconds since the Unix epoch.
fn now_seconds() -> Result<i64, AuthError> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| AuthError::Clock("system clock is before the Unix epoch"))?;

    i64::try_from(elapsed.as_secs())
        .map_err(|_| AuthError::Clock("system clock is outside the representable range"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use jsonwebtoken::{EncodingKey, Header, encode};

    use super::{AccessClaims, MIN_SECRET_BYTES, TokenIssuer};
    use crate::error::AuthError;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn issuer() -> TokenIssuer {
        TokenIssuer::new(
            SECRET,
            Duration::from_secs(900),
            Duration::from_secs(60 * 60 * 24 * 30),
        )
        .expect("a 32-byte secret must be accepted")
    }

    #[test]
    fn a_short_secret_is_rejected() {
        let result = TokenIssuer::new(
            "too-short",
            Duration::from_secs(900),
            Duration::from_secs(60),
        );

        assert!(matches!(result, Err(AuthError::Config(_))));
        assert!(SECRET.len() >= MIN_SECRET_BYTES);
    }

    #[test]
    fn issued_tokens_round_trip_with_their_identity() {
        let issued = issuer()
            .issue_access("01JABC1234567890ABCDEFGHJ1", "01JABC1234567890ABCDEFGHJ2")
            .expect("issue");

        assert_eq!(issued.expires_in, 900);

        let claims = issuer().verify_access(&issued.token).expect("verify");
        assert_eq!(claims.sub, "01JABC1234567890ABCDEFGHJ1");
        assert_eq!(claims.sid, "01JABC1234567890ABCDEFGHJ2");
        assert_eq!(claims.exp - claims.iat, 900);
    }

    #[test]
    fn a_tampered_token_is_rejected() {
        let issued = issuer().issue_access("user", "session").expect("issue");

        // Flip the final base64url character of the signature.
        let mut tampered = issued.token.clone();
        let last = tampered.pop().expect("token is non-empty");
        tampered.push(if last == 'A' { 'B' } else { 'A' });

        assert!(
            issuer().verify_access(&tampered).is_err(),
            "a token whose signature changed must not verify"
        );
    }

    #[test]
    fn an_expired_token_is_rejected() {
        // Forge a token with the same secret but an `exp` an hour in the past.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;

        let claims = AccessClaims {
            sub: "user".to_owned(),
            sid: "session".to_owned(),
            iat: now - 7_200,
            exp: now - 3_600,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .expect("forge");

        assert!(
            issuer().verify_access(&token).is_err(),
            "an expired token must not verify"
        );
    }

    #[test]
    fn refresh_tokens_are_random_and_have_a_stable_digest() {
        let issuer = issuer();

        let first = issuer.issue_refresh_token().expect("mint");
        let second = issuer.issue_refresh_token().expect("mint");

        assert_ne!(first, second, "each refresh token must be unique");
        assert_eq!(first.len(), 43, "32 bytes base64url without padding");

        let digest = TokenIssuer::hash_refresh_token(&first);
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(digest, digest.to_lowercase());
        assert_eq!(digest, TokenIssuer::hash_refresh_token(&first));
        assert_ne!(digest, TokenIssuer::hash_refresh_token(&second));
        assert!(
            !digest.contains(&first),
            "the digest must not embed the token"
        );
    }

    #[test]
    fn link_tokens_are_random_and_hash_to_a_storable_digest() {
        let issuer = issuer();

        let token = issuer.issue_link_token().expect("mint");
        let other = issuer.issue_link_token().expect("mint");

        assert_ne!(token, other, "each link token must be unique");
        assert_eq!(token.len(), 43, "32 bytes base64url without padding");

        let digest = TokenIssuer::hash_link_token(&token);
        assert_eq!(
            digest.len(),
            64,
            "account_tokens stores a 64-char hex digest"
        );
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(digest, TokenIssuer::hash_link_token(&token));
        assert_ne!(digest, token, "the digest must not be the token");
    }
}
