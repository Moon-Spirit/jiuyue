//! Router assembly 鈥?pure routing + state, no server binding, so tests can
//! drive it with `tower::ServiceExt::oneshot` (HTTP) or a real listener (WS).

use crate::state::AppState;
use crate::{auth, chat, e2ee, ws};
use axum::routing::get;
use axum::Router;

async fn healthz() -> &'static str {
    "ok"
}

/// Full application router: `/healthz` + `/api/auth/*` + `/api/conversations`
/// + `/api/e2ee/*` + `/ws`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .nest("/api/auth", auth::router())
        .nest("/api/e2ee", e2ee::router())
        .route(
            "/api/conversations",
            get(chat::list_conversations).post(chat::create_direct),
        )
        .route("/ws", get(ws::ws_handler))
        .with_state(state)
}
