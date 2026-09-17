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

    /// Too many failed attempts from this source recently; the caller must wait.
    /// The wait travels as data ([`jiuyue_contract::ErrorDetail::retry_after_seconds`])
    /// rather than as prose the client would have to parse.
    #[error("too many login attempts, retry in {retry_after_seconds}s")]
    TooManyAttempts {
        /// Length of the imposed backoff, in whole seconds.
        retry_after_seconds: u64,
    },

    /// Repeated throttling from this source escalated to a lockout.
    #[error("login source locked out, retry in {retry_after_seconds}s")]
    LockedOut {
        /// Length of the imposed penalty, in whole seconds.
        retry_after_seconds: u64,
    },

    /// The account exists but its email has not been verified, and the action is
    /// gated on verification. Distinct so the HTTP layer can answer with a code
    /// the client turns into "open your inbox / resend the link".
    #[error("email is not verified")]
    EmailNotVerified,

    /// A one-shot link is well-formed but past its lifetime. Kept separate from
    /// [`Self::TokenInvalid`] because "request a new one" is actionable advice.
    #[error("the link has expired")]
    TokenExpired,

    /// A one-shot link is unknown, malformed, or already spent. "Used" and "never
    /// existed" are deliberately the same answer: both mean start over.
    #[error("the link is invalid or already used")]
    TokenInvalid,

    /// A token was redeemed successfully but the account it named is gone. Only
    /// reachable through a race with account deletion; reported honestly rather
    /// than as a bad credential.
    #[error("the account no longer exists")]
    AccountMissing,

    /// A third-party sign-in used an address that already belongs to another
    /// account.
    ///
    /// This is the account-take-over refusal. Linking the provider automatically
    /// would let anyone who can make a provider assert an email address walk into
    /// the account that address belongs to — a provider with unverified addresses,
    /// a compromised provider account, or a misconfigured tenant all produce one.
    /// The provider identity is a credential *for* an account; it is never a way
    /// to claim one by email.
    #[error("an account already uses that address")]
    AccountExists,

    /// The provider identity is already bound to a different account.
    #[error("that provider identity is already linked to another account")]
    OAuthIdentityTaken,

    /// The `state` was unknown, already spent, past its lifetime, or belonged to
    /// another provider. Never proceed: this is the login-CSRF refusal.
    #[error("the OAuth round trip did not originate here, or has expired")]
    OAuthStateInvalid,

    /// The provider itself refused or was unreachable. The detail is for the log,
    /// never for the client: it is the provider's vocabulary and may echo a secret.
    #[error("the identity provider could not complete the sign-in: {0}")]
    OAuthProviderError(String),

    /// A provider was used that this instance has no credentials for. Normally
    /// unreachable, because an unconfigured provider is not offered — refused
    /// explicitly rather than trusting the client to only ask for what it saw.
    #[error("that sign-in provider is not configured on this instance")]
    OAuthNotConfigured,

    /// The account exists from a provider sign-in but has not chosen a username
    /// yet, and the action needs a finished account.
    #[error("this account must choose a username before it can be used")]
    UsernameRequired,

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
