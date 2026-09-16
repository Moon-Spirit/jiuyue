//! Shared application state.
//!
//! [`AppState`] is the single value every handler receives through
//! [`axum::extract::State`]. It stays small on purpose, but the shape is the one
//! that survives growth: later tickets add a database pool and presence handle
//! here without touching handler signatures.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::config::Config;

/// Process start marker, initialised on first access.
///
/// Uptime is a process property, not a per-state property: every `AppState`
/// built in a process must report the same origin.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// State shared by every request handler.
#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    started_at: Instant,
}

impl AppState {
    /// Build state from configuration, anchoring uptime to process start.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            started_at: *PROCESS_START.get_or_init(Instant::now),
        }
    }

    /// The effective configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Time elapsed since the process started.
    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }
}
