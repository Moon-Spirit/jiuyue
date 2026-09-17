//! Integration tests for the identity endpoints.
//!
//! These run against a **real PostgreSQL** and the **real Axum router** — no
//! mocks. The unique constraints, the Argon2id round trip and the session
//! revocation query are the things under test, and none of them exist in a mock.
//!
//! One test binary split into modules: `support` holds the harness, `flow` the
//! end-to-end journeys, `validation` the rejection shapes, `tokens` the access
//! token failures, `limiting` the login rate limiter, `email` the verification
//! and password-reset journeys. Each test creates its own schema, so the suite
//! runs in parallel and repeatedly without cleanup.

mod email;
mod flow;
mod limiting;
mod oauth;
mod oauth_support;
mod support;
mod tokens;
mod validation;
