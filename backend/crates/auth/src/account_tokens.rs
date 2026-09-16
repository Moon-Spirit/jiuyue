//! SQL for the one-shot identity links (`account_tokens`).
//!
//! This is the `account_tokens` table's only home (ADR-0011): the verification
//! and password-reset repositories live nowhere else. Queries use the
//! runtime-checked `sqlx::query` API for the same reason `repository.rs` does —
//! building the crate must never need a database.
//!
//! # The two properties the schema and these queries must together guarantee
//!
//! 1. **Only a digest is stored.** The caller mints the token and hashes it; this
//!    module never sees the raw value, and the column's CHECK refuses anything
//!    that is not a 64-char lowercase hex digest.
//! 2. **Redemption is atomic and happens first.** [`AccountTokenRepository::consume`]
//!    flips `consumed_at` in a single statement that also filters on `expires_at`
//!    and `consumed_at IS NULL`, and returns the owner only to the caller that won
//!    the flip. Whoever wins may then fail at whatever comes next — the token stays
//!    spent, which is what "used even if the next step fails halfway" means.

use sqlx::types::time::OffsetDateTime;
use sqlx::{PgPool, Row};

use crate::error::AuthError;

/// What a link is for.
///
/// A token only redeems against its own purpose, so a verification link cannot be
/// spent as a password reset (or the reverse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenPurpose {
    /// Prove control of the address a new account registered with.
    EmailVerification,
    /// Authorise a password change for an account locked out of its inbox-less login.
    PasswordReset,
}

impl TokenPurpose {
    /// The exact `account_tokens.purpose` value (matching the CHECK constraint).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmailVerification => "email_verification",
            Self::PasswordReset => "password_reset",
        }
    }
}

/// Everything needed to issue a link.
#[derive(Debug, Clone, Copy)]
pub struct NewToken<'a> {
    /// Application-generated ULID.
    pub id: &'a str,
    /// Owning user's ULID.
    pub user_id: &'a str,
    /// What the link authorises.
    pub purpose: TokenPurpose,
    /// SHA-256 hex of the token that goes in the email.
    pub token_hash: &'a str,
    /// Lifetime in seconds, turned into `expires_at` by the database clock so the
    /// expiry and the redemption check can never disagree.
    pub ttl_secs: f64,
}

/// A stored token, as redemption needs to reason about it.
#[derive(Debug, Clone)]
pub struct StoredToken {
    /// Owning user's ULID.
    pub user_id: String,
    /// When it stops being accepted.
    pub expires_at: OffsetDateTime,
    /// When it was spent, if it has been.
    pub consumed_at: Option<OffsetDateTime>,
}

impl StoredToken {
    /// Whether the token was already spent.
    pub fn is_consumed(&self) -> bool {
        self.consumed_at.is_some()
    }

    /// Whether the token's lifetime has passed.
    ///
    /// The application clock is not consulted: the value came from PostgreSQL's
    /// `now()` when the row was written, and this only compares it to the wall
    /// clock to phrase an error. The authoritative expiry check is the one inside
    /// [`AccountTokenRepository::consume`], evaluated by the database.
    pub fn is_expired(&self) -> bool {
        self.expires_at <= OffsetDateTime::now_utc()
    }
}

/// Data access for `account_tokens`.
#[derive(Debug, Clone)]
pub struct AccountTokenRepository {
    pool: PgPool,
}

impl AccountTokenRepository {
    /// Wrap a pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Issue a link, retiring any outstanding link of the same purpose.
    ///
    /// The retire and the insert are one transaction: a user who asks for a second
    /// reset link must not leave two live tokens behind. The insert is
    /// `INSERT … SELECT … WHERE EXISTS`, so issuing for a user that does not exist
    /// is a successful no-op that executes the *same* statements — the reason the
    /// forgot-password path can be called with an address that has no account and
    /// still do the same work.
    ///
    /// Returns whether a row was written (`false` when no such user exists).
    pub async fn issue(&self, token: &NewToken<'_>) -> Result<bool, AuthError> {
        let mut transaction = self.pool.begin().await.map_err(AuthError::Database)?;

        sqlx::query(
            "UPDATE account_tokens SET consumed_at = now() \
             WHERE user_id = $1 AND purpose = $2 AND consumed_at IS NULL",
        )
        .bind(token.user_id)
        .bind(token.purpose.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(AuthError::Database)?;

        let inserted = sqlx::query(
            "INSERT INTO account_tokens (id, user_id, purpose, token_hash, expires_at) \
             SELECT $1, u.id, $3, $4, now() + make_interval(secs => $5) \
             FROM users AS u WHERE u.id = $2 \
             RETURNING id",
        )
        .bind(token.id)
        .bind(token.user_id)
        .bind(token.purpose.as_str())
        .bind(token.token_hash)
        .bind(token.ttl_secs)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(AuthError::Database)?
        .is_some();

        transaction.commit().await.map_err(AuthError::Database)?;

        Ok(inserted)
    }

    /// Look a token up without spending it.
    ///
    /// Used only to tell an expired link from an invalid one when
    /// [`Self::consume`] refuses; the redemption decision itself is the atomic
    /// update, never this read.
    pub async fn find(
        &self,
        token_hash: &str,
        purpose: TokenPurpose,
    ) -> Result<Option<StoredToken>, AuthError> {
        sqlx::query(
            "SELECT user_id::text AS user_id, expires_at, consumed_at \
             FROM account_tokens WHERE token_hash = $1 AND purpose = $2",
        )
        .bind(token_hash)
        .bind(purpose.as_str())
        .fetch_optional(&self.pool)
        .await
        .map(|row| {
            row.map(|row| StoredToken {
                user_id: row.get("user_id"),
                expires_at: row.get("expires_at"),
                consumed_at: row.get("consumed_at"),
            })
        })
        .map_err(AuthError::Database)
    }

    /// Spend a token and return the user it belonged to.
    ///
    /// The single UPDATE is the whole redemption protocol: it matches only an
    /// unconsumed, unexpired token, and returns the owner only when it actually
    /// flipped the row. Two concurrent redemptions therefore cannot both succeed,
    /// and the winner's subsequent step failing does not un-spend the token.
    ///
    /// `Ok(None)` covers unknown, already-spent, and expired alike; the caller
    /// may call [`Self::find`] to phrase the refusal more usefully.
    pub async fn consume(
        &self,
        token_hash: &str,
        purpose: TokenPurpose,
    ) -> Result<Option<String>, AuthError> {
        sqlx::query(
            "UPDATE account_tokens SET consumed_at = now() \
             WHERE token_hash = $1 AND purpose = $2 \
               AND consumed_at IS NULL AND expires_at > now() \
             RETURNING user_id::text AS user_id",
        )
        .bind(token_hash)
        .bind(purpose.as_str())
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| row.get("user_id")))
        .map_err(AuthError::Database)
    }
}
