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
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use jiuyue_chat::{MembershipNotice, MembershipUpdate};
use jiuyue_contract::{
    AddGroupMembersRequest, ChangeMemberRoleRequest, ConversationCreated, ConversationList,
    ConversationSummary, CreateDirectConversationRequest, CreateGroupConversationRequest,
    GroupInfo, MembershipChanged, MessageList, MessagePageQuery, ServerEvent,
    TransferOwnershipRequest,
};

use super::api::{ApiError, bearer_token};
use crate::state::AppState;

/// The `/conversations` routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/conversations", get(list_conversations))
        .route("/conversations/direct", post(create_direct))
        .route("/conversations/group", post(create_group))
        .route(
            "/conversations/{conversation_id}",
            get(conversation_detail).delete(dissolve_group),
        )
        .route(
            "/conversations/{conversation_id}/messages",
            get(list_messages),
        )
        .route(
            "/conversations/{conversation_id}/members",
            post(add_group_members),
        )
        .route(
            "/conversations/{conversation_id}/members/{member_id}",
            patch(change_member_role).delete(remove_group_member),
        )
        .route("/conversations/{conversation_id}/leave", post(leave_group))
        .route(
            "/conversations/{conversation_id}/transfer",
            post(transfer_ownership),
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

/// `POST /conversations/group` — create a Group Conversation with the caller as owner.
///
/// Every invited member is greeted with their own [`ConversationCreated`] (their
/// view carries **their** Role), which is how the group appears in each of their
/// conversation lists without a refresh.
async fn create_group(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateGroupConversationRequest>,
) -> Result<(StatusCode, Json<ConversationSummary>), ApiError> {
    let session = authenticate(&state, &headers).await?;
    let opened = state
        .chat()?
        .create_group(&session.user_id, body)
        .await
        .map_err(ApiError::from)?;

    if opened.created {
        let hub = state.realtime()?;
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

    Ok((StatusCode::CREATED, Json(opened.summary)))
}

/// `GET /conversations/{id}` — the Group info panel's data, or the Direct summary.
///
/// A Group answer carries the member list with Roles; a Direct Conversation is
/// refused with `409 CONFLICT` (`NotAGroup`) because it has no members to show.
async fn conversation_detail(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(conversation_id): Path<String>,
) -> Result<Json<GroupInfo>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let info = state
        .chat()?
        .group_info(&session.user_id, &conversation_id)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(info))
}

/// `POST /conversations/{id}/members` — invite Users into a Group.
///
/// Returns the refreshed Group info, so the caller's member panel is authoritative
/// rather than a local guess. Every current Participant is told who joined.
async fn add_group_members(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(conversation_id): Path<String>,
    Json(body): Json<AddGroupMembersRequest>,
) -> Result<Json<GroupInfo>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let chat = state.chat()?;

    let update = chat
        .add_group_members(&session.user_id, &conversation_id, body)
        .await
        .map_err(ApiError::from)?;
    fan_out_membership(&state, &update).await?;

    let info = chat
        .group_info(&session.user_id, &conversation_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(info))
}

/// `DELETE /conversations/{id}/members/{member_id}` — remove a Participant.
///
/// The removed User is told about their own removal; from then on they are not in
/// the Group's message fan-out and cannot post.
async fn remove_group_member(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path((conversation_id, member_id)): Path<(String, String)>,
) -> Result<Json<GroupInfo>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let chat = state.chat()?;

    let update = chat
        .remove_group_member(&session.user_id, &conversation_id, &member_id)
        .await
        .map_err(ApiError::from)?;
    fan_out_membership(&state, &update).await?;

    let info = chat
        .group_info(&session.user_id, &conversation_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(info))
}

/// `PATCH /conversations/{id}/members/{member_id}` — promote or demote an admin.
async fn change_member_role(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path((conversation_id, member_id)): Path<(String, String)>,
    Json(body): Json<ChangeMemberRoleRequest>,
) -> Result<Json<GroupInfo>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let chat = state.chat()?;

    let update = chat
        .change_member_role(&session.user_id, &conversation_id, &member_id, body)
        .await
        .map_err(ApiError::from)?;
    fan_out_membership(&state, &update).await?;

    let info = chat
        .group_info(&session.user_id, &conversation_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(info))
}

/// `POST /conversations/{id}/leave` — leave a Group of one's own accord.
///
/// Answers `204`: the caller has no standing left to read the Group's info from,
/// and the remaining Participants were told by the fan-out.
async fn leave_group(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(conversation_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let session = authenticate(&state, &headers).await?;

    let update = state
        .chat()?
        .leave_group(&session.user_id, &conversation_id)
        .await
        .map_err(ApiError::from)?;
    fan_out_membership(&state, &update).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /conversations/{id}/transfer` — hand ownership to another Participant.
async fn transfer_ownership(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(conversation_id): Path<String>,
    Json(body): Json<TransferOwnershipRequest>,
) -> Result<Json<GroupInfo>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let chat = state.chat()?;

    let update = chat
        .transfer_group_ownership(&session.user_id, &conversation_id, body)
        .await
        .map_err(ApiError::from)?;
    fan_out_membership(&state, &update).await?;

    let info = chat
        .group_info(&session.user_id, &conversation_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(info))
}

/// `DELETE /conversations/{id}` — dissolve a Group for everyone.
///
/// Answers `204`: there is no Conversation left to return. Every former
/// Participant was told by the fan-out.
async fn dissolve_group(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(conversation_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let session = authenticate(&state, &headers).await?;

    let update = state
        .chat()?
        .dissolve_group(&session.user_id, &conversation_id)
        .await
        .map_err(ApiError::from)?;
    fan_out_membership(&state, &update).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Push one membership mutation to exactly the Users the domain named.
///
/// This is the seam between the domain's decision (who should see what) and the
/// transport (how it reaches them): the registry fans out to every live Device of
/// each listed User, and a User the domain left out of the list receives nothing —
/// which is what makes "a removed member stops receiving group traffic" true at
/// the fan-out, not merely by client-side politeness.
async fn fan_out_membership(state: &AppState, update: &MembershipUpdate) -> Result<(), ApiError> {
    let hub = state.realtime()?;

    for notice in &update.notices {
        match notice {
            MembershipNotice::Conversation {
                user_id,
                conversation,
            } => {
                hub.registry()
                    .deliver(
                        std::slice::from_ref(user_id),
                        &ServerEvent::ConversationCreated(ConversationCreated {
                            conversation: conversation.clone(),
                        }),
                    )
                    .await;
            }
            MembershipNotice::Changed { user_ids, change } => {
                hub.registry()
                    .deliver(
                        user_ids,
                        &ServerEvent::MembershipChanged(MembershipChanged {
                            conversation_id: update.conversation_id.clone(),
                            actor_id: update.actor_id.clone(),
                            change: change.clone(),
                        }),
                    )
                    .await;
            }
        }
    }

    Ok(())
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
