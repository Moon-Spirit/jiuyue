//! JiuYue server library.
//!
//! Split out of `main.rs` so integration tests can build the same router
//! (`build_router`) without binding a port.

pub mod app;
pub mod auth;
pub mod chat;
pub mod code_store;
pub mod config;
pub mod crypto;
pub mod e2ee;
pub mod error;
pub mod friends;
pub mod groups;
pub mod media;
pub mod profile;
pub mod push;
pub mod state;
pub mod users;
pub mod ws;

pub use app::build_router;

/// Embedded migrations (repo-root `migrations/`).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
