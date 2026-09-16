//! jiuyue identity.
//!
//! This crate owns everything about *who a request is*: password hashing
//! (Argon2id), access-token issuance and verification (HS256 JWT), refresh-token
//! storage, and the `sessions` table that makes logout immediate.
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
//! # What this crate does not do
//!
//! Email verification, login rate limiting and refresh-token rotation are
//! separate tickets. Nothing here delivers mail, throttles attempts, or rotates
//! refresh tokens; the schema and [`AuthService`] are shaped so those can be
//! added without replacing this module.
//!
//! # Cost control on a 2 vCPU / 2 GB box
//!
//! Argon2id at OWASP parameters allocates ~19 MiB per in-flight hash, so an
//! unbounded burst of logins could exhaust memory. [`PasswordHasher`] bounds
//! concurrent hashes with a semaphore and runs them on the blocking pool.

#![forbid(unsafe_code)]

mod error;
mod password;
mod repository;
mod service;
mod token;
pub mod validation;

pub use error::AuthError;
pub use password::PasswordHasher;
pub use service::{AuthConfig, AuthService, AuthenticatedSession, SessionContext};
pub use token::AccessClaims;
