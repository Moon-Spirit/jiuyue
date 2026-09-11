//! M7 profiles + XP economy HTTP surface (`/api/users/profile`) and the
//! XP award helpers the WS layer calls.
//!
//! Endpoints (Bearer-authenticated):
//!
//! | Method | Path                          | Semantics                                        |
//! |--------|-------------------------------|--------------------------------------------------|
//! | GET    | `/api/users/profile/{user_id}` | any user's public profile → 200; unknown → 404   |
//! | PATCH  | `/api/users/profile`           | self-only profile edit → 200 updated profile     |
//!
//! The response shape is the profile contract shared with the frontend:
//!
//! ```json
//! { "user_id", "username", "uid", "display_name", "bio", "avatar",
//!   "level", "title", "xp", "xp_to_next" }
//! ```
//!
//! `display_name` is ALWAYS the effective handle: empty stored value falls
//! back to `username`, so clients never implement the fallback rule. The
//! level curve / rank titles are the pure functions in
//! [`jiuyue_domain::progress`] — the DB stores `xp` plus a `level` that is
//! recomputed through the pure fn inside every award UPDATE (single source of
//! truth), and the title is derived on read.
//!
//! Avatar values are curated emoji only ([`AVATARS`]); membership is
//! full-string equality (multi-codepoint emoji like ⛏️ are legal values).

use crate::auth::extract::AuthUser;
use crate::error::AppError;
use crate::state::AppState;
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use jiuyue_domain::{
    DAILY_LOGIN_XP, MSG_XP_CHAR_BLOCK, MSG_XP_DAILY_CAP, MSG_XP_PER_BLOCK, level_from_xp,
    title_for_level, xp_for_level,
};
use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Curated avatar set
// ---------------------------------------------------------------------------

/// The curated avatar emoji whitelist (~40 Minecraft-flavoured / plain
/// emoji). `PATCH /api/users/profile` rejects any `avatar` not in this list;
/// the empty string clears the avatar.
pub const AVATARS: &[&str] = &[
    "🟫", "🪨", "🪵", "🧱", "⛏️", "🪓", "🏹", "🗡️", "🛡️", "🪖", "💎", "🪙", "⭐", "🌟", "🔥", "💧",
    "🌱", "🌳", "🍄", "🐷", "🐮", "🐑", "🐔", "🐺", "🐱", "🐲", "🥚", "⚗️", "🧪", "🪄", "📦", "🚪",
    "🗺️", "🧭", "🏔️", "🌋", "🌙", "☀️", "👑", "💀", "👾", "⚡", "🌀", "🌸",
];

/// Maximum accepted size of a custom data-URL avatar (raw base64 payload).
/// ~256 KB of base64 → roughly 192 KB of decoded image data.
const MAX_CUSTOM_AVATAR_BYTES: usize = 256 * 1024;

fn is_valid_avatar(value: &str) -> bool {
    if value.is_empty() || AVATARS.contains(&value) {
        return true;
    }
    is_valid_custom_avatar(value)
}

/// Custom uploads arrive as `data:image/{png|jpeg|webp};base64,…` strings.
/// The decoder is lenient (a few hundred KB), so validation is a shape check
/// plus a payload-size cap rather than a full re-decode.
pub(crate) fn is_valid_custom_avatar(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("data:image/") else {
        return false;
    };
    let Some((mime, payload)) = rest.split_once(";base64,") else {
        return false;
    };
    if !matches!(mime, "png" | "jpeg" | "webp") {
        return false;
    }
    payload.len() <= MAX_CUSTOM_AVATAR_BYTES && !payload.is_empty()
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// One user's full public profile. `display_name` carries the effective
/// handle (stored value, or `username` when empty); `avatar` is the raw
/// stored emoji (possibly `""`).
#[derive(Debug, Serialize)]
pub struct ProfileResponse {
    pub user_id: Uuid,
    pub uid: i64,
    pub username: String,
    pub display_name: String,
    pub bio: String,
    pub avatar: String,
    pub level: i64,
    /// Rank title for `level` (Minecraft ladder) — derived from the stored
    /// level via the domain pure fn, never stored itself.
    pub title: &'static str,
    pub xp: i64,
    /// Total XP required to advance from the current level (bar denominator).
    pub xp_to_next: i64,
}

/// Partial profile update: every field optional; only present fields change.
/// An explicitly-present empty string clears (`display_name`/`avatar`) or
/// empties (`bio`).
#[derive(Debug, Deserialize, Default)]
pub struct UpdateProfileRequest {
    /// After trim: 1..=24 characters, or empty → clear (fallback to username).
    #[serde(default)]
    pub display_name: Option<String>,
    /// At most 200 characters; empty → clear.
    #[serde(default)]
    pub bio: Option<String>,
    /// One of [`AVATARS`]; empty → clear. Anything else → 422.
    #[serde(default)]
    pub avatar: Option<String>,
}

/// Effective handle: an empty stored `display_name` renders as `username`.
pub(crate) fn effective_display_name(display_name: &str, username: &str) -> String {
    if display_name.is_empty() {
        username.to_owned()
    } else {
        display_name.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Row loading
// ---------------------------------------------------------------------------

type ProfileRow = (Uuid, i64, String, String, String, String, i32, i64);

const PROFILE_QUERY: &str = "SELECT u.id, u.uid, u.username, u.display_name, u.bio, u.avatar, \
     COALESCE(x.level, 1) AS level, COALESCE(x.xp, 0) AS xp \
     FROM users u \
     LEFT JOIN xp_accounts x ON x.user_id = u.id \
     WHERE u.id = $1";

async fn load_profile(state: &AppState, user_id: Uuid) -> Result<ProfileResponse, AppError> {
    let row: Option<ProfileRow> = sqlx::query_as(PROFILE_QUERY)
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::internal)?;
    let Some((uid_user_id, uid, username, display_name, bio, avatar, level, xp)) = row else {
        return Err(AppError::ResourceNotFound);
    };
    let level = i64::from(level);
    // xp_to_next = req(level) − progress within the current level.
    let xp_to_next = xp_for_level(level);
    Ok(ProfileResponse {
        user_id: uid_user_id,
        uid,
        username: username.clone(),
        display_name: effective_display_name(&display_name, &username),
        bio,
        avatar,
        level,
        title: title_for_level(level),
        xp,
        xp_to_next: xp_to_next.max(0),
    })
}

// ---------------------------------------------------------------------------
// GET /api/users/profile/{user_id} — self and others share this route
// ---------------------------------------------------------------------------

pub async fn get_profile(
    State(state): State<AppState>,
    user: AuthUser,
    Path(target_id): Path<Uuid>,
) -> Result<Json<ProfileResponse>, AppError> {
    // Same endpoint for self and others (the caller id is only used for
    // auth; nothing here is private).
    let _ = user;
    load_profile(&state, target_id).await.map(Json)
}

// ---------------------------------------------------------------------------
// PATCH /api/users/profile — self-only profile edit
// ---------------------------------------------------------------------------

pub async fn update_profile(
    State(state): State<AppState>,
    user: AuthUser,
    payload: Result<Json<UpdateProfileRequest>, JsonRejection>,
) -> Result<Json<ProfileResponse>, AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;

    // Read the current row so PATCH semantics stay "change what is present".
    let current: Option<(String, String, String, String)> =
        sqlx::query_as("SELECT username, display_name, bio, avatar FROM users WHERE id = $1")
            .bind(user.0)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    let Some((_username, cur_display, cur_bio, cur_avatar)) = current else {
        return Err(AppError::ResourceNotFound);
    };

    // display_name: trim; empty → clear; else 1..=24 chars.
    let display_name = match req.display_name {
        Some(raw) => {
            let trimmed = raw.trim().to_owned();
            if !trimmed.is_empty() {
                let len = trimmed.chars().count();
                if !(1..=24).contains(&len) {
                    return Err(AppError::Validation(
                        "display_name must be 1-24 characters, or empty to clear".into(),
                    ));
                }
            }
            trimmed
        }
        None => cur_display,
    };

    // bio: ≤ 200 chars; empty clears.
    let bio = match req.bio {
        Some(raw) => {
            if raw.chars().count() > 200 {
                return Err(AppError::Validation(
                    "bio must be at most 200 characters".into(),
                ));
            }
            raw
        }
        None => cur_bio,
    };

    // avatar: empty clears; else must be a member of the curated set.
    let avatar = match req.avatar {
        Some(raw) => {
            if !is_valid_avatar(&raw) {
                return Err(AppError::Validation(format!(
                    "avatar must be one of the {} curated emoji, or empty to clear",
                    AVATARS.len()
                )));
            }
            raw
        }
        None => cur_avatar,
    };

    sqlx::query("UPDATE users SET display_name = $1, bio = $2, avatar = $3 WHERE id = $4")
        .bind(&display_name)
        .bind(&bio)
        .bind(&avatar)
        .bind(user.0)
        .execute(&state.pool)
        .await
        .map_err(AppError::internal)?;

    tracing::info!(user = %user.0, "profile updated");

    // Live-notify conversation partners so avatars/display names refresh on
    // their session rows without a REST round-trip (best-effort fan-out).
    crate::ws::relay_profile_updated(&state, user.0, &display_name, &avatar).await;

    load_profile(&state, user.0).await.map(Json)
}

// ---------------------------------------------------------------------------
// XP award helpers (called from the WS layer; all best-effort)
// ---------------------------------------------------------------------------

/// The xp_accounts row state a writer needs to apply an award atomically.
struct XpRow {
    xp: i64,
    last_daily_bonus_date: Option<Date>,
    last_msg_xp_date: Option<Date>,
    msg_xp_today: i32,
}

/// Lazily creates the user's xp_accounts row, then locks it `FOR UPDATE` so
/// concurrent awards serialize on one writer per user.
async fn lock_xp_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
) -> Result<XpRow, sqlx::Error> {
    sqlx::query("INSERT INTO xp_accounts (user_id) VALUES ($1) ON CONFLICT (user_id) DO NOTHING")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    let row: (i64, Option<Date>, Option<Date>, i32) = sqlx::query_as(
        "SELECT xp, last_daily_bonus_date, last_msg_xp_date, msg_xp_today \
         FROM xp_accounts WHERE user_id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(XpRow {
        xp: row.0,
        last_daily_bonus_date: row.1,
        last_msg_xp_date: row.2,
        msg_xp_today: row.3,
    })
}

/// Applies a raw XP delta and recomputes the stored level through the domain
/// pure fn (single UPDATE: xp, level, updated_at travel together).
async fn apply_xp(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
    delta: i64,
    set_daily_bonus_date: Option<Date>,
    set_msg_date: Option<(Date, i32)>,
) -> Result<(), sqlx::Error> {
    let row = lock_xp_row(tx, user_id).await?;
    let new_xp = row.xp + delta;
    let new_level = level_from_xp(new_xp);
    let (new_msg_today, new_msg_date) = match set_msg_date {
        Some((date, today_used)) => {
            let reset_base = if row.last_msg_xp_date == Some(date) {
                row.msg_xp_today
            } else {
                0
            };
            (reset_base + today_used, Some(date))
        }
        None => (row.msg_xp_today, row.last_msg_xp_date),
    };
    let new_bonus_date = set_daily_bonus_date.or(row.last_daily_bonus_date);
    sqlx::query(
        "UPDATE xp_accounts SET xp = $1, level = $2, last_daily_bonus_date = $3, \
         last_msg_xp_date = $4, msg_xp_today = $5, updated_at = now() WHERE user_id = $6",
    )
    .bind(new_xp)
    .bind(new_level)
    .bind(new_bonus_date)
    .bind(new_msg_date)
    .bind(new_msg_today)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Daily-login bonus: +[`DAILY_LOGIN_XP`] XP once per UTC day, granted when
/// the guard `last_daily_bonus_date < today` still holds under the row lock.
/// Idempotent under concurrent connections — only the first wins.
pub(crate) async fn grant_daily_login_bonus(
    pool: &sqlx::PgPool,
    user_id: Uuid,
) -> anyhow::Result<()> {
    let today = OffsetDateTime::now_utc().date();
    let mut tx = pool.begin().await?;
    let row = lock_xp_row(&mut tx, user_id).await?;
    if row.last_daily_bonus_date == Some(today) {
        // Already granted for today (any connection/device this UTC day).
        tx.commit().await?;
        return Ok(());
    }
    apply_xp(&mut tx, user_id, DAILY_LOGIN_XP, Some(today), None).await?;
    tx.commit().await?;
    tracing::debug!(%user_id, %today, xp = DAILY_LOGIN_XP, "daily login bonus granted");
    Ok(())
}

/// Messaging XP: `+MSG_XP_PER_BLOCK` per full [`MSG_XP_CHAR_BLOCK`]-character
/// block of a SENT plaintext message, capped at [`MSG_XP_DAILY_CAP`]/day.
///
/// Only `kind='direct'` conversations earn XP — secret-conversation messages
/// count 0 (their content is not server-readable). Only the character COUNT
/// is passed in; the body itself never reaches this module.
pub(crate) async fn award_message_xp_for_chars(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    conversation_id: i64,
    char_count: usize,
) -> anyhow::Result<()> {
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_optional(pool)
        .await?;
    let Some(kind) = kind else {
        return Ok(()); // conversation vanished — nothing to do
    };
    if kind != "direct" {
        return Ok(()); // secret (and any future non-direct) chats earn 0
    }

    let gain = (char_count / MSG_XP_CHAR_BLOCK) as i64 * MSG_XP_PER_BLOCK;
    if gain <= 0 {
        return Ok(());
    }

    let today = OffsetDateTime::now_utc().date();
    let mut tx = pool.begin().await?;
    let row = lock_xp_row(&mut tx, user_id).await?;
    let used_today = if row.last_msg_xp_date == Some(today) {
        row.msg_xp_today
    } else {
        0
    };
    let available = MSG_XP_DAILY_CAP - i64::from(used_today);
    if available <= 0 {
        tx.commit().await?;
        return Ok(());
    }
    let actual = gain.min(available);
    apply_xp(&mut tx, user_id, actual, None, Some((today, actual as i32))).await?;
    tx.commit().await?;
    tracing::debug!(%user_id, %conversation_id, actual, "message xp awarded");
    Ok(())
}
