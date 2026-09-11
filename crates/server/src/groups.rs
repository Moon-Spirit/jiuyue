//! M11a group-chat HTTP surface (`/api/groups/*`).
//!
//! Groups are a first-class conversation kind (`conversations.kind='group'`)
//! with a three-tier role model (`owner` > `admin` > `member`) and
//! consent-based invites: an owner/admin invites a username, and the INVITEE
//! must accept before becoming a member.
//!
//! Endpoints (all Bearer-authenticated):
//!
//! | Method | Path                                      | Semantics                                                       |
//! |--------|-------------------------------------------|-----------------------------------------------------------------|
//! | POST   | `/`                                       | create `{name, invite_usernames?}` → 201; 422 bad name; 404 unknown invitee |
//! | GET    | `/{id}`                                   | member-only group info (non-member → 404, no oracle)            |
//! | POST   | `/{id}/invites`                           | owner/admin invites `{username}` → 201 `{invite_id}`            |
//! | GET    | `/invites`                                | my pending invites                                              |
//! | POST   | `/invites/{invite_id}/accept`             | invitee accepts → 200 `{conversation_id}`; not pending → 409     |
//! | POST   | `/invites/{invite_id}/decline`            | invitee declines → 204                                          |
//! | POST   | `/{id}/members/{user_id}/kick`            | owner/admin kicks; owner immutable, admin✗admin → 204/403       |
//! | POST   | `/{id}/members/{user_id}/role`            | owner sets `admin`\|`member` → 200                              |
//! | POST   | `/{id}/transfer`                          | owner hands ownership to a member → 200                         |
//! | POST   | `/{id}/leave`                             | member leaves; owner must transfer first → 204/422              |
//!
//! M13a group-file storage is routed here and implemented in
//! [`crate::group_files`]: `POST|GET /{id}/files` (upload/list),
//! `GET /files/{file_id}` (download) and
//! `POST /files/{file_id}/delete` (uploader/owner/admin). See that module for
//! the 1 GiB free-quota → 7-day temporary pricing rule and the expiry sweeper.
//!
//! Non-members never learn whether a group exists: every group-scoped read or
//! mutation answers a plain 404 for a non-member (and for a conversation that
//! is not a group). Authenticated-but-underprivileged members get 403.
//!
//! Live delivery rides fire-and-forget WS frames via [`crate::ws`]: each new
//! invite pushes `group.invited` to the invitee; every membership/role
//! mutation broadcasts `group.updated` to all current members so clients can
//! refetch. Both are never persisted and never replayed by `sync.req` — the
//! REST surface is authoritative.

use crate::auth::extract::AuthUser;
use crate::error::{AppError, ConflictKind};
use crate::groups_titles::{
    CUSTOM_TITLE_MAX_CHARS, GROUP_XP_DAILY_CAP, GROUP_XP_PER_MESSAGE, group_level_from_xp,
    resolve_member_title,
};
use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use jiuyue_protocol::{GroupInvited, Payload, UserIdentity};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use time::Date;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Group names are trimmed and capped at this many characters.
const GROUP_NAME_MAX_CHARS: usize = 32;

/// Group descriptions are capped at this many characters (profile bio parity).
const GROUP_DESCRIPTION_MAX_CHARS: usize = 200;

/// Group routes nested under `/api/groups`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(create_group))
        .route("/invites", get(list_my_invites))
        .route("/invites/{invite_id}/accept", post(accept_invite))
        .route("/invites/{invite_id}/decline", post(decline_invite))
        .route("/{conversation_id}", get(get_group).patch(update_group))
        // M13a group files. `/{conversation_id}/files` and the literal
        // `/files/{file_id}` are structurally distinct (matchit prefers the
        // literal `files` branch), so the two path families coexist.
        .route(
            "/{conversation_id}/files",
            get(crate::group_files::list_files).post(crate::group_files::upload_file),
        )
        .route(
            "/files/{file_id}",
            get(crate::group_files::download_file),
        )
        .route(
            "/files/{file_id}/delete",
            post(crate::group_files::delete_file),
        )
        .route("/{conversation_id}/invites", post(invite_member))
        .route("/{conversation_id}/members/{user_id}/kick", post(kick_member))
        .route("/{conversation_id}/members/{user_id}/role", post(set_role))
        .route(
            "/{conversation_id}/members/{user_id}/title",
            post(set_member_title),
        )
        .route("/{conversation_id}/transfer", post(transfer_ownership))
        .route("/{conversation_id}/leave", post(leave_group))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn rfc3339(value: OffsetDateTime) -> Result<String, AppError> {
    value.format(&Rfc3339).map_err(AppError::internal)
}

/// Resolves a user's wire identity block (`user_id`, `username`, `uid`).
async fn identity_of(state: &AppState, user_id: Uuid) -> Result<UserIdentity, AppError> {
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT username, uid FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    let (username, uid) = row.ok_or_else(|| {
        AppError::internal(anyhow::anyhow!("user {user_id} vanished"))
    })?;
    let uid = u64::try_from(uid).map_err(|_| {
        AppError::internal(anyhow::anyhow!("user {user_id} has out-of-range uid {uid}"))
    })?;
    Ok(UserIdentity {
        user_id,
        username,
        uid: Some(uid),
    })
}

/// Returns the requester's role when they are a member of a `kind='group'`
/// conversation, `None` otherwise (non-member OR non-group). Callers map
/// `None` to a plain 404 so the endpoint is never an existence oracle.
pub(crate) async fn group_role(
    state: &AppState,
    conversation_id: i64,
    user_id: Uuid,
) -> Result<Option<String>, AppError> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT cm.role, c.kind FROM conversation_members cm \
         JOIN conversations c ON c.id = cm.conversation_id \
         WHERE cm.conversation_id = $1 AND cm.user_id = $2",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;
    Ok(row.and_then(|(role, kind)| (kind == "group").then_some(role)))
}

async fn member_role(
    state: &AppState,
    conversation_id: i64,
    user_id: Uuid,
) -> Result<Option<String>, AppError> {
    sqlx::query_scalar(
        "SELECT role FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)
}

async fn group_name(state: &AppState, conversation_id: i64) -> Result<Option<String>, AppError> {
    sqlx::query_scalar("SELECT name FROM conversations WHERE id = $1 AND kind = 'group'")
        .bind(conversation_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::internal)
}

/// Loads the group profile triple `(name, description, avatar)` for a
/// `kind='group'` conversation; `None` for a non-group or missing row.
async fn group_profile(
    state: &AppState,
    conversation_id: i64,
) -> Result<Option<(Option<String>, String, String)>, AppError> {
    sqlx::query_as(
        "SELECT name, description, avatar FROM conversations \
         WHERE id = $1 AND kind = 'group'",
    )
    .bind(conversation_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)
}

// ---------------------------------------------------------------------------
// POST / — create a group
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    #[serde(default)]
    pub invite_usernames: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateGroupResponse {
    pub conversation_id: i64,
    pub name: String,
    pub member_count: i64,
    /// Usernames actually invited (trimmed, lowercased, deduped, self never
    /// included).
    pub invited: Vec<String>,
}

pub async fn create_group(
    State(state): State<AppState>,
    user: AuthUser,
    payload: Result<Json<CreateGroupRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CreateGroupResponse>), AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;

    let name = req.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > GROUP_NAME_MAX_CHARS {
        return Err(AppError::Validation(format!(
            "group name must be 1..={GROUP_NAME_MAX_CHARS} characters"
        )));
    }

    // Resolve and validate EVERY invitee BEFORE creating anything: an unknown
    // username rejects the whole request with 404 and leaves no rows behind.
    // Self is skipped (you are already the owner); repeated usernames collapse.
    let from = identity_of(&state, user.0).await?;
    let mut invited: Vec<String> = Vec::new();
    let mut invitee_ids: Vec<Uuid> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    for raw in &req.invite_usernames {
        let username = raw.trim().to_lowercase();
        if username.is_empty() {
            continue;
        }
        let target: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM users WHERE username = $1 LIMIT 1")
                .bind(&username)
                .fetch_optional(&state.pool)
                .await
                .map_err(AppError::internal)?;
        let Some(target_id) = target else {
            return Err(AppError::PeerNotFound);
        };
        if target_id == user.0 || !seen.insert(target_id) {
            continue;
        }
        invited.push(username);
        invitee_ids.push(target_id);
    }

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    let conversation_id: i64 = sqlx::query_scalar(
        "INSERT INTO conversations (kind, name, created_by) \
         VALUES ('group', $1, $2) RETURNING id",
    )
    .bind(&name)
    .bind(user.0)
    .fetch_one(&mut *tx)
    .await
    .map_err(AppError::internal)?;

    sqlx::query(
        "INSERT INTO conversation_members (conversation_id, user_id, role) \
         VALUES ($1, $2, 'owner')",
    )
    .bind(conversation_id)
    .bind(user.0)
    .execute(&mut *tx)
    .await
    .map_err(AppError::internal)?;

    let mut invite_ids: Vec<Uuid> = Vec::with_capacity(invitee_ids.len());
    for target_id in &invitee_ids {
        let invite_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO group_invites (id, conversation_id, from_user, to_user) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(invite_id)
        .bind(conversation_id)
        .bind(user.0)
        .bind(target_id)
        .execute(&mut *tx)
        .await
        .map_err(AppError::internal)?;
        invite_ids.push(invite_id);
    }
    tx.commit().await.map_err(AppError::internal)?;

    // Best-effort live nudge per invitee (REST remains authoritative).
    for (invite_id, target_id) in invite_ids.iter().zip(invitee_ids.iter()) {
        crate::ws::relay_group_payload(
            &state,
            *target_id,
            Payload::GroupInvited(GroupInvited {
                invite_id: *invite_id,
                conversation_id,
                group_name: name.clone(),
                from: from.clone(),
            }),
        );
    }

    tracing::info!(%conversation_id, owner = %user.0, invites = invitee_ids.len(), "group created");
    Ok((
        StatusCode::CREATED,
        Json(CreateGroupResponse {
            conversation_id,
            name,
            member_count: 1,
            invited,
        }),
    ))
}

// ---------------------------------------------------------------------------
// GET /{conversation_id} — member-only group info
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GroupMemberItem {
    pub user_id: Uuid,
    pub username: String,
    /// Effective display handle (stored display_name, or username when empty).
    pub display_name: String,
    pub avatar: String,
    pub role: String,
    pub joined_at: String,
    /// Group-local XP total and level (M12a); level is the domain curve applied
    /// to `group_xp`.
    pub group_xp: i64,
    pub group_level: i64,
    /// Resolved display title: custom (if set) → role label → level tier.
    pub title: String,
    /// Raw stored custom title, `null` when the member has none.
    pub custom_title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GetGroupResponse {
    pub conversation_id: i64,
    pub name: Option<String>,
    /// Group blurb (empty when unset).
    pub description: String,
    /// Group avatar data-URL (empty when unset).
    pub avatar: String,
    pub my_role: String,
    pub member_count: i64,
    pub members: Vec<GroupMemberItem>,
}

pub async fn get_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path(conversation_id): Path<i64>,
) -> Result<Json<GetGroupResponse>, AppError> {
    let profile = group_profile(&state, conversation_id).await?;
    let Some((name, description, avatar)) = profile else {
        return Err(AppError::ResourceNotFound);
    };
    let my_role = member_role(&state, conversation_id, user.0).await?;
    let Some(my_role) = my_role else {
        return Err(AppError::ResourceNotFound);
    };

    type Row = (
        Uuid,
        String,
        String,
        String,
        String,
        OffsetDateTime,
        Option<String>,
        i64,
        i32,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT cm.user_id, u.username, u.display_name, u.avatar, cm.role, cm.joined_at, \
                cm.custom_title, cm.group_xp, cm.group_level \
         FROM conversation_members cm \
         JOIN users u ON u.id = cm.user_id \
         WHERE cm.conversation_id = $1 \
         ORDER BY CASE cm.role WHEN 'owner' THEN 0 WHEN 'admin' THEN 1 ELSE 2 END, \
                  cm.joined_at ASC",
    )
    .bind(conversation_id)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let member_count = rows.len() as i64;
    let mut members = Vec::with_capacity(rows.len());
    for (user_id, username, display_name, avatar, role, joined_at, custom_title, group_xp, raw_level)
        in rows
    {
        let group_level = i64::from(raw_level);
        let title = resolve_member_title(custom_title.as_deref(), &role, group_level);
        members.push(GroupMemberItem {
            user_id,
            display_name: crate::profile::effective_display_name(&display_name, &username),
            username,
            avatar,
            role,
            joined_at: rfc3339(joined_at)?,
            group_xp,
            group_level,
            title,
            custom_title,
        });
    }

    Ok(Json(GetGroupResponse {
        conversation_id,
        name,
        description,
        avatar,
        my_role,
        member_count,
        members,
    }))
}

// ---------------------------------------------------------------------------
// PATCH /{conversation_id} — update group name / description / avatar
// ---------------------------------------------------------------------------

/// Group settings PATCH body: every field optional; only present fields
/// change. Empty `description`/`avatar` clears them; `name` must stay
/// non-empty.
#[derive(Debug, Deserialize, Default)]
pub struct UpdateGroupRequest {
    /// Trimmed, 1..=32 characters. Owner only.
    #[serde(default)]
    pub name: Option<String>,
    /// ≤200 characters; empty clears. Owner or admin.
    #[serde(default)]
    pub description: Option<String>,
    /// `data:image/{png|jpeg|webp};base64,…` (same rule as user avatars);
    /// empty clears. Owner or admin.
    #[serde(default)]
    pub avatar: Option<String>,
}

/// Generic `{ "ok": true }` acknowledgement for group setting/title writes.
#[derive(Debug, Serialize)]
pub struct GroupOkResponse {
    pub ok: bool,
}

pub async fn update_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path(conversation_id): Path<i64>,
    payload: Result<Json<UpdateGroupRequest>, JsonRejection>,
) -> Result<Json<GroupOkResponse>, AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;

    let requester_role = group_role(&state, conversation_id, user.0).await?;
    let Some(requester_role) = requester_role else {
        return Err(AppError::ResourceNotFound);
    };

    // Name is owner-only; description/avatar are owner OR admin.
    if req.name.is_some() && requester_role != "owner" {
        return Err(AppError::Forbidden(
            "only the group owner may rename the group".to_owned(),
        ));
    }
    if (req.description.is_some() || req.avatar.is_some())
        && requester_role != "owner"
        && requester_role != "admin"
    {
        return Err(AppError::Forbidden(
            "only the owner or an admin may edit the group profile".to_owned(),
        ));
    }

    let Some((cur_name, cur_description, cur_avatar)) =
        group_profile(&state, conversation_id).await?
    else {
        return Err(AppError::ResourceNotFound);
    };

    let name = match req.name {
        Some(raw) => {
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() || trimmed.chars().count() > GROUP_NAME_MAX_CHARS {
                return Err(AppError::Validation(format!(
                    "group name must be 1..={GROUP_NAME_MAX_CHARS} characters"
                )));
            }
            Some(trimmed)
        }
        None => cur_name,
    };

    let description = match req.description {
        Some(raw) => {
            if raw.chars().count() > GROUP_DESCRIPTION_MAX_CHARS {
                return Err(AppError::Validation(format!(
                    "description must be at most {GROUP_DESCRIPTION_MAX_CHARS} characters"
                )));
            }
            raw
        }
        None => cur_description,
    };

    let avatar = match req.avatar {
        Some(raw) => {
            // Empty clears; anything else must be a valid custom data-URL.
            if !raw.is_empty() && !crate::profile::is_valid_custom_avatar(&raw) {
                return Err(AppError::Validation(
                    "avatar must be a data:image/(png|jpeg|webp);base64 URL, or empty to clear"
                        .to_owned(),
                ));
            }
            raw
        }
        None => cur_avatar,
    };

    sqlx::query(
        "UPDATE conversations SET name = $1, description = $2, avatar = $3 \
         WHERE id = $4 AND kind = 'group'",
    )
    .bind(&name)
    .bind(&description)
    .bind(&avatar)
    .bind(conversation_id)
    .execute(&state.pool)
    .await
    .map_err(AppError::internal)?;

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(%conversation_id, actor = %user.0, "group settings updated");
    Ok(Json(GroupOkResponse { ok: true }))
}

// ---------------------------------------------------------------------------
// POST /{conversation_id}/members/{user_id}/title — owner sets a custom title
// ---------------------------------------------------------------------------

/// Body for the title endpoint: `null` (or omitted/empty) clears the title.
#[derive(Debug, Deserialize, Default)]
pub struct SetTitleRequest {
    #[serde(default)]
    pub title: Option<String>,
}

pub async fn set_member_title(
    State(state): State<AppState>,
    user: AuthUser,
    Path((conversation_id, target_id)): Path<(i64, Uuid)>,
    payload: Result<Json<SetTitleRequest>, JsonRejection>,
) -> Result<Json<GroupOkResponse>, AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;

    let requester_role = group_role(&state, conversation_id, user.0).await?;
    let Some(requester_role) = requester_role else {
        return Err(AppError::ResourceNotFound);
    };
    if requester_role != "owner" {
        return Err(AppError::Forbidden(
            "only the group owner may set member titles".to_owned(),
        ));
    }

    let target_role = member_role(&state, conversation_id, target_id).await?;
    let Some(target_role) = target_role else {
        return Err(AppError::ResourceNotFound);
    };
    // The owner's own row always displays as 群主; a custom title is rejected.
    if target_role == "owner" {
        return Err(AppError::Validation(
            "cannot set a custom title on the group owner".to_owned(),
        ));
    }

    // Trim; empty/null clears (stored as NULL).
    let title = req
        .title
        .map(|raw| raw.trim().to_owned())
        .filter(|trimmed| !trimmed.is_empty());
    if title
        .as_ref()
        .is_some_and(|t| t.chars().count() > CUSTOM_TITLE_MAX_CHARS)
    {
        return Err(AppError::Validation(format!(
            "title must be at most {CUSTOM_TITLE_MAX_CHARS} characters"
        )));
    }

    sqlx::query(
        "UPDATE conversation_members SET custom_title = $1 \
         WHERE conversation_id = $2 AND user_id = $3",
    )
    .bind(title.as_deref())
    .bind(conversation_id)
    .bind(target_id)
    .execute(&state.pool)
    .await
    .map_err(AppError::internal)?;

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(%conversation_id, actor = %user.0, target = %target_id, "group member title set");
    Ok(Json(GroupOkResponse { ok: true }))
}

// ---------------------------------------------------------------------------
// POST /{conversation_id}/invites — owner/admin invites a username
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct InviteRequest {
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct InviteResponse {
    pub invite_id: Uuid,
}

pub async fn invite_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(conversation_id): Path<i64>,
    payload: Result<Json<InviteRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<InviteResponse>), AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;

    let requester_role = group_role(&state, conversation_id, user.0).await?;
    let Some(requester_role) = requester_role else {
        return Err(AppError::ResourceNotFound);
    };
    if requester_role != "owner" && requester_role != "admin" {
        return Err(AppError::Forbidden(
            "only the owner or an admin may invite".to_owned(),
        ));
    }

    let username = req.username.trim().to_lowercase();
    if username.is_empty() {
        return Err(AppError::Validation("username is required".to_owned()));
    }
    let target: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE username = $1 LIMIT 1")
            .bind(&username)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    let Some(target_id) = target else {
        return Err(AppError::PeerNotFound);
    };
    if target_id == user.0 {
        return Err(AppError::Validation(
            "you are already a member of this group".to_owned(),
        ));
    }

    let already_member: Option<i64> = sqlx::query_scalar(
        "SELECT 1::int8 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(target_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;
    if already_member.is_some() {
        return Err(AppError::Conflict(ConflictKind::AlreadyMember));
    }

    let pending: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM group_invites \
         WHERE conversation_id = $1 AND to_user = $2 AND status = 'pending'",
    )
    .bind(conversation_id)
    .bind(target_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;
    if pending.is_some() {
        return Err(AppError::Conflict(ConflictKind::InviteAlreadyPending));
    }

    let candidate = Uuid::now_v7();
    let invite_id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO group_invites (id, conversation_id, from_user, to_user) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (conversation_id, to_user) WHERE status = 'pending' DO NOTHING \
         RETURNING id",
    )
    .bind(candidate)
    .bind(conversation_id)
    .bind(user.0)
    .bind(target_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;
    let Some(invite_id) = invite_id else {
        // Lost a race: a pending invite appeared concurrently.
        return Err(AppError::Conflict(ConflictKind::InviteAlreadyPending));
    };

    let name = group_name(&state, conversation_id).await?.unwrap_or_default();
    let from = identity_of(&state, user.0).await?;
    crate::ws::relay_group_payload(
        &state,
        target_id,
        Payload::GroupInvited(GroupInvited {
            invite_id,
            conversation_id,
            group_name: name,
            from,
        }),
    );

    tracing::info!(%conversation_id, inviter = %user.0, invitee = %target_id, "group invite sent");
    Ok((StatusCode::CREATED, Json(InviteResponse { invite_id })))
}

// ---------------------------------------------------------------------------
// GET /invites — my pending invites
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct InviteFrom {
    pub user_id: Uuid,
    pub username: String,
    /// Effective display handle (stored display_name, or username when empty).
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct InviteListItem {
    pub invite_id: Uuid,
    pub conversation_id: i64,
    pub group_name: String,
    pub from: InviteFrom,
}

pub async fn list_my_invites(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<InviteListItem>>, AppError> {
    type Row = (Uuid, i64, Option<String>, Uuid, String, String);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT gi.id, gi.conversation_id, c.name, u.id, u.username, u.display_name \
         FROM group_invites gi \
         JOIN conversations c ON c.id = gi.conversation_id \
         JOIN users u ON u.id = gi.from_user \
         WHERE gi.to_user = $1 AND gi.status = 'pending' \
         ORDER BY gi.created_at DESC",
    )
    .bind(user.0)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let items = rows
        .into_iter()
        .map(
            |(invite_id, conversation_id, group_name, from_id, from_username, from_display)| {
                InviteListItem {
                    invite_id,
                    conversation_id,
                    group_name: group_name.unwrap_or_default(),
                    from: InviteFrom {
                        user_id: from_id,
                        display_name: crate::profile::effective_display_name(
                            &from_display,
                            &from_username,
                        ),
                        username: from_username,
                    },
                }
            },
        )
        .collect();
    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// POST /invites/{invite_id}/accept — invitee joins
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct AcceptInviteResponse {
    pub conversation_id: i64,
}

pub async fn accept_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(invite_id): Path<Uuid>,
) -> Result<Json<AcceptInviteResponse>, AppError> {
    let invite: Option<(i64, Uuid, String)> = sqlx::query_as(
        "SELECT conversation_id, to_user, status FROM group_invites WHERE id = $1",
    )
    .bind(invite_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;
    let Some((conversation_id, to_user, status)) = invite else {
        return Err(AppError::ResourceNotFound);
    };
    if to_user != user.0 {
        return Err(AppError::ResourceNotFound);
    }
    if status != "pending" {
        return Err(AppError::Conflict(ConflictKind::InviteNotPending));
    }

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    // Pending-guarded transition: a concurrent decline/accept wins instead.
    let updated: Option<Uuid> = sqlx::query_scalar(
        "UPDATE group_invites SET status = 'accepted', responded_at = now() \
         WHERE id = $1 AND to_user = $2 AND status = 'pending' RETURNING id",
    )
    .bind(invite_id)
    .bind(user.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AppError::internal)?;
    if updated.is_none() {
        return Err(AppError::Conflict(ConflictKind::InviteNotPending));
    }

    sqlx::query(
        "INSERT INTO conversation_members (conversation_id, user_id, role) \
         VALUES ($1, $2, 'member') \
         ON CONFLICT (conversation_id, user_id) DO NOTHING",
    )
    .bind(conversation_id)
    .bind(user.0)
    .execute(&mut *tx)
    .await
    .map_err(AppError::internal)?;

    // Drop any sibling pending invites for this (conversation, user): once
    // accepted, an outstanding duplicate must not linger as a dead notice.
    sqlx::query(
        "DELETE FROM group_invites \
         WHERE conversation_id = $1 AND to_user = $2 AND status = 'pending'",
    )
    .bind(conversation_id)
    .bind(user.0)
    .execute(&mut *tx)
    .await
    .map_err(AppError::internal)?;

    tx.commit().await.map_err(AppError::internal)?;

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(%invite_id, user = %user.0, %conversation_id, "group invite accepted");
    Ok(Json(AcceptInviteResponse { conversation_id }))
}

// ---------------------------------------------------------------------------
// POST /invites/{invite_id}/decline — invitee declines
// ---------------------------------------------------------------------------

pub async fn decline_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(invite_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    // Invitee-only; a non-invitee (or non-pending invite) sees a plain 404 so
    // invite ids are never an oracle.
    let updated = sqlx::query(
        "UPDATE group_invites SET status = 'declined', responded_at = now() \
         WHERE id = $1 AND to_user = $2 AND status = 'pending'",
    )
    .bind(invite_id)
    .bind(user.0)
    .execute(&state.pool)
    .await
    .map_err(AppError::internal)?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::ResourceNotFound);
    }

    tracing::info!(%invite_id, user = %user.0, "group invite declined");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// POST /{conversation_id}/members/{user_id}/kick
// ---------------------------------------------------------------------------

pub async fn kick_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((conversation_id, target_id)): Path<(i64, Uuid)>,
) -> Result<StatusCode, AppError> {
    let requester_role = group_role(&state, conversation_id, user.0).await?;
    let Some(requester_role) = requester_role else {
        return Err(AppError::ResourceNotFound);
    };
    if requester_role != "owner" && requester_role != "admin" {
        return Err(AppError::Forbidden(
            "only the owner or an admin may kick".to_owned(),
        ));
    }

    let target_role = member_role(&state, conversation_id, target_id).await?;
    let Some(target_role) = target_role else {
        return Err(AppError::ResourceNotFound);
    };
    if target_role == "owner" {
        return Err(AppError::Forbidden(
            "the group owner cannot be kicked".to_owned(),
        ));
    }
    if requester_role == "admin" && target_role == "admin" {
        return Err(AppError::Forbidden(
            "an admin cannot kick another admin".to_owned(),
        ));
    }

    let removed = sqlx::query(
        "DELETE FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(target_id)
    .execute(&state.pool)
    .await
    .map_err(AppError::internal)?
    .rows_affected();
    if removed == 0 {
        return Err(AppError::ResourceNotFound);
    }

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(%conversation_id, actor = %user.0, target = %target_id, "group member kicked");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// POST /{conversation_id}/members/{user_id}/role — owner sets admin/member
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SetRoleRequest {
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct GroupMutationResponse {
    pub conversation_id: i64,
}

pub async fn set_role(
    State(state): State<AppState>,
    user: AuthUser,
    Path((conversation_id, target_id)): Path<(i64, Uuid)>,
    payload: Result<Json<SetRoleRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<GroupMutationResponse>), AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;
    let role = req.role.trim();
    if role != "admin" && role != "member" {
        return Err(AppError::Validation(
            "role must be \"admin\" or \"member\"".to_owned(),
        ));
    }

    let requester_role = group_role(&state, conversation_id, user.0).await?;
    let Some(requester_role) = requester_role else {
        return Err(AppError::ResourceNotFound);
    };
    if requester_role != "owner" {
        return Err(AppError::Forbidden(
            "only the group owner may change roles".to_owned(),
        ));
    }

    let target_role = member_role(&state, conversation_id, target_id).await?;
    let Some(target_role) = target_role else {
        return Err(AppError::ResourceNotFound);
    };
    if target_role == "owner" {
        return Err(AppError::Validation(
            "cannot change the owner's role; transfer ownership first".to_owned(),
        ));
    }

    sqlx::query(
        "UPDATE conversation_members SET role = $1 WHERE conversation_id = $2 AND user_id = $3",
    )
    .bind(role)
    .bind(conversation_id)
    .bind(target_id)
    .execute(&state.pool)
    .await
    .map_err(AppError::internal)?;

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(%conversation_id, actor = %user.0, target = %target_id, role, "group role changed");
    Ok((
        StatusCode::OK,
        Json(GroupMutationResponse { conversation_id }),
    ))
}

// ---------------------------------------------------------------------------
// POST /{conversation_id}/transfer — owner hands ownership to a member
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TransferRequest {
    pub user_id: Uuid,
}

pub async fn transfer_ownership(
    State(state): State<AppState>,
    user: AuthUser,
    Path(conversation_id): Path<i64>,
    payload: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<GroupMutationResponse>), AppError> {
    let Json(req) = payload.map_err(|rejection| AppError::BadRequest(rejection.body_text()))?;

    let requester_role = group_role(&state, conversation_id, user.0).await?;
    let Some(requester_role) = requester_role else {
        return Err(AppError::ResourceNotFound);
    };
    if requester_role != "owner" {
        return Err(AppError::Forbidden(
            "only the group owner may transfer ownership".to_owned(),
        ));
    }
    if req.user_id == user.0 {
        return Err(AppError::Validation(
            "you are already the owner".to_owned(),
        ));
    }
    // The successor must be a CURRENT member (no dangling owner).
    let target_role = member_role(&state, conversation_id, req.user_id).await?;
    if target_role.is_none() {
        return Err(AppError::ResourceNotFound);
    }

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    // Documented decision: the outgoing owner is demoted to `member` (not
    // `admin`) — ownership transfer is a clean hand-off, not a promotion.
    sqlx::query(
        "UPDATE conversation_members SET role = 'member' \
         WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(user.0)
    .execute(&mut *tx)
    .await
    .map_err(AppError::internal)?;
    sqlx::query(
        "UPDATE conversation_members SET role = 'owner' \
         WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(req.user_id)
    .execute(&mut *tx)
    .await
    .map_err(AppError::internal)?;
    tx.commit().await.map_err(AppError::internal)?;

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(%conversation_id, from = %user.0, to = %req.user_id, "group ownership transferred");
    Ok((
        StatusCode::OK,
        Json(GroupMutationResponse { conversation_id }),
    ))
}

// ---------------------------------------------------------------------------
// POST /{conversation_id}/leave — non-owner member leaves
// ---------------------------------------------------------------------------

pub async fn leave_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path(conversation_id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let role = group_role(&state, conversation_id, user.0).await?;
    let Some(role) = role else {
        return Err(AppError::ResourceNotFound);
    };
    if role == "owner" {
        // A group must never be ownerless: the owner transfers first.
        return Err(AppError::Validation(
            "transfer ownership before leaving".to_owned(),
        ));
    }

    sqlx::query("DELETE FROM conversation_members WHERE conversation_id = $1 AND user_id = $2")
        .bind(conversation_id)
        .bind(user.0)
        .execute(&state.pool)
        .await
        .map_err(AppError::internal)?;

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(%conversation_id, user = %user.0, "group member left");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Group XP economy (called from the WS layer; best-effort)
// ---------------------------------------------------------------------------

/// Group-local messaging XP: +[`GROUP_XP_PER_MESSAGE`] for one plaintext text
/// message sent in a `kind='group'` conversation, capped at
/// [`GROUP_XP_DAILY_CAP`] per `(conversation, user)` per UTC day.
///
/// Mirrors the global `xp_accounts` writer pattern: the member row is locked
/// `FOR UPDATE`, the daily budget rolls over when `msg_xp_date` is not today,
/// and `group_level` is recomputed through the shared domain curve inside the
/// same UPDATE. Secret/direct conversations award nothing (early return).
/// Called as a fire-and-forget task after the send commits + acks, so any
/// failure here degrades to a logged no-op and never fails the send.
pub(crate) async fn award_group_message_xp(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    conversation_id: i64,
) -> anyhow::Result<()> {
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_optional(pool)
        .await?;
    let Some(kind) = kind else {
        return Ok(()); // conversation vanished — nothing to do
    };
    if kind != "group" {
        return Ok(()); // direct/secret chats earn no group XP
    }

    let today = OffsetDateTime::now_utc().date();
    let mut tx = pool.begin().await?;
    // Lazily nothing to create: membership rows exist for every member. Look
    // the row up under a lock so concurrent sends serialize on one writer.
    let row: Option<(i64, Option<Date>, i32)> = sqlx::query_as(
        "SELECT group_xp, msg_xp_date, msg_xp_today FROM conversation_members \
         WHERE conversation_id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((group_xp, msg_xp_date, msg_xp_today)) = row else {
        tx.commit().await?;
        return Ok(()); // sender is no longer a member — award nothing
    };

    let used_today = if msg_xp_date == Some(today) {
        i64::from(msg_xp_today)
    } else {
        0
    };
    let available = GROUP_XP_DAILY_CAP - used_today;
    if available <= 0 {
        tx.commit().await?;
        return Ok(()); // daily cap reached
    }
    let actual = GROUP_XP_PER_MESSAGE.min(available);
    let new_xp = group_xp + actual;
    let new_level = group_level_from_xp(new_xp);
    let new_today = (used_today + actual) as i32;

    sqlx::query(
        "UPDATE conversation_members \
         SET group_xp = $1, group_level = $2, msg_xp_date = $3, msg_xp_today = $4 \
         WHERE conversation_id = $5 AND user_id = $6",
    )
    .bind(new_xp)
    .bind(new_level)
    .bind(today)
    .bind(new_today)
    .bind(conversation_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::debug!(%user_id, %conversation_id, actual, new_xp, new_level, "group xp awarded");
    Ok(())
}
