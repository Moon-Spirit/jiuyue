//! jiuyue identity.
//!
//! This crate owns everything about *who a request is*: password hashing
//! (Argon2id), access-token issuance and verification (HS256 JWT), refresh-token
//! storage, the `sessions` table that makes logout immediate, and the one-shot
//! email links that verify an address or reset a forgotten password.
//!
//! # Why a session row, when the token is already signed
//!
//! A signed JWT is self-contained, which is exactly the problem: nothing in the
//! token can be un-said before it expires. "After logging out a protected
//! endpoint is no longer reachable" is an acceptance criterion, so a self-
//! sufficient token is not enough. The access token therefore carries a session
//! id (`sid`) and [`AuthService::authenticate`] looks that session up on every
//! request; [`AuthService::logout`] sets `revoked_at`, and the next request
//! fails. There is deliberately no lookup cache — on one node a primary-key
//! probe is cheap, and a cache would reintroduce revocation lag.
//!
//! # Email links
//!
//! Verification and password-reset links are opaque random tokens; only their
//! SHA-256 digests are stored (`account_tokens`, see [`AccountTokenRepository`]).
//! They expire, and redemption is a single atomic UPDATE that spends the token
//! before the caller does anything else. Delivery goes through the [`Mailer`]
//! seam: [`InMemoryMailer`] for development and tests, a provider-backed
//! implementation for production. See [`mailer`] for what a deployment must
//! supply, and `docs/adr/0015-email-verification-and-the-mailer-seam.md` for the
//! full decision.
//!
//! # Third-party sign-in
//!
//! GitHub and Google sign-in live in [`oauth`]. What must be understood about
//! them before touching either is one sentence: **a provider identity is a
//! credential for an account, never a way to claim one by email.** A provider
//! address that matches an existing account is refused, not linked; the full
//! reasoning is in that module's documentation and in ADR-0016.
//!
//! # Cost control on a 2 vCPU / 2 GB box
//!
//! Argon2id at OWASP parameters allocates ~19 MiB per in-flight hash, so an
//! unbounded burst of logins could exhaust memory. [`PasswordHasher`] bounds
//! concurrent hashes with a semaphore and runs them on the blocking pool. The
//! endpoints that trigger email share the same rate limiter as login, keyed on
//! the caller's source, so neither can be turned into a spam cannon.

#![forbid(unsafe_code)]

mod account_tokens;
mod error;
pub mod limiter;
mod links;
pub mod mailer;
pub mod oauth;
mod password;
mod repository;
mod service;
mod token;
pub mod validation;

pub use account_tokens::{AccountTokenRepository, NewToken, StoredToken, TokenPurpose};
pub use error::AuthError;
pub use limiter::{
    InProcessLoginAttemptStore, LoginAttempt, LoginAttemptPolicy, LoginAttemptPolicyBuilder,
    LoginAttemptStore, MAX_LOCKOUT_SECS,
};
pub use mailer::{InMemoryMailer, MailError, Mailer, OutgoingMessage};
pub use oauth::{
    CallbackOutcome, GitHubAdapter, GoogleAdapter, HttpOAuthClient, OAuthClient, OAuthConfig,
    OAuthEndpoint, OAuthFuture, OAuthIdentity, OAuthProviderAdapter, OAuthProviderConfig,
    OAuthProviders, OAuthRepository, OAuthRequest, OAuthResponse, OAuthService, PkcePair,
    adapter_for, session_outcome,
};
pub use password::PasswordHasher;
pub use service::{
    AuthConfig, AuthService, AuthServiceBuilder, AuthenticatedSession, SessionContext,
    UNKNOWN_SOURCE,
};
pub use token::{AccessClaims, TokenIssuer, hash_token, issue_opaque_token};
