//! REST handlers for the chat module.
//!
//! Mounted under `/conversations`, which the frontend reaches as
//! `/api/conversations`. Sending a Message is *not* here: it travels over the
//! realtime socket (see `jiuyue-realtime`), because the acknowledgement and the
//! fan-out are the same round trip. These routes cover what a socket should not:
//! opening a Conversation (idempotent by construction), listing the caller's
//! Conversations, and paging through history.
//!
//! # History pagination
//!
//! `GET /conversations/{id}/messages` reads one **page** of history with a cursor
//! on the Conversation's Sequence Number — the ordering authority (ADR-0003):
//!
//! - `before` (optional, exclusive): return Messages with `seq < before`. Omit it
//!   for the newest page; pass the previous response's `next_before` for the page
//!   before that.
//! - `limit` (optional): page size, clamped to `[1, MAX_MESSAGE_PAGE_SIZE]` and
//!   defaulting to `DEFAULT_MESSAGE_PAGE_SIZE`.
//!
//! The response always lists `messages` ascending by `seq` plus `next_before` and
//! `has_more`. Because the cursor is anchored to a `seq` that never moves, a page
//! boundary is stable while new Messages arrive — the failure mode of
//! `LIMIT/OFFSET`, where a concurrent insert shifts every later page.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use jiuyue_contract::{
    ConversationCreated, ConversationList, ConversationSummary, CreateDirectConversationRequest,
    MessageList, MessagePageQuery, ServerEvent,
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

/// `GET /conversations/{id}/messages` — one page of history, oldest first.
///
/// See the module docs for the cursor contract. A non-Participant is refused with
/// `403 NOT_A_PARTICIPANT`; a cursor that names no Message is not an error — it
/// simply yields an empty or most-recent page.
async fn list_messages(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(conversation_id): Path<String>,
    Query(page): Query<MessagePageQuery>,
) -> Result<Json<MessageList>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let page = state
        .chat()?
        .list_messages(&session.user_id, &conversation_id, page)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(page))
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
