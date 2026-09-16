//! SQL for the identity tables.
//!
//! `users` and `sessions` are the tables this domain owns (ADR-0011); the SQL is
//! here and nowhere else. Queries use the runtime-checked `sqlx::query` API, not
//! the compile-time macros, so building the crate never needs a database —
//! consistent with `jiuyue-store`.
//!
//! Unique-constraint violations are translated into [`AuthError`] variants by
//! constraint name rather than by matching message text, so "email already
//! registered" stays distinguishable from "username already taken" even if the
//! wording of a PostgreSQL error changes.

use sqlx::types::time::OffsetDateTime;
use sqlx::{PgPool, Row};

use crate::error::AuthError;

const INSERT_USER: &str = "\
    INSERT INTO users (id, username, email, password_hash, display_name) \
    VALUES ($1, $2, $3, $4, $5) \
    RETURNING id::text AS id, username, email, display_name, avatar_url, \
              password_hash, email_verified_at, created_at";

const INSERT_SESSION: &str = "\
    INSERT INTO sessions \
        (id, user_id, refresh_token_hash, device_label, user_agent, ip_address, expires_at) \
    VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))";

const SELECT_USER: &str = "\
    SELECT id::text AS id, username, email, display_name, avatar_url, \
           password_hash, email_verified_at, created_at \
    FROM users";

const SELECT_ACTIVE_SESSION: &str = "\
    SELECT s.id::text AS id, s.user_id::text AS user_id, u.username \
    FROM sessions AS s \
    JOIN users AS u ON u.id = s.user_id \
    WHERE s.revoked_at IS NULL AND s.expires_at > now()";

/// A row of `users`, as this module needs it.
#[derive(Debug, Clone)]
pub struct UserRow {
    /// ULID.
    pub id: String,
    /// `@handle`, already lowercase.
    pub username: String,
    /// Email, already lowercase.
    pub email: String,
    /// Display name.
    pub display_name: String,
    /// Avatar URL, when set.
    pub avatar_url: Option<String>,
    /// Argon2id PHC string; `None` for OAuth-only accounts.
    pub password_hash: Option<String>,
    /// When the email was verified; `None` until the email ticket lands.
    pub email_verified_at: Option<OffsetDateTime>,
    /// Account creation time.
    pub created_at: OffsetDateTime,
}

/// A row of `sessions` joined with the owning user's handle.
///
/// Only the fields a live request needs: `expires_at` and `revoked_at` are
/// filtered in SQL, so a row that comes back is by definition usable.
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// Session ULID.
    pub id: String,
    /// Owning user's ULID.
    pub user_id: String,
    /// Owning user's `@handle`.
    pub username: String,
}

/// Everything needed to insert a new account.
#[derive(Debug, Clone, Copy)]
pub struct NewAccount<'a> {
    /// Application-generated ULID.
    pub id: &'a str,
    /// Lowercase `@handle`.
    pub username: &'a str,
    /// Lowercase email.
    pub email: &'a str,
    /// Argon2id PHC string.
    pub password_hash: &'a str,
    /// Resolved display name (never blank).
    pub display_name: &'a str,
}

/// Everything needed to open a session.
#[derive(Debug, Clone, Copy)]
pub struct NewSession<'a> {
    /// Application-generated ULID.
    pub id: &'a str,
    /// Owning user's ULID.
    pub user_id: &'a str,
    /// SHA-256 hex of the refresh token.
    pub refresh_token_hash: &'a str,
    /// Client-supplied device label, for the device list.
    pub device_label: Option<&'a str>,
    /// Observed User-Agent.
    pub user_agent: Option<&'a str>,
    /// Observed client address.
    pub ip_address: Option<&'a str>,
    /// Refresh-token lifetime in seconds; the database turns it into an expiry
    /// with its own clock, so `expires_at` can never disagree with `now()`.
    pub refresh_ttl_secs: f64,
}

/// Data access for identity.
#[derive(Debug, Clone)]
pub struct SessionRepository {
    pool: PgPool,
}

impl SessionRepository {
    /// Wrap a pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert an account and open its first session atomically.
    ///
    /// The two rows are one unit of work: a registration that produced a user but
    /// no session would be a confusing half-success, so both are in one
    /// transaction and a duplicate email or username rolls the whole thing back.
    pub async fn create_account(
        &self,
        account: &NewAccount<'_>,
        session: &NewSession<'_>,
    ) -> Result<UserRow, AuthError> {
        let mut transaction = self.pool.begin().await.map_err(AuthError::Database)?;

        let row = sqlx::query(INSERT_USER)
            .bind(account.id)
            .bind(account.username)
            .bind(account.email)
            .bind(account.password_hash)
            .bind(account.display_name)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_account_insert_error)?;

        let user = user_from_row(&row);

        sqlx::query(INSERT_SESSION)
            .bind(session.id)
            .bind(session.user_id)
            .bind(session.refresh_token_hash)
            .bind(session.device_label)
            .bind(session.user_agent)
            .bind(session.ip_address)
            .bind(session.refresh_ttl_secs)
            .execute(&mut *transaction)
            .await
            .map_err(AuthError::Database)?;

        transaction.commit().await.map_err(AuthError::Database)?;

        Ok(user)
    }

    /// Open a session for an existing account.
    pub async fn create_session(&self, session: &NewSession<'_>) -> Result<(), AuthError> {
        sqlx::query(INSERT_SESSION)
            .bind(session.id)
            .bind(session.user_id)
            .bind(session.refresh_token_hash)
            .bind(session.device_label)
            .bind(session.user_agent)
            .bind(session.ip_address)
            .bind(session.refresh_ttl_secs)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(AuthError::Database)
    }

    /// Look an account up by its normalised email.
    pub async fn find_user_by_email(&self, email: &str) -> Result<Option<UserRow>, AuthError> {
        self.find_user_by("email", email).await
    }

    /// Look an account up by its ULID.
    pub async fn find_user_by_id(&self, id: &str) -> Result<Option<UserRow>, AuthError> {
        self.find_user_by("id", id).await
    }

    /// Resolve a session by refresh-token digest, only if it is still usable.
    pub async fn find_active_session_by_token_hash(
        &self,
        refresh_token_hash: &str,
    ) -> Result<Option<SessionRow>, AuthError> {
        let query = format!("{SELECT_ACTIVE_SESSION} AND s.refresh_token_hash = $1");

        sqlx::query(&query)
            .bind(refresh_token_hash)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| session_from_row(&row)))
            .map_err(AuthError::Database)
    }

    /// Resolve a session by id, only if it is still usable.
    ///
    /// This is the revocation check: `revoked_at IS NULL` and `expires_at > now()`
    /// are evaluated by the database on every call, so a logout is visible to the
    /// very next request and no cache can delay it.
    pub async fn find_active_session_by_id(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionRow>, AuthError> {
        let query = format!("{SELECT_ACTIVE_SESSION} AND s.id = $1");

        sqlx::query(&query)
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| session_from_row(&row)))
            .map_err(AuthError::Database)
    }

    /// Revoke a session if it is still active.
    pub async fn revoke_session(&self, session_id: &str) -> Result<(), AuthError> {
        sqlx::query(
            "UPDATE sessions SET revoked_at = now() \
             WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(AuthError::Database)
    }

    /// Record activity on a session, for the device list's "last seen".
    pub async fn touch_session(&self, session_id: &str) -> Result<(), AuthError> {
        sqlx::query(
            "UPDATE sessions SET last_seen_at = now() \
             WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(AuthError::Database)
    }

    /// Mark an account's email verified and return the updated row.
    ///
    /// `COALESCE` keeps the *first* verification time: redeeming a second link (or
    /// re-verifying an already-verified account) must not move the timestamp that
    /// records when the address was proven.
    pub async fn mark_email_verified(&self, user_id: &str) -> Result<UserRow, AuthError> {
        sqlx::query(
            "UPDATE users \
             SET email_verified_at = COALESCE(email_verified_at, now()), updated_at = now() \
             WHERE id = $1 \
             RETURNING id::text AS id, username, email, display_name, avatar_url, \
                       password_hash, email_verified_at, created_at",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(AuthError::Database)?
        .map(|row| user_from_row(&row))
        .ok_or(AuthError::AccountMissing)
    }

    /// Replace an account's password hash.
    ///
    /// Callers revoke the account's sessions after this — a password change must
    /// end every session opened with the old one.
    pub async fn update_password(
        &self,
        user_id: &str,
        password_hash: &str,
    ) -> Result<(), AuthError> {
        let updated =
            sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
                .bind(user_id)
                .bind(password_hash)
                .execute(&self.pool)
                .await
                .map_err(AuthError::Database)?
                .rows_affected();

        if updated == 0 {
            return Err(AuthError::AccountMissing);
        }

        Ok(())
    }

    /// Revoke every active session for one account.
    ///
    /// This is what makes "reset the password" also mean "sign out everywhere":
    /// each revoked row makes the access token that names it fail on the very next
    /// request, because `authenticate` re-reads the row rather than trusting the
    /// signed token.
    pub async fn revoke_all_sessions(&self, user_id: &str) -> Result<(), AuthError> {
        sqlx::query(
            "UPDATE sessions SET revoked_at = now() \
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(AuthError::Database)
    }

    /// Shared body for the two account lookups.
    async fn find_user_by(&self, column: &str, value: &str) -> Result<Option<UserRow>, AuthError> {
        // `column` is a hard-coded literal from the two callers above, never input.
        let query = format!("{SELECT_USER} WHERE {column} = $1");

        sqlx::query(&query)
            .bind(value)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| user_from_row(&row)))
            .map_err(AuthError::Database)
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

fn session_from_row(row: &sqlx::postgres::PgRow) -> SessionRow {
    SessionRow {
        id: row.get("id"),
        user_id: row.get("user_id"),
        username: row.get("username"),
    }
}

/// Turn a unique-constraint violation on `users` into its specific variant.
fn map_account_insert_error(error: sqlx::Error) -> AuthError {
    let (code, constraint) = match &error {
        sqlx::Error::Database(database_error) => (
            database_error.code().map(|code| code.into_owned()),
            database_error.constraint().map(str::to_owned),
        ),
        _ => (None, None),
    };

    if code.as_deref() == Some("23505") {
        match constraint.as_deref() {
            Some("users_email_key") => return AuthError::EmailTaken,
            Some("users_username_key") => return AuthError::UsernameTaken,
            _ => {}
        }
    }

    AuthError::Database(error)
}
