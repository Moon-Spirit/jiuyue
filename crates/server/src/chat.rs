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
    /// Optional conversation kind: `"direct"` (default) or `"secret"`.
    /// Secret pairs share the sorted-pair-key create-or-get contract via the
    /// `conversations_secret_pair_key_uq` partial index.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PeerInfo {
    pub user_id: Uuid,
    /// Stable numeric user id (QQ-style); mirrors the peer's `users.uid`.
    pub uid: i64,
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct CreateDirectResponse {
    pub conversation_id: i64,
    /// True when this call created the row; false for create-or-get hits.
    pub created: bool,
    /// `"direct"` or `"secret"` — echoes the resolved request kind.
    pub kind: String,
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
    let kind = normalize_kind(req.kind.as_deref()).map_err(AppError::BadRequest)?;

    let peer: Option<(Uuid, i64, String)> =
        sqlx::query_as("SELECT id, uid, username FROM users WHERE username = $1 LIMIT 1")
            .bind(&peer_username)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    // Unknown peer is a plain 404 with a machine code (no existence games
    // needed here: usernames are public handles, not secrets).
    let Some((peer_id, peer_uid, peer_username)) = peer else {
        return Err(AppError::PeerNotFound);
    };
    if peer_id == user.0 {
        return Err(AppError::BadRequest(
            "cannot open a direct conversation with yourself".to_owned(),
        ));
    }

    let pair_key = direct_pair_key(user.0, peer_id);
    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    // `kind` is whitelisted above ('direct'|'secret'), so interpolating it
    // into the statement (the ON CONFLICT clause must name the matching
    // partial-index predicate) is injection-safe.
    let inserted: Option<i64> = sqlx::query_scalar(&format!(
        "INSERT INTO conversations (kind, pair_key, created_by) \
         VALUES ('{kind}', $1, $2) \
         ON CONFLICT (pair_key) WHERE kind = '{kind}' DO NOTHING \
         RETURNING id"
    ))
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
            let existing: i64 = sqlx::query_scalar(&format!(
                "SELECT id FROM conversations WHERE pair_key = $1 AND kind = '{kind}'"
            ))
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

    tracing::info!(%conversation_id, created, kind, "conversation ready");
    Ok((
        StatusCode::CREATED,
        Json(CreateDirectResponse {
            conversation_id,
            created,
            kind: kind.to_owned(),
            peer: PeerInfo {
                user_id: peer_id,
                uid: peer_uid,
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

/// Validates the optional `kind` field; absent/empty means `"direct"`.
fn normalize_kind(kind: Option<&str>) -> Result<&'static str, String> {
    match kind.map(str::trim) {
        None | Some("") | Some("direct") => Ok("direct"),
        Some("secret") => Ok("secret"),
        Some(other) => Err(format!(
            "unsupported kind `{other}`; expected \"direct\" or \"secret\""
        )),
    }
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

    #[test]
    fn normalize_kind_defaults_to_direct_and_accepts_secret() {
        assert_eq!(normalize_kind(None).unwrap(), "direct");
        assert_eq!(normalize_kind(Some("")).unwrap(), "direct");
        assert_eq!(normalize_kind(Some("direct")).unwrap(), "direct");
        assert_eq!(normalize_kind(Some(" secret ")).unwrap(), "secret");
        assert!(normalize_kind(Some("group")).is_err());
        assert!(normalize_kind(Some("SECRET")).is_err(), "kinds are lowercase-only");
    }
}

/// GET /api/conversations — conversation memberships of the authenticated
/// user, newest first. For `direct` conversations `peer` carries the single
/// other member; other kinds leave it `null` until group support lands.
#[derive(Debug, Serialize)]
pub struct ConversationListItem {
    pub conversation_id: i64,
    pub kind: String,
    pub peer: Option<PeerInfo>,
    pub last_seq: i64,
    /// Server-side delivery cursor for THIS member; clients bootstrap their
    /// offline sync from here on fresh sessions.
    pub last_delivered_seq: i64,
}

pub async fn list_conversations(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<ConversationListItem>>, AppError> {
    type ConvRow = (i64, String, i64, i64, Option<Uuid>, Option<i64>, Option<String>);
    let rows: Vec<ConvRow> = sqlx::query_as(
        r#"
        SELECT c.id,
               c.kind,
               c.last_seq,
               m.last_delivered_seq,
               peer.user_id,
               peer.uid,
               peer.username
        FROM conversation_members m
        JOIN conversations c ON c.id = m.conversation_id
        LEFT JOIN LATERAL (
            SELECT cm2.user_id AS user_id, u.uid AS uid, u.username AS username
            FROM conversation_members cm2
            JOIN users u ON u.id = cm2.user_id
            WHERE cm2.conversation_id = c.id AND cm2.user_id <> m.user_id
            LIMIT 1
        ) peer ON TRUE
        WHERE m.user_id = $1
        ORDER BY c.id DESC
        "#,
    )
    .bind(user.0)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let items = rows
        .into_iter()
        .map(
            |(conversation_id, kind, last_seq, last_delivered_seq, peer_id, peer_uid, peer_username)| {
                ConversationListItem {
                    conversation_id,
                    kind,
                    peer: peer_id.map(|user_id| PeerInfo {
                        user_id,
                        uid: peer_uid.unwrap_or_default(),
                        username: peer_username.unwrap_or_default(),
                    }),
                    last_seq,
                    last_delivered_seq,
                }
            },
        )
        .collect();
    Ok(Json(items))
}
