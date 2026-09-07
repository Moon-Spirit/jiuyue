//! `/api/users/*` — public user directory lookups (Bearer-gated).
//!
//! Endpoints (all Bearer-authenticated):
//!
//! | Method | Path                 | Semantics                                               |
//! |--------|----------------------|---------------------------------------------------------|
//! | GET    | `/api/users/search`  | `?q=<query>` → `[{user_id, username, uid}]`, max 10    |
//!
//! # Match rule (deterministic)
//!
//! `q` is trimmed; an empty result answers 422 `validation_error`.
//!
//! - **All-digits `q`** (numeric): resolved as a uid search — exact uid match
//!   ranks first, then uid-prefix matches (`uid::text LIKE 'q%'`), and a user
//!   whose *username* equals the digits (numeric usernames are legal per the
//!   `[a-z0-9_]{3,32}` rule) also matches.
//! - **Any other `q`**: resolved as a username search — exact username match
//!   ranks first, then username-prefix matches (`ILIKE 'q%'`, so it is
//!   case-insensitive even though stored usernames are lowercase).
//!
//! The authenticated caller is always excluded. No matches → `200 []`
//! (never 404). Responses are capped at 10 rows and ordered by the rank above
//! then by uid, keeping results deterministic for a fixed dataset.

use crate::auth::extract::AuthUser;
use crate::error::AppError;
use crate::profile::{get_profile, update_profile};
use crate::state::AppState;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Hard cap on search results; the UI paginates by typing more characters.
const SEARCH_LIMIT: i64 = 10;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(search))
        // M7: self-only profile edit + any-user profile fetch. A single GET
        // route serves self and others alike (`/profile/{user_id}`).
        .route("/profile", patch(update_profile))
        .route("/profile/{user_id}", get(get_profile))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

/// One search hit. `uid` is the stable numeric user id (QQ-style).
/// `display_name` is the effective handle (falls back to `username` server-
/// side); `avatar` is the raw curated emoji (possibly `""`).
#[derive(Debug, Serialize)]
pub struct SearchResultItem {
    pub user_id: Uuid,
    pub username: String,
    pub uid: i64,
    pub display_name: String,
    pub avatar: String,
}

/// `GET /api/users/search?q=<query>` — Bearer-gated user lookup by uid or
/// username. See the module docs for the deterministic match rule.
pub async fn search(
    State(state): State<AppState>,
    user: AuthUser,
    payload: Result<Query<SearchQuery>, QueryRejection>,
) -> Result<Json<Vec<SearchResultItem>>, AppError> {
    let q = payload
        .map_err(|_| AppError::Validation("q is required".into()))?
        .q
        .trim()
        .to_owned();
    if q.is_empty() {
        return Err(AppError::Validation("q must be a non-empty query".into()));
    }

    let all_digits = !q.is_empty() && q.chars().all(|c| c.is_ascii_digit());
    let items = if all_digits {
        search_numeric(&state, user.0, &q).await?
    } else {
        search_by_username(&state, user.0, &q).await?
    };

    Ok(Json(items))
}

/// Numeric `q`: exact uid first, then uid-prefix, then a username equal to
/// the digits. `rank` keeps the ordering deterministic for a fixed dataset.
async fn search_numeric(
    state: &AppState,
    me: Uuid,
    q: &str,
) -> Result<Vec<SearchResultItem>, AppError> {
    let prefix = format!("{q}%");
    type Row = (Uuid, String, i64, String, String, i32);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, username, uid, display_name, avatar, \
                CASE \
                    WHEN uid::text = $1 THEN 0 \
                    WHEN uid::text LIKE $2 THEN 1 \
                    ELSE 2 \
                END AS rank \
         FROM users \
         WHERE id <> $3 \
           AND (uid::text = $1 OR uid::text LIKE $2 OR username = $1) \
         ORDER BY rank ASC, uid ASC \
         LIMIT $4",
    )
    .bind(q)
    .bind(&prefix)
    .bind(me)
    .bind(SEARCH_LIMIT)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    Ok(rows
        .into_iter()
        .map(|(user_id, username, uid, display_name, avatar, _rank)| SearchResultItem {
            user_id,
            username: username.clone(),
            uid,
            display_name: crate::profile::effective_display_name(&display_name, &username),
            avatar,
        })
        .collect())
}

/// Non-numeric `q`: exact username first, then username-prefix (`ILIKE`).
async fn search_by_username(
    state: &AppState,
    me: Uuid,
    q: &str,
) -> Result<Vec<SearchResultItem>, AppError> {
    let lowered = q.to_lowercase();
    let prefix = format!("{lowered}%");
    type Row = (Uuid, String, i64, String, String, i32);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, username, uid, display_name, avatar, \
                CASE \
                    WHEN username = $1 THEN 0 \
                    ELSE 1 \
                END AS rank \
         FROM users \
         WHERE id <> $3 \
           AND (username = $1 OR username ILIKE $2) \
         ORDER BY rank ASC, uid ASC \
         LIMIT $4",
    )
    .bind(&lowered)
    .bind(&prefix)
    .bind(me)
    .bind(SEARCH_LIMIT)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    Ok(rows
        .into_iter()
        .map(|(user_id, username, uid, display_name, avatar, _rank)| SearchResultItem {
            user_id,
            username: username.clone(),
            uid,
            display_name: crate::profile::effective_display_name(&display_name, &username),
            avatar,
        })
        .collect())
}
