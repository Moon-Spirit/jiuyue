//! Shared application state.
//!
//! [`AppState`] is the single value every handler receives through
//! [`axum::extract::State`]. It carries the configuration, the process start time,
//! and the domain services an instance runs with.
//!
//! Every service is optional so the router can be built in tests (and so
//! `/health` keeps working) without a database. A handler that needs one asks
//! through [`AppState::auth`] / [`AppState::chat`] / [`AppState::realtime`] and
//! gets [`ServiceUnavailable`], which the HTTP layer turns into `503`, rather than
//! a panic.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use jiuyue_auth::AuthService;
use jiuyue_chat::ChatService;
use jiuyue_realtime::RealtimeHub;
use thiserror::Error;

use crate::config::Config;

/// Process start marker, initialised on first access.
///
/// Uptime is a process property, not a per-state property: every `AppState`
/// built in a process must report the same origin.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Returned when an endpoint needs a subsystem this instance does not run.
#[derive(Debug, Error)]
#[error("the {service} subsystem is not configured on this instance")]
pub struct ServiceUnavailable {
    /// Human-readable name of the missing subsystem (Chinese, for the client).
    pub service: &'static str,
}

/// The domain services an instance runs with, as one unit.
///
/// Bundling them keeps [`AppState::with_services`] a two-argument call and makes
/// "an instance either has all three or none" explicit.
#[derive(Clone)]
pub struct Services {
    /// Identity: register, log in, resolve access tokens.
    pub auth: Arc<AuthService>,
    /// Chat: Conversations and Messages.
    pub chat: Arc<ChatService>,
    /// Realtime: the live-connection registry and the socket path.
    pub realtime: Arc<RealtimeHub>,
}

/// State shared by every request handler.
#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    started_at: Instant,
    auth: Option<Arc<AuthService>>,
    chat: Option<Arc<ChatService>>,
    realtime: Option<Arc<RealtimeHub>>,
}

impl AppState {
    /// Build state from configuration, anchoring uptime to process start.
    ///
    /// No domain service is present; use [`AppState::with_services`] once the
    /// database is reachable.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            started_at: *PROCESS_START.get_or_init(Instant::now),
            auth: None,
            chat: None,
            realtime: None,
        }
    }

    /// Build state that can serve identity endpoints but nothing else.
    pub fn with_auth(config: Config, auth: AuthService) -> Self {
        Self {
            auth: Some(Arc::new(auth)),
            ..Self::new(config)
        }
    }

    /// Build state that can serve every endpoint.
    pub fn with_services(config: Config, services: Services) -> Self {
        Self {
            auth: Some(services.auth),
            chat: Some(services.chat),
            realtime: Some(services.realtime),
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

    /// The identity service, or [`ServiceUnavailable`] on a database-less instance.
    pub fn auth(&self) -> Result<Arc<AuthService>, ServiceUnavailable> {
        self.auth
            .clone()
            .ok_or(ServiceUnavailable { service: "身份" })
    }

    /// The chat service, or [`ServiceUnavailable`] on a database-less instance.
    pub fn chat(&self) -> Result<Arc<ChatService>, ServiceUnavailable> {
        self.chat
            .clone()
            .ok_or(ServiceUnavailable { service: "会话" })
    }

    /// The realtime hub, or [`ServiceUnavailable`] on a database-less instance.
    pub fn realtime(&self) -> Result<Arc<RealtimeHub>, ServiceUnavailable> {
        self.realtime
            .clone()
            .ok_or(ServiceUnavailable { service: "实时" })
    }
}
