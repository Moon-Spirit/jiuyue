//! Errors returned by [`crate::Store`].
//!
//! Every fallible operation in this crate returns a `Result` built from this
//! enum. Library code never panics on a recoverable condition, and each variant
//! keeps the underlying error as a `source` so the cause is not lost.

use thiserror::Error;

/// Problems connecting to or migrating the PostgreSQL database.
#[derive(Debug, Error)]
pub enum StoreError {
    /// `DATABASE_URL` was not a valid PostgreSQL connection string.
    #[error("invalid database URL")]
    InvalidUrl(#[source] sqlx::Error),

    /// The pool could not reach the database or authenticate.
    #[error("failed to connect to the database")]
    Connect(#[source] sqlx::Error),

    /// A migration failed to apply, or an already-applied one no longer matches
    /// the checksum recorded in `_sqlx_migrations`.
    #[error("failed to run database migrations")]
    Migrate(#[source] sqlx::migrate::MigrateError),
}
