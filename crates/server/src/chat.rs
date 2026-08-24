//! Ticket 05 HTTP surface: direct-conversation creation.
//!
//! `POST /api/conversations { peer_username }` — create-or-get semantics via
//! the partial unique index on `conversations(pair_key) WHERE kind='direct'`:
//! both peers derive the identical lexicographically-sorted `pair_key`, so
//! concurrent creations collapse onto one row (`ON CONFLICT DO NOTHING` +
//! re-select) and every caller ends up in the same conversation with both
//! members present.

use crate::auth::extract::AuthUser;
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct CreateDirectRequest {
    pub peer_username: String,
}

#[derive(Debug, Serialize)]
pub struct PeerInfo {
    pub user_id: Uuid,
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct CreateDirectResponse {
    pub conversation_id: i64,
    /// True when this call created the row; false for create-or-get hits.
    pub created: bool,
    pub peer: PeerInfo,
}

/// POST /api/conversations — Bearer-authenticated direct-conversation
/// create-or-get. Always answers `201` with `{conversation_id, created, peer}`.
pub async fn create_direct(
    State(state): State<AppState>,
    user: AuthUser,
    payload: Result<Json<CreateDirectRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreateDirectResponse>), AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;
    let peer_username = req.peer_username.trim().to_lowercase();
    if peer_username.is_empty() {
        return Err(AppError::BadRequest("peer_username is required".to_owned()));
    }

    let peer: Option<(Uuid, String)> =
        sqlx::query_as("SELECT id, username FROM users WHERE username = $1 LIMIT 1")
            .bind(&peer_username)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    // Unknown peer is a plain 404 with a machine code (no existence games
    // needed here: usernames are public handles, not secrets).
    let Some((peer_id, peer_username)) = peer else {
        return Err(AppError::PeerNotFound);
    };
    if peer_id == user.0 {
        return Err(AppError::BadRequest(
            "cannot open a direct conversation with yourself".to_owned(),
        ));
    }

    let pair_key = direct_pair_key(user.0, peer_id);
    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO conversations (kind, pair_key, created_by) \
         VALUES ('direct', $1, $2) \
         ON CONFLICT (pair_key) WHERE kind = 'direct' DO NOTHING \
         RETURNING id",
    )
    .bind(&pair_key)
    .bind(user.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AppError::internal)?;

    let (conversation_id, created) = match inserted {
        Some(id) => (id, true),
        None => {
            // Lost the race: the winner's row is committed (or at least
            // visible under read-committed once its tx committed) — fetch it.
            let existing: i64 = sqlx::query_scalar(
                "SELECT id FROM conversations WHERE pair_key = $1 AND kind = 'direct'",
            )
            .bind(&pair_key)
            .fetch_one(&mut *tx)
            .await
            .map_err(AppError::internal)?;
            (existing, false)
        }
    };

    // Idempotent membership for both peers (covers the create-or-get path).
    for member_id in [user.0, peer_id] {
        sqlx::query(
            "INSERT INTO conversation_members (conversation_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(conversation_id)
        .bind(member_id)
        .execute(&mut *tx)
        .await
        .map_err(AppError::internal)?;
    }
    tx.commit().await.map_err(AppError::internal)?;

    tracing::info!(%conversation_id, created, "direct conversation ready");
    Ok((
        StatusCode::CREATED,
        Json(CreateDirectResponse {
            conversation_id,
            created,
            peer: PeerInfo {
                user_id: peer_id,
                username: peer_username,
            },
        }),
    ))
}

/// Lexicographically sorted `"uuidA:uuidB"` — order-independent so both peers
/// compute the identical key from either side of the conversation.
fn direct_pair_key(a: Uuid, b: Uuid) -> String {
    let (low, high) = if a.as_bytes() <= b.as_bytes() { (a, b) } else { (b, a) };
    format!("{low}:{high}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_key_is_order_independent_and_sorted() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert_eq!(direct_pair_key(a, b), direct_pair_key(b, a));
        let key = direct_pair_key(a, b).clone();
        let (left, right) = key.split_once(':').expect("separator");
        assert!(left < right, "lexicographic order required: {key}");
    }

    #[test]
    fn pair_key_never_collides_with_self() {
        let a = Uuid::now_v7();
        let key = direct_pair_key(a, a);
        assert_eq!(key, format!("{a}:{a}"), "self-pair is well-formed (server rejects it earlier)");
    }
}
