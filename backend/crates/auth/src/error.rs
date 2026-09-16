//! Errors raised by the identity module.
//!
//! Every fallible operation returns a `Result` built from [`AuthError`]; library
//! code never panics on a recoverable condition. The variants are the vocabulary
//! the HTTP layer maps to status codes, and [`AuthError::Validation`] carries the
//! field-level detail the client renders — so a duplicate email is a different
//! variant, not a different string.

use jiuyue_contract::FieldError;
use thiserror::Error;

/// Failures the identity module can report.
#[derive(Debug, Error)]
pub enum AuthError {
    /// One or more request fields failed validation.
    #[error("request validation failed")]
    Validation(Vec<FieldError>),

    /// The email is already registered by another account.
    #[error("email is already registered")]
    EmailTaken,

    /// The username is already taken by another account.
    #[error("username is already taken")]
    UsernameTaken,

    /// The email/password pair did not match. The variant does not say *which*
    /// half was wrong, so a caller cannot turn it into account enumeration.
    #[error("invalid email or password")]
    InvalidCredentials,

    /// The access token is missing, malformed, expired, or its session was
    /// revoked or is past its own expiry.
    #[error("missing, expired or revoked credentials")]
    Unauthenticated,

    /// The Argon2 parameters were rejected by the library.
    #[error("password hashing parameters are invalid")]
    Params(#[source] argon2::Error),

    /// Hashing or verifying a password failed.
    #[error("password hashing failed")]
    Hash(#[source] argon2::password_hash::Error),

    /// A token could not be encoded or decoded/verified.
    #[error("access token could not be issued or verified")]
    Token(#[source] jsonwebtoken::errors::Error),

    /// A database operation failed.
    #[error("identity storage failed")]
    Database(#[source] sqlx::Error),

    /// Configuration is unusable (for example a too-short signing secret).
    #[error("identity is misconfigured: {0}")]
    Config(&'static str),

    /// The OS randomness source failed.
    #[error("failed to gather OS randomness")]
    Random(#[source] getrandom::Error),

    /// The system clock could not be read.
    #[error("system clock error: {0}")]
    Clock(&'static str),

    /// A blocking hashing worker could not run to completion.
    #[error("a password hashing worker failed")]
    Worker,
}
