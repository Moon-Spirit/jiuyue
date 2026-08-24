//! Router assembly — pure routing + state, no server binding, so tests can
//! drive it with `tower::ServiceExt::oneshot` (HTTP) or a real listener (WS).

use crate::state::AppState;
use crate::{auth, chat, ws};
use axum::routing::{get, post};
use axum::Router;

async fn healthz() -> &'static str {
    "ok"
}

/// Full application router: `/healthz` + `/api/auth/*` + `/api/conversations`
/// + `/ws`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .nest("/api/auth", auth::router())
        .route("/api/conversations", post(chat::create_direct))
        .route("/ws", get(ws::ws_handler))
        .with_state(state)
}
