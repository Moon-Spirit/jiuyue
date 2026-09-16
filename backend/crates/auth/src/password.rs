//! Argon2id password hashing, bounded so a login burst cannot exhaust memory.
//!
//! Parameters are the OWASP baseline — m = 19456 KiB (19 MiB), t = 2, p = 1 —
//! which means *every in-flight hash allocates ~19 MiB*. On the 2 vCPU / 2 GB
//! production box an unbounded burst of login attempts would therefore be a
//! denial-of-service against the process, so concurrency is capped by a
//! semaphore and the work runs on the blocking pool rather than stalling a
//! runtime worker.
//!
//! The plaintext password exists only as a local, moved into the blocking task,
//! and is dropped there. It is never logged, never placed in an error, and never
//! stored — only the PHC string leaves this module.

use std::sync::Arc;

use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _, phc::PasswordHash};
use argon2::{Algorithm, Argon2, Params, Version};
use tokio::sync::Semaphore;

use crate::error::AuthError;

/// Memory cost in KiB (19 MiB), from the OWASP Argon2id recommendation.
pub const MEMORY_COST_KIB: u32 = 19_456;
/// Iteration count, from the OWASP Argon2id recommendation.
pub const TIME_COST: u32 = 2;
/// Degree of parallelism. Kept at 1: a 2 vCPU box gains nothing from threads and
/// the semaphore, not `p`, is what bounds memory.
pub const PARALLELISM: u32 = 1;
/// Upper bound on simultaneous hashes: 4 × 19 MiB ≈ 76 MiB worst case.
pub const MAX_CONCURRENT_HASHES: usize = 4;

/// Hashes and verifies passwords with Argon2id.
///
/// Cheap to clone-free share by reference; the semaphore is shared through an
/// [`Arc`], so one instance per process is enough.
pub struct PasswordHasher {
    permits: Arc<Semaphore>,
}

impl Default for PasswordHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl PasswordHasher {
    /// Build a hasher with the process-wide concurrency bound.
    pub fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_HASHES)),
        }
    }

    /// Hash a plaintext password, returning its PHC string
    /// (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`).
    ///
    /// A fresh random salt is generated per call, so hashing the same password
    /// twice yields two different strings.
    pub async fn hash(&self, plaintext: &str) -> Result<String, AuthError> {
        let plaintext = plaintext.to_owned();

        self.bounded(move || {
            let argon2 = argon2id()?;
            let hash = argon2
                .hash_password(plaintext.as_bytes())
                .map_err(AuthError::Hash)?;
            Ok(hash.to_string())
        })
        .await
    }

    /// Verify a plaintext password against a stored PHC string.
    ///
    /// Returns `Ok(false)` for a genuine mismatch; a malformed stored hash is an
    /// error, not a mismatch, because that is a server-side defect rather than a
    /// wrong password.
    pub async fn verify(&self, plaintext: &str, phc: &str) -> Result<bool, AuthError> {
        let plaintext = plaintext.to_owned();
        let phc = phc.to_owned();

        self.bounded(move || {
            // `phc::Error` converts into `password_hash::Error`; keep one error
            // type on the boundary rather than leaking the parser's.
            let parsed = PasswordHash::new(&phc).map_err(|error| AuthError::Hash(error.into()))?;
            let argon2 = argon2id()?;

            match argon2.verify_password(plaintext.as_bytes(), &parsed) {
                Ok(()) => Ok(true),
                Err(argon2::password_hash::Error::PasswordInvalid) => Ok(false),
                Err(error) => Err(AuthError::Hash(error)),
            }
        })
        .await
    }

    /// Run one hash on the blocking pool, holding a semaphore permit for its
    /// whole duration so at most [`MAX_CONCURRENT_HASHES`] exist at once.
    async fn bounded<T, F>(&self, work: F) -> Result<T, AuthError>
    where
        F: FnOnce() -> Result<T, AuthError> + Send + 'static,
        T: Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AuthError::Worker)?;

        tokio::task::spawn_blocking(move || {
            // Held for the duration of the hash; released when the closure ends.
            let _permit = permit;
            work()
        })
        .await
        .map_err(|_| AuthError::Worker)?
    }
}

/// Build the Argon2id instance with the pinned parameters.
fn argon2id() -> Result<Argon2<'static>, AuthError> {
    let params =
        Params::new(MEMORY_COST_KIB, TIME_COST, PARALLELISM, None).map_err(AuthError::Params)?;

    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

#[cfg(test)]
mod tests {
    use super::{MEMORY_COST_KIB, PARALLELISM, PasswordHasher, TIME_COST};

    fn argon2_prefix() -> String {
        format!("$argon2id$v=19$m={MEMORY_COST_KIB},t={TIME_COST},p={PARALLELISM}$")
    }

    #[tokio::test]
    async fn hashes_to_an_argon2id_phc_string_without_the_plaintext() {
        let hasher = PasswordHasher::new();
        let phc = hasher.hash("correct horse battery1").await.expect("hash");

        assert!(
            phc.starts_with(&argon2_prefix()),
            "unexpected PHC string: {phc}"
        );
        assert!(
            !phc.contains("correct horse battery1"),
            "the plaintext must never appear in the hash"
        );

        // salt and digest are both base64, so a full hash has 6 fields.
        assert_eq!(phc.split('$').count(), 6);
    }

    #[tokio::test]
    async fn verification_accepts_the_right_password_and_rejects_the_wrong_one() {
        let hasher = PasswordHasher::new();
        let phc = hasher.hash("secret123").await.expect("hash");

        assert!(
            hasher.verify("secret123", &phc).await.expect("verify"),
            "the correct password must verify"
        );
        assert!(
            !hasher.verify("secret124", &phc).await.expect("verify"),
            "a different password must not verify"
        );
    }

    #[tokio::test]
    async fn a_fresh_salt_makes_every_hash_distinct() {
        let hasher = PasswordHasher::new();

        let first = hasher.hash("secret123").await.expect("hash");
        let second = hasher.hash("secret123").await.expect("hash");

        assert_ne!(first, second, "per-password salts must differ");
    }

    #[tokio::test]
    async fn a_malformed_stored_hash_is_an_error_not_a_mismatch() {
        let hasher = PasswordHasher::new();

        let result = hasher.verify("secret123", "not-a-phc-string").await;

        assert!(
            result.is_err(),
            "a corrupt stored hash must surface as an error"
        );
    }
}
