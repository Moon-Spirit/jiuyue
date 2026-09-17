//! SQL for the three tables third-party sign-in owns.
//!
//! `oauth_identities`, `oauth_states` and `oauth_pending_sessions` are this
//! module's tables (ADR-0011: one table, one owning crate). Queries are the
//! runtime-checked `sqlx::query` API for the same reason `repository.rs` uses it:
//! building the crate must never need a database.
//!
//! # The single-use guarantee
//!
//! [`OAuthRepository::consume_state`] is one UPDATE that matches only an
//! unconsumed, unexpired state and returns the row only to the caller that won
//! the flip — the same protocol as `account_tokens`. It also clears
//! `code_verifier_hash` in the same statement, so the verifier dies with the
//! state rather than lingering for the life of the row.
//!
//! [`OAuthRepository::consume_pending_session`] follows the same shape: the
//! limited session is spent before the username is written, so a replayed
//! completion cannot mint a second session.

use sqlx::types::time::OffsetDateTime;
use sqlx::{PgPool, Row};

use crate::error::AuthError;
use crate::repository::UserRow;
use jiuyue_contract::OAuthProvider;

/// Everything a round trip needs to remember between the redirect out and the
/// callback in.
#[derive(Debug, Clone, Copy)]
pub struct NewOAuthState<'a> {
    /// Application-generated ULID.
    pub id: &'a str,
    /// SHA-256 hex of the `state` in the authorization URL.
    pub state_hash: &'a str,
    /// Which provider.
    pub provider: OAuthProvider,
    /// SHA-256 hex of the PKCE verifier.
    pub code_verifier_hash: &'a str,
    /// Relative path to return to, when the flow was started from a deep link.
    pub redirect_path: Option<&'a str>,
    /// Non-NULL for a linking attempt by an already-signed-in user.
    pub user_id: Option<&'a str>,
    /// Lifetime in seconds; the database clock computes `expires_at`.
    pub ttl_secs: f64,
}

/// A state row as redemption needs it.
#[derive(Debug, Clone)]
pub struct StoredOAuthState {
    /// Which provider the round trip was started for.
    pub provider: OAuthProvider,
    /// The PKCE verifier's digest; the verifier itself is whatever the client
    /// presented and is only ever compared, never reconstructed.
    pub code_verifier_hash: String,
    /// The account a linking attempt named, if this was one.
    pub user_id: Option<String>,
    /// Where to send the browser afterwards.
    pub redirect_path: Option<String>,
    /// When it stops being accepted.
    pub expires_at: OffsetDateTime,
}

/// Everything needed to bind a provider identity to an account.
#[derive(Debug, Clone, Copy)]
pub struct NewOAuthIdentity<'a> {
    /// Application-generated ULID.
    pub id: &'a str,
    /// The account it belongs to.
    pub user_id: &'a str,
    /// Which provider.
    pub provider: OAuthProvider,
    /// The provider's immutable id for the account.
    pub subject: &'a str,
    /// What the provider calls the account.
    pub display_name: Option<&'a str>,
    /// The address the provider reported, informational only.
    pub email: Option<&'a str>,
    /// Whether the provider asserted the address is verified.
    pub email_verified: bool,
}

/// Everything needed to create an account from a provider identity, in one unit.
#[derive(Debug, Clone, Copy)]
pub struct NewOAuthAccount<'a> {
    /// Application-generated ULID for the account.
    pub user_id: &'a str,
    /// Generated `@handle`, unique by retry rather than by luck.
    pub username: &'a str,
    /// The provider's address, when it reported one.
    pub email: Option<&'a str>,
    /// Display name for the account.
    pub display_name: &'a str,
    /// Whether the address was reported as verified.
    pub email_verified: bool,
    /// The provider identity to bind.
    pub identity: NewOAuthIdentity<'a>,
    /// Application-generated ULID for the limited-session row.
    pub pending_id: &'a str,
    /// SHA-256 hex of the limited-session token.
    pub pending_token_hash: &'a str,
    /// Limited-session lifetime in seconds.
    pub pending_ttl_secs: f64,
}

/// Data access for the three OAuth tables.
#[derive(Debug, Clone)]
pub struct OAuthRepository {
    pool: PgPool,
}

impl OAuthRepository {
    /// Wrap a pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Store a state (and the PKCE verifier) for one round trip.
    pub async fn insert_state(&self, state: &NewOAuthState<'_>) -> Result<(), AuthError> {
        sqlx::query(
            "INSERT INTO oauth_states \
                 (id, state_hash, provider, code_verifier_hash, redirect_path, user_id, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
        )
        .bind(state.id)
        .bind(state.state_hash)
        .bind(state.provider.as_str())
        .bind(state.code_verifier_hash)
        .bind(state.redirect_path)
        .bind(state.user_id)
        .bind(state.ttl_secs)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(AuthError::Database)
    }

    /// Spend a state and return it, once.
    ///
    /// The UPDATE is the whole redemption protocol: it matches only an unconsumed,
    /// unexpired row, flips it, and returns it to exactly one caller. A replayed
    /// `state` therefore finds nothing, which is the login-CSRF refusal.
    ///
    /// `code_verifier_hash` is cleared as part of the spend. It has done its job
    /// by the time this returns, and leaving it behind would keep a secret alive
    /// in a table nobody reads.
    pub async fn consume_state(
        &self,
        state_hash: &str,
    ) -> Result<Option<StoredOAuthState>, AuthError> {
        sqlx::query(
            "UPDATE oauth_states \
             SET consumed_at = now(), code_verifier_hash = repeat('0', 64) \
             WHERE state_hash = $1 AND consumed_at IS NULL AND expires_at > now() \
             RETURNING provider, code_verifier_hash, user_id::text AS user_id, redirect_path, expires_at",
        )
        .bind(state_hash)
        .fetch_optional(&self.pool)
        .await
        .map(|row| {
            row.and_then(|row| {
                Some(StoredOAuthState {
                    provider: OAuthProvider::from_str_opt(&row.get::<String, _>("provider"))?,
                    code_verifier_hash: row.get("code_verifier_hash"),
                    user_id: row.get("user_id"),
                    redirect_path: row.get("redirect_path"),
                    expires_at: row.get("expires_at"),
                })
            })
        })
        .map_err(AuthError::Database)
    }

    /// Look an account up by a provider identity.
    pub async fn find_user_by_identity(
        &self,
        provider: OAuthProvider,
        subject: &str,
    ) -> Result<Option<UserRow>, AuthError> {
        sqlx::query(
            "SELECT u.id::text AS id, u.username, u.email, u.display_name, u.avatar_url, \
                    u.password_hash, u.email_verified_at, u.created_at \
             FROM oauth_identities AS oi \
             JOIN users AS u ON u.id = oi.user_id \
             WHERE oi.provider = $1 AND oi.subject = $2",
        )
        .bind(provider.as_str())
        .bind(subject)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| user_from_row(&row)))
        .map_err(AuthError::Database)
    }

    /// Load an account by its ULID, for a callback that already knows who it is.
    pub async fn find_user_by_id(&self, id: &str) -> Result<Option<UserRow>, AuthError> {
        sqlx::query(
            "SELECT id::text AS id, username, email, display_name, avatar_url, \
                    password_hash, email_verified_at, created_at \
             FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| user_from_row(&row)))
        .map_err(AuthError::Database)
    }

    /// The account a provider identity is bound to, whoever it is.
    ///
    /// Used only to phrase a refusal: the difference between "this identity is
    /// already yours" and "this identity belongs to someone else".
    pub async fn identity_owner(
        &self,
        provider: OAuthProvider,
        subject: &str,
    ) -> Result<Option<String>, AuthError> {
        sqlx::query(
            "SELECT user_id::text AS user_id FROM oauth_identities \
             WHERE provider = $1 AND subject = $2",
        )
        .bind(provider.as_str())
        .bind(subject)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| row.get("user_id")))
        .map_err(AuthError::Database)
    }

    /// Bind a provider identity to an account, if nothing else holds it.
    ///
    /// `ON CONFLICT DO NOTHING` on `(provider, subject)` is what makes "a provider
    /// identity belongs to exactly one account" true under concurrency without a
    /// read-then-write race. `Ok(false)` means somebody else won.
    pub async fn link_identity(&self, identity: &NewOAuthIdentity<'_>) -> Result<bool, AuthError> {
        let inserted = sqlx::query(
            "INSERT INTO oauth_identities \
                 (id, user_id, provider, subject, display_name, email, email_verified) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (provider, subject) DO NOTHING",
        )
        .bind(identity.id)
        .bind(identity.user_id)
        .bind(identity.provider.as_str())
        .bind(identity.subject)
        .bind(identity.display_name)
        .bind(identity.email)
        .bind(identity.email_verified)
        .execute(&self.pool)
        .await
        .map_err(AuthError::Database)?
        .rows_affected()
            > 0;

        Ok(inserted)
    }

    /// Whether an account already uses an address.
    ///
    /// The take-over check runs before any write so the refusal is cheap and the
    /// message precise; `users_email_key` is still the authority, and a race that
    /// slips past this check is refused by the constraint.
    pub async fn email_is_taken(&self, email: &str) -> Result<bool, AuthError> {
        sqlx::query("SELECT 1 AS present FROM users WHERE email = $1")
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.is_some())
            .map_err(AuthError::Database)
    }

    /// Create the account, bind the identity, and mint the limited session, all
    /// or nothing.
    ///
    /// The three rows are one unit of work: an account with no identity would be
    /// unreachable by the provider that created it, and an identity with no
    /// pending session would leave a username-less user with no way forward.
    ///
    /// [`AuthError::UsernameTaken`] is a *request* to try another handle, not a
    /// failure — the caller owns the retry because the caller owns the generator.
    /// [`AuthError::EmailTaken`] means the take-over guard was beaten by a race
    /// and is final.
    pub async fn create_oauth_account(
        &self,
        account: &NewOAuthAccount<'_>,
    ) -> Result<UserRow, AuthError> {
        let mut transaction = self.pool.begin().await.map_err(AuthError::Database)?;

        let row = sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, display_name, email_verified_at) \
             VALUES ($1, $2, $3, NULL, $4, CASE WHEN $5 THEN now() ELSE NULL END) \
             RETURNING id::text AS id, username, email, display_name, avatar_url, \
                       password_hash, email_verified_at, created_at",
        )
        .bind(account.user_id)
        .bind(account.username)
        .bind(account.email)
        .bind(account.display_name)
        .bind(account.email_verified)
        .fetch_one(&mut *transaction)
        .await
        .map_err(map_user_insert_error)?;

        let user = user_from_row(&row);

        sqlx::query(
            "INSERT INTO oauth_identities \
                 (id, user_id, provider, subject, display_name, email, email_verified) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(account.identity.id)
        .bind(account.user_id)
        .bind(account.identity.provider.as_str())
        .bind(account.identity.subject)
        .bind(account.identity.display_name)
        .bind(account.identity.email)
        .bind(account.identity.email_verified)
        .execute(&mut *transaction)
        .await
        .map_err(map_identity_insert_error)?;

        sqlx::query(
            "INSERT INTO oauth_pending_sessions \
                 (id, token_hash, provider, subject, user_id, expires_at) \
             VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))",
        )
        .bind(account.pending_id)
        .bind(account.pending_token_hash)
        .bind(account.identity.provider.as_str())
        .bind(account.identity.subject)
        .bind(account.user_id)
        .bind(account.pending_ttl_secs)
        .execute(&mut *transaction)
        .await
        .map_err(AuthError::Database)?;

        transaction.commit().await.map_err(AuthError::Database)?;

        Ok(user)
    }

    /// Mint a fresh limited session for an account that still has no username.
    ///
    /// The upsert retires whatever token was outstanding for the same provider
    /// identity in the same statement, so an abandoned onboarding can be resumed
    /// without leaving two live limited tokens behind.
    pub async fn mint_pending_session(
        &self,
        id: &str,
        token_hash: &str,
        provider: OAuthProvider,
        subject: &str,
        user_id: &str,
        ttl_secs: f64,
    ) -> Result<(), AuthError> {
        sqlx::query(
            "INSERT INTO oauth_pending_sessions \
                 (id, token_hash, provider, subject, user_id, expires_at) \
             VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6)) \
             ON CONFLICT (provider, subject) DO UPDATE SET \
                 id = EXCLUDED.id, \
                 token_hash = EXCLUDED.token_hash, \
                 user_id = EXCLUDED.user_id, \
                 consumed_at = NULL, \
                 expires_at = EXCLUDED.expires_at",
        )
        .bind(id)
        .bind(token_hash)
        .bind(provider.as_str())
        .bind(subject)
        .bind(user_id)
        .bind(ttl_secs)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(AuthError::Database)
    }

    /// The live limited session for a provider identity, if one is outstanding.
    ///
    /// This is the question "is this account still being onboarded?", asked the
    /// only way that has a truthful answer. A finished OAuth account has no
    /// password, so `users.password_hash IS NULL` cannot distinguish it from one
    /// whose owner never chose a username — and treating them the same would send
    /// a returning user back to the username step forever. An outstanding
    /// (unconsumed, unexpired) row is exactly the state "onboarding is
    /// unfinished".
    pub async fn active_pending_session(
        &self,
        provider: OAuthProvider,
        subject: &str,
    ) -> Result<Option<String>, AuthError> {
        sqlx::query(
            "SELECT id::text AS id FROM oauth_pending_sessions \
             WHERE provider = $1 AND subject = $2 \
               AND consumed_at IS NULL AND expires_at > now()",
        )
        .bind(provider.as_str())
        .bind(subject)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| row.get("id")))
        .map_err(AuthError::Database)
    }

    /// Spend one outstanding limited session by its row id.
    ///
    /// Used when a returning user's onboarding is resumed: the old token is
    /// retired in the same breath as a new one being minted, so exactly one live
    /// limited session exists per provider identity at any moment.
    pub async fn consume_pending_session_by_id(&self, id: &str) -> Result<(), AuthError> {
        sqlx::query(
            "UPDATE oauth_pending_sessions SET consumed_at = now() \
             WHERE id = $1 AND consumed_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(AuthError::Database)
    }

    /// Claim a username and spend the limited session, atomically.
    ///
    /// This is the only way a first-time provider user becomes a full account, and
    /// the ordering is what makes the limited session single-use: the username is
    /// written and the token spent in one statement, so a replayed completion
    /// updates nothing and finds no row.
    ///
    /// [`AuthError::UsernameTaken`] means the handle lost a race; the caller may
    /// retry with the same token because nothing was written.
    pub async fn claim_username(
        &self,
        token_hash: &str,
        username: &str,
        display_name: &str,
    ) -> Result<UserRow, AuthError> {
        let mut transaction = self.pool.begin().await.map_err(AuthError::Database)?;

        let row = sqlx::query(
            "UPDATE users SET username = $2, display_name = $3, updated_at = now() \
             WHERE id = ( \
                 SELECT user_id FROM oauth_pending_sessions \
                 WHERE token_hash = $1 AND consumed_at IS NULL AND expires_at > now() \
                 FOR UPDATE \
             ) \
             RETURNING id::text AS id, username, email, display_name, avatar_url, \
                       password_hash, email_verified_at, created_at",
        )
        .bind(token_hash)
        .bind(username)
        .bind(display_name)
        .fetch_optional(&mut *transaction)
        .await;

        let row = match row {
            // A taken handle is retryable and wrote nothing: the transaction is
            // rolled back by dropping it, and the caller keeps the same token.
            Err(error) => return Err(map_user_update_error(error)),
            // No row means the token was missing, spent, or expired. The
            // transaction made no change; rolling back is the honest outcome.
            Ok(None) => return Err(AuthError::OAuthStateInvalid),
            Ok(Some(row)) => row,
        };

        let user = user_from_row(&row);

        sqlx::query("UPDATE oauth_pending_sessions SET consumed_at = now() WHERE token_hash = $1")
            .bind(token_hash)
            .execute(&mut *transaction)
            .await
            .map_err(AuthError::Database)?;

        transaction.commit().await.map_err(AuthError::Database)?;

        Ok(user)
    }
}

fn user_from_row(row: &sqlx::postgres::PgRow) -> UserRow {
    UserRow {
        id: row.get("id"),
        username: row.get("username"),
        email: row.get("email"),
        display_name: row.get("display_name"),
        avatar_url: row.get("avatar_url"),
        password_hash: row.get("password_hash"),
        email_verified_at: row.get("email_verified_at"),
        created_at: row.get("created_at"),
    }
}

/// The unique-constraint name behind a database error, when there is one.
fn constraint_name(error: &sqlx::Error) -> Option<&str> {
    match error {
        sqlx::Error::Database(database_error) => database_error.constraint(),
        _ => None,
    }
}

fn map_user_insert_error(error: sqlx::Error) -> AuthError {
    match constraint_name(&error) {
        Some("users_username_key") => AuthError::UsernameTaken,
        Some("users_email_key") => AuthError::EmailTaken,
        _ => AuthError::Database(error),
    }
}

fn map_user_update_error(error: sqlx::Error) -> AuthError {
    match constraint_name(&error) {
        Some("users_username_key") => AuthError::UsernameTaken,
        _ => AuthError::Database(error),
    }
}

fn map_identity_insert_error(error: sqlx::Error) -> AuthError {
    match constraint_name(&error) {
        Some("oauth_identities_provider_subject_key") => AuthError::OAuthIdentityTaken,
        _ => AuthError::Database(error),
    }
}
