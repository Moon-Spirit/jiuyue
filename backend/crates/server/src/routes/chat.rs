//! REST handlers for the chat module.
//!
//! Mounted under `/conversations`, which the frontend reaches as
//! `/api/conversations`. Sending a Message is *not* here: it travels over the
//! realtime socket (see `jiuyue-realtime`), because the acknowledgement and the
//! fan-out are the same round trip. These routes cover what a socket should not:
//! opening a Conversation (idempotent by construction), listing the caller's
//! Conversations, and reading a bounded window of history.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use jiuyue_contract::{
    ConversationCreated, ConversationList, ConversationSummary, CreateDirectConversationRequest,
    MessageList, ServerEvent,
};

use super::api::{ApiError, bearer_token};
use crate::state::AppState;

/// The `/conversations` routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/conversations", get(list_conversations))
        .route("/conversations/direct", post(create_direct))
        .route(
            "/conversations/{conversation_id}/messages",
            get(list_messages),
        )
}

/// `POST /conversations/direct` — open (or reopen) the Direct Conversation with a peer.
///
/// Idempotent: the second call returns the same Conversation, and the peer's live
/// sessions are greeted with [`ConversationCreated`] only on the call that actually
/// created it.
async fn create_direct(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateDirectConversationRequest>,
) -> Result<(StatusCode, Json<ConversationSummary>), ApiError> {
    let session = authenticate(&state, &headers).await?;
    let opened = state
        .chat()?
        .open_direct(&session.user_id, &body.peer_username)
        .await
        .map_err(ApiError::from)?;

    if opened.created {
        let hub = state.realtime()?;

        // Each Participant's view names the *other* party as the peer, so the
        // notices are per-recipient rather than one shared event.
        for notice in &opened.notices {
            hub.registry()
                .deliver(
                    std::slice::from_ref(&notice.user_id),
                    &ServerEvent::ConversationCreated(ConversationCreated {
                        conversation: notice.conversation.clone(),
                    }),
                )
                .await;
        }
    }

    Ok((StatusCode::OK, Json(opened.summary)))
}

/// `GET /conversations` — the caller's Conversations, newest first.
async fn list_conversations(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<ConversationList>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let conversations = state
        .chat()?
        .list_conversations(&session.user_id)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(ConversationList { conversations }))
}

/// `GET /conversations/{id}/messages` — a bounded window of history, oldest first.
async fn list_messages(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(conversation_id): Path<String>,
) -> Result<Json<MessageList>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let messages = state
        .chat()?
        .list_messages(&session.user_id, &conversation_id)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(MessageList { messages }))
}

/// Resolve an authorization header to the caller, or fail with `401`.
async fn authenticate(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<jiuyue_auth::AuthenticatedSession, ApiError> {
    let token = bearer_token(headers)?;
    state
        .auth()?
        .authenticate(&token)
        .await
        .map_err(ApiError::from)
}
