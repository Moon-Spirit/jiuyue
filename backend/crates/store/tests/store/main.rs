//! Integration tests for `jiuyue-store` against a real PostgreSQL.
//!
//! One test binary, split into modules: `support` holds the shared harness,
//! `migrations` covers the migration framework itself, `users` covers the first
//! schema. Keeping it one binary means the harness has no cross-target dead code
//! and needs no lint suppressions.
//!
//! Every test opens its own schema, so the suite runs in parallel and repeatedly
//! without manual cleanup. Without `TEST_DATABASE_URL` (or `DATABASE_URL`) the
//! harness panics with instructions — a silently skipped test would hide schema
//! rot, which is worse than no test at all.

mod migrations;
mod support;
mod users;
