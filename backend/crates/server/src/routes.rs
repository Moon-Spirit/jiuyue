//! HTTP routing and handlers.

use std::time::Instant;

use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

/// Payload returned by `GET /health`.
///
/// The shape is part of the contract, so the type stays `serde`-serialisable and
/// honest about field types: `uptime_s` is a whole number of seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Health {
    /// Always `"ok"` while the process is serving traffic.
    pub status: &'static str,
    /// Crate version compiled into the binary.
    pub version: &'static str,
    /// Whole seconds since the process started.
    pub uptime_s: u64,
}

/// Build the application router with its routes, middleware and state attached.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        // Realtime lives in its own crate; the server only wires the route. The
        // upgrade handler is stateless, so it takes no state out of this crate.
        .route("/ws", get(jiuyue_realtime::ws_handler))
        .layer(middleware::from_fn(log_request))
        .with_state(state)
}

/// `GET /health` — liveness probe.
async fn health(State(state): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_s: state.uptime().as_secs(),
    })
}

/// Emit exactly one log line per request: method, path, status and latency.
async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();

    let response = next.run(request).await;

    tracing::info!(
        %method,
        %path,
        status = response.status().as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        "request",
    );

    response
}
