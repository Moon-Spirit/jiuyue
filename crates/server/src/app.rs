//! Router assembly 鈥?pure routing + state, no server binding, so tests can
//! drive it with `tower::ServiceExt::oneshot` (HTTP) or a real listener (WS).

use crate::state::AppState;
use crate::{auth, chat, e2ee, friends, groups, media, push, users, ws};
use axum::Router;
use axum::routing::{get, post};

async fn healthz() -> &'static str {
    "ok"
}

/// Full application router: `/healthz`, `/api/auth/*`, `/api/conversations`,
/// `/api/e2ee/*`, `/api/friends/*`, `/api/media/*`, `/ws` (dev builds add
/// `/api/dev/push-log`).
pub fn build_router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .nest("/api/auth", auth::router())
        .nest("/api/e2ee", e2ee::router())
        .nest("/api/friends", friends::router())
        .nest("/api/groups", groups::router())
        .nest("/api/users", users::router())
        .route(
            "/api/conversations",
            get(chat::list_conversations).post(chat::create_direct),
        )
        .route("/api/media", post(media::upload))
        .route("/api/media/{id}", get(media::serve))
        .route("/ws", get(ws::ws_handler));

    // M4 debug surface for offline-push evidence. Dev-profile only
    // (`cfg!(debug_assertions)`): release builds never register the route.
    #[cfg(debug_assertions)]
    {
        router = router.route("/api/dev/push-log", get(push::push_log));
    }

    router.with_state(state)
}
