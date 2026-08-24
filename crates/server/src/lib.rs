//! JiuYue server library.
//!
//! Split out of `main.rs` so integration tests can build the same router
//! (`build_router`) without binding a port.

pub mod app;
pub mod auth;
pub mod code_store;
pub mod config;
pub mod error;
pub mod state;

pub use app::build_router;

/// Embedded migrations (repo-root `migrations/`).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
