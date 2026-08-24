//! HTTP handlers for `/api/auth/*`.

pub mod extract;
pub mod handlers;
pub mod jwt;
pub mod password;
pub mod tokens;
pub mod ws_ticket;

use axum::routing::post;
use axum::Router;

pub fn router() -> Router<crate::state::AppState> {
    Router::new()
        .route("/request-code", post(handlers::request_code))
        .route("/register", post(handlers::register))
        .route("/login", post(handlers::login))
        .route("/refresh", post(handlers::refresh))
        .route("/ws-ticket", post(handlers::ws_ticket))
}
