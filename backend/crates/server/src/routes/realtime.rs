//! The authenticated realtime route.
//!
//! `GET /ws` is the single socket endpoint. It authenticates **before** the
//! upgrade and refuses an unauthenticated request with `401` and the contract's
//! error body — there is no path that serves an anonymous socket.
//!
//! # Why the token is in the query string
//!
//! The browser's `WebSocket` constructor cannot set an `Authorization` header, so
//! the short-lived access token travels as `?token=…`. TLS is terminated by Caddy
//! and the token expires on its own; a dedicated socket-ticket credential is a
//! later refinement, not a prerequisite for the tracer bullet.

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::response::Response;
use jiuyue_realtime::{MAX_FRAME_SIZE, MAX_MESSAGE_SIZE};
use serde::Deserialize;

use super::api::ApiError;
use crate::state::AppState;

/// Query parameters of `GET /ws`.
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// The access token to bind the connection to a User.
    token: Option<String>,
}

/// `GET /ws` — authenticate, then upgrade.
pub async fn upgrade(
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let token = query
        .token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(ApiError::unauthenticated)?;

    let session = state
        .auth()?
        .authenticate(token.trim())
        .await
        .map_err(ApiError::from)?;
    let hub = state.realtime()?;

    Ok(ws
        .max_frame_size(MAX_FRAME_SIZE)
        .max_message_size(MAX_MESSAGE_SIZE)
        .write_buffer_size(MAX_FRAME_SIZE)
        .max_write_buffer_size(MAX_FRAME_SIZE * 2)
        .on_upgrade(move |socket| jiuyue_realtime::serve_connection(socket, session.user_id, hub)))
}
