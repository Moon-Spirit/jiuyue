//! M5 friend system HTTP surface (`/api/friends/*`).
//!
//! Friends are an **address-book layer only**: they do NOT gate conversation
//! creation (direct chats stay open per product decision) and accepting a
//! request never auto-creates a conversation.
//!
//! Endpoints (all Bearer-authenticated):
//!
//! | Method | Path                        | Semantics                                                        |
//! |--------|-----------------------------|------------------------------------------------------------------|
//! | POST   | `/requests`                 | send `{username}` → 201; 404 peer_not_found; 400 self_request;   |
//! |        |                             | 409 already_friends \| request_already_pending                   |
//! | GET    | `/requests`                 | `{incoming:[…], outgoing:[…]}` (pending only)                    |
//! | POST   | `/requests/{id}/accept`     | recipient-only, pending-only → 200 `{friend}`; else 404/409      |
//! | POST   | `/requests/{id}/decline`    | recipient-only, pending-only → 204                               |
//! | DELETE | `/requests/{id}`            | sender-only cancel, pending-only → 204                           |
//! | GET    | `/`                         | friend list `[{user_id, username, since}]`                       |
//! | DELETE | `/{userId}`                 | unfriend → removes BOTH rows via shared pair_key → 204           |
//!
//! Duplicate policy: a pending request in EITHER direction counts as a
//! duplicate (`request_already_pending`). A DECLINED row between the pair
//! (either direction) is flipped back to pending on re-send instead of
//! inserting a fresh row. Live peers additionally receive fire-and-forget
//! `friend.requested` / `friend.accepted` registry relays ([`crate::ws`]) —
//! never persisted, never replayed by sync.

use crate::auth::extract::AuthUser;
use crate::error::{AppError, ConflictKind};
use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use jiuyue_protocol::{FriendAccepted, FriendRequested, Payload, UserIdentity};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Friends + requests routes nested under `/api/friends`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/requests", post(send_request).get(list_requests))
        .route("/requests/{id}/accept", post(accept_request))
        .route("/requests/{id}/decline", post(decline_request))
        .route("/requests/{id}", delete(cancel_request))
        .route("/", get(list_friends))
        .route("/{userId}", delete(unfriend))
}

// ---------------------------------------------------------------------------
// Shared shapes
// ---------------------------------------------------------------------------

/// Peer identity block used across responses (`to`, `from`, `friend`).
#[derive(Debug, Serialize)]
pub struct FriendPeer {
    pub user_id: Uuid,
    /// Stable numeric user id (QQ-style); mirrors the peer's `users.uid`.
    pub uid: i64,
    pub username: String,
    /// Effective display handle (stored display_name, or username when empty).
    pub display_name: String,
    /// Curated avatar emoji (possibly `""`).
    pub avatar: String,
}

impl FriendPeer {
    /// Builds a peer block from a DB row, resolving the display-name fallback
    /// server-side (`empty display_name` renders as `username`).
    fn from_row(
        user_id: Uuid,
        uid: i64,
        username: String,
        display_name: String,
        avatar: String,
    ) -> Self {
        let effective = crate::profile::effective_display_name(&display_name, &username);
        Self {
            user_id,
            uid,
            username,
            display_name: effective,
            avatar,
        }
    }
}

/// Lexicographically sorted `"uuidA:uuidB"` — identical from both sides so
/// friendships and their removal key off one value (same convention as
/// `conversations.pair_key`).
fn friend_pair_key(a: Uuid, b: Uuid) -> String {
    let (low, high) = if a.as_bytes() <= b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    };
    format!("{low}:{high}")
}

fn rfc3339(value: OffsetDateTime) -> Result<String, AppError> {
    value.format(&Rfc3339).map_err(AppError::internal)
}

/// Resolves a user's `(username, uid, display_name, avatar)` — the identity
/// block REST responses carry. The DB column is signed `bigint` but only ever
/// holds non-negative values (sequence starts at 100000).
async fn peer_identity_of(
    state: &AppState,
    user_id: Uuid,
) -> Result<(String, i64, String, String), AppError> {
    let row: Option<(String, i64, String, String)> =
        sqlx::query_as("SELECT username, uid, display_name, avatar FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    row.ok_or_else(|| AppError::internal(anyhow::anyhow!("user {user_id} vanished")))
}

/// Widens a DB uid into the protocol wire type (`u64`). Total by construction
/// (uids come from `users_uid_seq` which starts at 100000), but surfaced as a
/// guarded error rather than a panic.
fn wire_uid(user_id: Uuid, uid: i64) -> Result<u64, AppError> {
    u64::try_from(uid).map_err(|_| {
        AppError::internal(anyhow::anyhow!(
            "user {user_id} has an out-of-range uid {uid}"
        ))
    })
}

/// True when ANY friendship row already exists for the unordered pair.
async fn already_friends(tx: &mut sqlx::PgConnection, pair_key: &str) -> Result<bool, AppError> {
    let hit: Option<i64> =
        sqlx::query_scalar("SELECT 1::int8 FROM friendships WHERE pair_key = $1")
            .bind(pair_key)
            .fetch_optional(tx)
            .await
            .map_err(AppError::internal)?;
    Ok(hit.is_some())
}

// ---------------------------------------------------------------------------
// POST /requests — send a friend request
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SendFriendRequestRequest {
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct SendFriendRequestResponse {
    pub request_id: Uuid,
    pub to: FriendPeer,
}

pub async fn send_request(
    State(state): State<AppState>,
    user: AuthUser,
    payload: Result<Json<SendFriendRequestRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<SendFriendRequestResponse>), AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;
    let username = req.username.trim().to_lowercase();
    if username.is_empty() {
        return Err(AppError::BadRequest("username is required".to_owned()));
    }

    // Usernames are public handles: plain 404 on miss (chat.rs convention).
    let peer: Option<(Uuid, i64, String, String, String)> = sqlx::query_as(
        "SELECT id, uid, username, display_name, avatar FROM users WHERE username = $1 LIMIT 1",
    )
    .bind(&username)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;
    let Some((peer_id, peer_uid, peer_username, peer_display, peer_avatar)) = peer else {
        return Err(AppError::PeerNotFound);
    };
    if peer_id == user.0 {
        return Err(AppError::SelfRequest);
    }

    // Needed for the live `friend.requested` relay (response carries only
    // the peer side).
    let (my_username, my_uid, _, _) = peer_identity_of(&state, user.0).await?;
    let pair_key = friend_pair_key(user.0, peer_id);

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;

    if already_friends(&mut tx, &pair_key).await? {
        return Err(AppError::Conflict(ConflictKind::AlreadyFriends));
    }

    // A pending request in EITHER direction is a duplicate.
    let pending: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM friend_requests \
         WHERE status = 'pending' \
           AND ((from_user = $1 AND to_user = $2) OR (from_user = $2 AND to_user = $1)) \
         LIMIT 1",
    )
    .bind(user.0)
    .bind(peer_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AppError::internal)?;
    if pending.is_some() {
        return Err(AppError::Conflict(ConflictKind::RequestAlreadyPending));
    }

    // Re-send path: flip a DECLINED row between the pair (either direction)
    // back to pending instead of stacking history rows. The scalar subquery
    // picks (and locks) one candidate so concurrent re-sends serialize.
    let reused: Option<Uuid> = sqlx::query_scalar(
        "UPDATE friend_requests \
         SET from_user = $1, to_user = $2, status = 'pending', \
             created_at = now(), responded_at = NULL \
         WHERE id = (\
             SELECT id FROM friend_requests \
             WHERE status = 'declined' \
               AND ((from_user = $1 AND to_user = $2) OR (from_user = $2 AND to_user = $1)) \
             ORDER BY created_at DESC LIMIT 1 FOR UPDATE\
         ) \
         RETURNING id",
    )
    .bind(user.0)
    .bind(peer_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AppError::internal)?;

    let request_id = match reused {
        Some(id) => id,
        None => {
            // Fresh request. The partial unique index arbitrates races; a
            // lost race means a pending row appeared concurrently.
            let candidate_id = Uuid::now_v7();
            let inserted: Option<Uuid> = sqlx::query_scalar(
                "INSERT INTO friend_requests (id, from_user, to_user) \
                 VALUES ($1, $2, $3) \
                 ON CONFLICT (from_user, to_user) WHERE status = 'pending' DO NOTHING \
                 RETURNING id",
            )
            .bind(candidate_id)
            .bind(user.0)
            .bind(peer_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(AppError::internal)?;
            inserted.ok_or(AppError::Conflict(ConflictKind::RequestAlreadyPending))?
        }
    };

    tx.commit().await.map_err(AppError::internal)?;

    // Fire-and-forget live notice to the recipient (drop-lag registry relay).
    crate::ws::relay_friend_payload(
        &state,
        peer_id,
        Payload::FriendRequested(FriendRequested {
            request_id,
            from: UserIdentity {
                user_id: user.0,
                username: my_username,
                uid: Some(wire_uid(user.0, my_uid)?),
            },
        }),
    );

    tracing::info!(%request_id, from = %user.0, to = %peer_id, "friend request sent");
    Ok((
        StatusCode::CREATED,
        Json(SendFriendRequestResponse {
            request_id,
            to: FriendPeer::from_row(peer_id, peer_uid, peer_username, peer_display, peer_avatar),
        }),
    ))
}

// ---------------------------------------------------------------------------
// GET /requests — pending inbox + outbox
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct IncomingRequestItem {
    pub request_id: Uuid,
    pub from: FriendPeer,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct OutgoingRequestItem {
    pub request_id: Uuid,
    pub to: FriendPeer,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct ListRequestsResponse {
    pub incoming: Vec<IncomingRequestItem>,
    pub outgoing: Vec<OutgoingRequestItem>,
}

pub async fn list_requests(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<ListRequestsResponse>, AppError> {
    type Row = (Uuid, Uuid, i64, String, String, String, OffsetDateTime);

    let incoming_rows: Vec<Row> = sqlx::query_as(
        "SELECT r.id, r.from_user, u.uid, u.username, u.display_name, u.avatar, r.created_at \
         FROM friend_requests r \
         JOIN users u ON u.id = r.from_user \
         WHERE r.to_user = $1 AND r.status = 'pending' \
         ORDER BY r.created_at ASC",
    )
    .bind(user.0)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let outgoing_rows: Vec<Row> = sqlx::query_as(
        "SELECT r.id, r.to_user, u.uid, u.username, u.display_name, u.avatar, r.created_at \
         FROM friend_requests r \
         JOIN users u ON u.id = r.to_user \
         WHERE r.from_user = $1 AND r.status = 'pending' \
         ORDER BY r.created_at ASC",
    )
    .bind(user.0)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let mut incoming = Vec::with_capacity(incoming_rows.len());
    for (request_id, peer_id, peer_uid, peer_username, peer_display, peer_avatar, created_at) in
        incoming_rows
    {
        incoming.push(IncomingRequestItem {
            request_id,
            from: FriendPeer::from_row(peer_id, peer_uid, peer_username, peer_display, peer_avatar),
            created_at: rfc3339(created_at)?,
        });
    }

    let mut outgoing = Vec::with_capacity(outgoing_rows.len());
    for (request_id, peer_id, peer_uid, peer_username, peer_display, peer_avatar, created_at) in
        outgoing_rows
    {
        outgoing.push(OutgoingRequestItem {
            request_id,
            to: FriendPeer::from_row(peer_id, peer_uid, peer_username, peer_display, peer_avatar),
            created_at: rfc3339(created_at)?,
        });
    }

    Ok(Json(ListRequestsResponse { incoming, outgoing }))
}

// ---------------------------------------------------------------------------
// POST /requests/{id}/accept — recipient accepts
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AcceptFriendRequestResponse {
    /// The user whose request was just accepted (the original sender).
    pub friend: FriendPeer,
}

pub async fn accept_request(
    State(state): State<AppState>,
    user: AuthUser,
    Path(request_id): Path<Uuid>,
) -> Result<Json<AcceptFriendRequestResponse>, AppError> {
    let request: Option<(Uuid, Uuid, String)> =
        sqlx::query_as("SELECT from_user, to_user, status FROM friend_requests WHERE id = $1")
            .bind(request_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    let Some((from_user, to_user, status)) = request else {
        return Err(AppError::ResourceNotFound);
    };
    // Only the recipient may act; everyone else (including the sender) gets
    // the same 404 — existence of the request is never revealed.
    if to_user != user.0 {
        return Err(AppError::ResourceNotFound);
    }
    let pair_key = friend_pair_key(from_user, to_user);

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;

    // Double-accept (or befriended through another path meanwhile).
    if already_friends(&mut tx, &pair_key).await? {
        return Err(AppError::Conflict(ConflictKind::AlreadyFriends));
    }
    if status != "pending" {
        // Declined/cancelled requests are gone as far as accepting goes.
        return Err(AppError::ResourceNotFound);
    }

    // Pending-guarded transition: a concurrent decline/cancel wins instead.
    let updated: Option<Uuid> = sqlx::query_scalar(
        "UPDATE friend_requests SET status = 'accepted', responded_at = now() \
             WHERE id = $1 AND status = 'pending' RETURNING id",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AppError::internal)?;
    if updated.is_none() {
        return Err(AppError::ResourceNotFound);
    }

    // Symmetric edge: two rows sharing one sorted pair_key.
    for (a, b) in [(from_user, to_user), (to_user, from_user)] {
        sqlx::query(
            "INSERT INTO friendships (user_id, friend_id, pair_key) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
        )
        .bind(a)
        .bind(b)
        .bind(&pair_key)
        .execute(&mut *tx)
        .await
        .map_err(AppError::internal)?;
    }
    tx.commit().await.map_err(AppError::internal)?;

    // Response names the ORIGINAL SENDER (the person just befriended); the
    // wire frame tells the sender WHO accepted.
    let (sender_username, sender_uid, sender_display, sender_avatar) =
        peer_identity_of(&state, from_user).await?;
    let (my_username, my_uid, _, _) = peer_identity_of(&state, user.0).await?;

    crate::ws::relay_friend_payload(
        &state,
        from_user,
        Payload::FriendAccepted(FriendAccepted {
            friend: UserIdentity {
                user_id: user.0,
                username: my_username,
                uid: Some(wire_uid(user.0, my_uid)?),
            },
        }),
    );

    tracing::info!(%request_id, accepter = %user.0, sender = %from_user, "friend request accepted");
    Ok(Json(AcceptFriendRequestResponse {
        friend: FriendPeer::from_row(
            from_user,
            sender_uid,
            sender_username,
            sender_display,
            sender_avatar,
        ),
    }))
}

// ---------------------------------------------------------------------------
// POST /requests/{id}/decline — recipient declines
// ---------------------------------------------------------------------------

pub async fn decline_request(
    State(state): State<AppState>,
    user: AuthUser,
    Path(request_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let request: Option<(Uuid, Uuid, String)> =
        sqlx::query_as("SELECT from_user, to_user, status FROM friend_requests WHERE id = $1")
            .bind(request_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    let Some((from_user, to_user, status)) = request else {
        return Err(AppError::ResourceNotFound);
    };
    if to_user != user.0 || status != "pending" {
        return Err(AppError::ResourceNotFound);
    }

    let result = sqlx::query(
        "UPDATE friend_requests SET status = 'declined', responded_at = now() \
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(request_id)
    .execute(&state.pool)
    .await
    .map_err(AppError::internal)?;
    if result.rows_affected() == 0 {
        // Raced with another terminal transition.
        return Err(AppError::ResourceNotFound);
    }

    tracing::info!(%request_id, decliner = %user.0, sender = %from_user, "friend request declined");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// DELETE /requests/{id} — sender cancels
// ---------------------------------------------------------------------------

pub async fn cancel_request(
    State(state): State<AppState>,
    user: AuthUser,
    Path(request_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let request: Option<(Uuid, Uuid, String)> =
        sqlx::query_as("SELECT from_user, to_user, status FROM friend_requests WHERE id = $1")
            .bind(request_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    let Some((from_user, _to_user, status)) = request else {
        return Err(AppError::ResourceNotFound);
    };
    // Sender-only cancel; recipients use decline.
    if from_user != user.0 || status != "pending" {
        return Err(AppError::ResourceNotFound);
    }

    let result = sqlx::query(
        "DELETE FROM friend_requests WHERE id = $1 AND from_user = $2 AND status = 'pending'",
    )
    .bind(request_id)
    .bind(user.0)
    .execute(&state.pool)
    .await
    .map_err(AppError::internal)?;
    if result.rows_affected() == 0 {
        return Err(AppError::ResourceNotFound);
    }

    tracing::info!(%request_id, canceller = %user.0, "friend request cancelled");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// GET / — friend list
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct FriendListItem {
    pub user_id: Uuid,
    /// Stable numeric user id (QQ-style); mirrors the friend's `users.uid`.
    pub uid: i64,
    pub username: String,
    /// Effective display handle (stored display_name, or username when empty).
    pub display_name: String,
    /// Curated avatar emoji (possibly `""`).
    pub avatar: String,
    pub since: String,
}

pub async fn list_friends(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<FriendListItem>>, AppError> {
    type Row = (Uuid, i64, String, String, String, OffsetDateTime);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT f.friend_id, u.uid, u.username, u.display_name, u.avatar, f.since \
         FROM friendships f \
         JOIN users u ON u.id = f.friend_id \
         WHERE f.user_id = $1 \
         ORDER BY u.username ASC",
    )
    .bind(user.0)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let mut items = Vec::with_capacity(rows.len());
    for (user_id, uid, username, display_name, avatar, since) in rows {
        items.push(FriendListItem {
            user_id,
            uid,
            username: username.clone(),
            display_name: crate::profile::effective_display_name(&display_name, &username),
            avatar,
            since: rfc3339(since)?,
        });
    }
    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// DELETE /{userId} — unfriend
// ---------------------------------------------------------------------------

pub async fn unfriend(
    State(state): State<AppState>,
    user: AuthUser,
    Path(target_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    // Both directions share the sorted pair_key, so ONE predicate clears the
    // whole friendship. Idempotent by design: unfriending a non-friend is a
    // no-op that still answers 204.
    let pair_key = friend_pair_key(user.0, target_id);
    sqlx::query("DELETE FROM friendships WHERE pair_key = $1")
        .bind(pair_key)
        .execute(&state.pool)
        .await
        .map_err(AppError::internal)?;

    tracing::info!(user = %user.0, removed = %target_id, "unfriended");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_key_is_order_independent_and_sorted() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert_eq!(friend_pair_key(a, b), friend_pair_key(b, a));
        let key = friend_pair_key(a, b);
        let (left, right) = key.split_once(':').expect("separator");
        assert!(left < right, "lexicographic order required: {key}");
    }

    #[test]
    fn pair_key_never_collides_with_self() {
        let a = Uuid::now_v7();
        assert_eq!(friend_pair_key(a, a), format!("{a}:{a}"));
    }
}
