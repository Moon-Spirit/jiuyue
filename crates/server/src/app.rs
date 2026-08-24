//! Router assembly 鈥?pure routing + state, no server binding, so tests can
//! drive it with `tower::ServiceExt::oneshot` (HTTP) or a real listener (WS).

use crate::state::AppState;
use crate::{auth, chat, e2ee, friends, push, ws};
use axum::routing::get;
use axum::Router;

async fn healthz() -> &'static str {
    "ok"
}

/// Full application router: `/healthz` + `/api/auth/*` + `/api/conversations`
/// + `/api/e2ee/*` + `/api/friends/*` + `/ws` (dev builds add `/api/dev/push-log`).
pub fn build_router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .nest("/api/auth", auth::router())
        .nest("/api/e2ee", e2ee::router())
        .nest("/api/friends", friends::router())
        .route(
            "/api/conversations",
            get(chat::list_conversations).post(chat::create_direct),
        )
        .route("/ws", get(ws::ws_handler));

    // M4 debug surface for offline-push evidence. Dev-profile only
    // (`cfg!(debug_assertions)`): release builds never register the route.
    #[cfg(debug_assertions)]
    {
        router = router.route("/api/dev/push-log", get(push::push_log));
    }

    router.with_state(state)
}
