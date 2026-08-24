//! M3 HTTP surface: E2EE key distribution for secret chats.
//!
//! The server stores PUBLIC key material only (identity keys + one-time
//! prekeys) — never any private key, and key bytes are never logged. Bundle
//! fetches CONSUME exactly one one-time key (Signal-style take-one
//! semantics) via a transactional `SELECT ... FOR UPDATE` plus a
//! `jsonb - idx` pop, so two concurrent claimers can never observe the same
//! one-time key.
//!
//! Routes (Bearer-authenticated):
//!
//! - `POST /api/e2ee/keys/upload[?device_id=<uuid>]` — upsert the caller's
//!   bundle keyed by device (latest upload wins). Without `device_id` a
//!   fresh device row is registered first (same pattern as WS connect);
//!   with it, the row must already belong to the caller.
//! - `GET /api/e2ee/keys/{username}` — most recent bundle of ANY device of
//!   that user, popping one one-time key. Answers `409 no_one_time_keys`
//!   once the pool is drained and `404 peer_not_found` when the user has no
//!   bundle at all.

use crate::auth::extract::AuthUser;
use crate::error::{AppError, ConflictKind};
use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/keys/upload", axum::routing::post(upload_keys))
        .route("/keys/{username}", axum::routing::get(fetch_keys))
}

#[derive(Debug, Deserialize)]
pub struct UploadKeysRequest {
    /// Base64 Curve25519 identity key of the caller's device.
    pub identity_key: String,
    /// Base64 public halves of freshly generated one-time keys.
    #[serde(default)]
    pub one_time_keys: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct UploadKeysResponse {
    pub device_id: Uuid,
    pub one_time_key_count: usize,
}

/// POST /api/e2ee/keys/upload — publish/replace this device's bundle.
pub async fn upload_keys(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<HashMap<String, String>>,
    payload: Result<Json<UploadKeysRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<UploadKeysResponse>), AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;
    let identity_key = req.identity_key.trim();
    if identity_key.is_empty() {
        return Err(AppError::BadRequest("identity_key is required".to_owned()));
    }

    // Device resolution: an explicit ?device_id= must belong to the caller;
    // otherwise a fresh device row is registered (same pattern as WS
    // connect's per-connection device rows).
    let device_id: Uuid = match params.get("device_id").map(String::as_str) {
        Some(raw) => {
            let requested: Uuid = raw
                .parse()
                .map_err(|_| AppError::BadRequest("device_id must be a uuid".to_owned()))?;
            let owned: Option<i64> =
                sqlx::query_scalar("SELECT 1::int8 FROM devices WHERE id = $1 AND user_id = $2")
                    .bind(requested)
                    .bind(user.0)
                    .fetch_optional(&state.pool)
                    .await
                    .map_err(AppError::internal)?;
            if owned.is_none() {
                return Err(AppError::BadRequest(
                    "device_id does not belong to the caller".to_owned(),
                ));
            }
            requested
        }
        None => {
            let fresh = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO devices (id, user_id, platform, last_seen_at) \
                 VALUES ($1, $2, $3, now())",
            )
            .bind(fresh)
            .bind(user.0)
            .bind("api")
            .execute(&state.pool)
            .await
            .map_err(AppError::internal)?;
            fresh
        }
    };

    // Upsert keyed by device: re-uploads replace identity_key + OTK pool.
    sqlx::query(
        "INSERT INTO e2ee_identities (id, device_id, user_id, identity_key, one_time_keys) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (device_id) DO UPDATE \
         SET identity_key = EXCLUDED.identity_key, \
             one_time_keys = EXCLUDED.one_time_keys, \
             uploaded_at = now()",
    )
    .bind(Uuid::now_v7())
    .bind(device_id)
    .bind(user.0)
    .bind(identity_key)
    .bind(serde_json::to_value(&req.one_time_keys).map_err(AppError::internal)?)
    .execute(&state.pool)
    .await
    .map_err(AppError::internal)?;

    // Log counts/ids only — never key material.
    tracing::info!(
        user_id = %user.0,
        %device_id,
        count = req.one_time_keys.len(),
        "e2ee keys uploaded"
    );
    Ok((
        StatusCode::CREATED,
        Json(UploadKeysResponse {
            device_id,
            one_time_key_count: req.one_time_keys.len(),
        }),
    ))
}

#[derive(Debug, Serialize)]
pub struct KeyBundleResponse {
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub identity_key: String,
    /// Exactly ONE consumed one-time key (base64).
    pub one_time_key: String,
}

/// GET /api/e2ee/keys/{username} — fetch-and-consume the freshest bundle.
pub async fn fetch_keys(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(username): Path<String>,
) -> Result<Json<KeyBundleResponse>, AppError> {
    let username = username.trim().to_lowercase();

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    let target: Option<(Uuid, Uuid, Uuid, String, serde_json::Value)> = sqlx::query_as(
        "SELECT e.id, u.id, d.id, e.identity_key, e.one_time_keys \
         FROM e2ee_identities e \
         JOIN users u ON u.id = e.user_id \
         JOIN devices d ON d.id = e.device_id \
         WHERE u.username = $1 \
         ORDER BY e.uploaded_at DESC \
         LIMIT 1 \
         FOR UPDATE OF e",
    )
    .bind(&username)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AppError::internal)?;

    let Some((bundle_id, user_id, device_id, identity_key, one_time_keys)) = target else {
        // No bundle for that username: plain 404 with the existing machine
        // code (usernames are public handles; no existence games needed).
        return Err(AppError::PeerNotFound);
    };

    // The upload path only ever stores JSON string arrays; filter defensively
    // anyway so malformed elements can never panic or leak into responses.
    let keys: Vec<String> = one_time_keys
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if keys.is_empty() {
        return Err(AppError::Conflict(ConflictKind::NoOneTimeKeys));
    }

    // Pop exactly ONE element (the last) inside the locked transaction.
    let popped = keys[keys.len() - 1].clone();
    let idx = i32::try_from(keys.len() - 1).map_err(AppError::internal)?;
    sqlx::query("UPDATE e2ee_identities SET one_time_keys = one_time_keys - $2::int WHERE id = $1")
        .bind(bundle_id)
        .bind(idx)
        .execute(&mut *tx)
        .await
        .map_err(AppError::internal)?;
    tx.commit().await.map_err(AppError::internal)?;

    tracing::info!(%user_id, %device_id, remaining = keys.len() - 1, "e2ee bundle claimed");
    Ok(Json(KeyBundleResponse {
        user_id,
        device_id,
        identity_key,
        one_time_key: popped,
    }))
}
