//! Shared application state.
//!
//! [`AppState`] is the single value every handler receives through
//! [`axum::extract::State`]. It carries the configuration, the process start
//! time, and — once the server has reached the database — the identity service.
//!
//! The identity service is optional so the router can be built in tests (and so
//! `/health` and `/ws` keep working) without a database. A handler that needs it
//! asks through [`AppState::auth`] and gets [`AuthUnavailable`], which the HTTP
//! layer turns into `503`, rather than a panic.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use jiuyue_auth::AuthService;
use thiserror::Error;

use crate::config::Config;

/// Process start marker, initialised on first access.
///
/// Uptime is a process property, not a per-state property: every `AppState`
/// built in a process must report the same origin.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Returned when an endpoint needs identity but this instance has no database.
#[derive(Debug, Error)]
#[error("identity is not configured on this instance")]
pub struct AuthUnavailable;

/// State shared by every request handler.
#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    started_at: Instant,
    auth: Option<Arc<AuthService>>,
}

impl AppState {
    /// Build state from configuration, anchoring uptime to process start.
    ///
    /// The identity service is absent; use [`AppState::with_auth`] once the
    /// database is reachable.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            started_at: *PROCESS_START.get_or_init(Instant::now),
            auth: None,
        }
    }

    /// Build state that can serve identity endpoints.
    pub fn with_auth(config: Config, auth: AuthService) -> Self {
        Self {
            auth: Some(Arc::new(auth)),
            ..Self::new(config)
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

    /// The identity service, or [`AuthUnavailable`] on a database-less instance.
    pub fn auth(&self) -> Result<&AuthService, AuthUnavailable> {
        self.auth.as_deref().ok_or(AuthUnavailable)
    }
}
