//! jiuyue backend server.
//!
//! The crate is deliberately split into a library and a thin binary so that
//! integration tests can build the router in process, without binding a socket.
//! Everything the binary assembles (configuration, state, routing) is reachable
//! through this crate's public API.

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod routes;
pub mod state;

pub use config::Config;
pub use error::Error;
pub use routes::{Health, app};
pub use state::{AppState, ServiceUnavailable, Services};
