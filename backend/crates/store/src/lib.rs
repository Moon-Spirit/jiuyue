//! jiuyue persistence layer.
//!
//! This crate owns the PostgreSQL connection pool and the embedded database
//! migrations. It is the only place in the backend that talks SQL: other crates
//! depend on `jiuyue-store` rather than reaching into PostgreSQL themselves, so
//! the schema and the queries that use it stay in one module.
//!
//! The public API is deliberately tiny — connect, migrate, hand out the pool:
//!
//! ```no_run
//! use jiuyue_store::Store;
//!
//! # async fn example() -> Result<(), jiuyue_store::StoreError> {
//! let store = Store::connect("postgres://user:password@localhost:5432/jiuyue").await?;
//! store.migrate().await?;
//! let _pool = store.pool();
//! # Ok(())
//! # }
//! ```
//!
//! Migrations live in `backend/migrations/` and are embedded into the binary at
//! compile time, so a deployed server can migrate itself at boot without the
//! source tree. See `README.md` for the workflow.

#![forbid(unsafe_code)]

mod error;
mod store;

pub use error::StoreError;
pub use store::Store;
