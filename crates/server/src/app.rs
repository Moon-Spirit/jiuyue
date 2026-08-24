//! Router assembly — pure routing + state, no server binding, so tests can
//! drive it with `tower::ServiceExt::oneshot`.

use crate::auth;
use crate::state::AppState;
use axum::routing::get;
use axum::Router;

async fn healthz() -> &'static str {
    "ok"
}

/// Full application router: `/healthz` + `/api/auth/*`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .nest("/api/auth", auth::router())
        .with_state(state)
}
