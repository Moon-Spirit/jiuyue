//! M13a per-group file storage (`/api/groups/*` file endpoints).
//!
//! Every `kind='group'` conversation gets 1 GiB of free file storage
//! ([`FREE_GROUP_BYTES`]). The quota is enforced as a **pricing rule**, never
//! as a hard block:
//!
//! * current usage (sum of non-expired `bytes`) `< 1 GiB` → the new upload is
//!   permanent (`expires_at = NULL`);
//! * current usage `>= 1 GiB` → the new upload is TEMPORARY, valid for
//!   [`TEMPORARY_TTL_DAYS`] days (`expires_at = now + 7d`).
//!
//! Uploads are always accepted (up to the per-file streaming cap
//! [`crate::state::AppState::group_file_max_bytes`]); only the 7-day rule
//! changes. Expired files vanish from listings and downloads and are swept
//! from disk by [`spawn_sweeper`].
//!
//! | Method | Path                                    | Auth        | Semantics                                  |
//! |--------|-----------------------------------------|-------------|--------------------------------------------|
//! | POST   | `/api/groups/{id}/files`                | member      | raw-body upload → 201 metadata             |
//! | GET    | `/api/groups/{id}/files`                | member      | `{usage_bytes, quota_bytes, files:[…]}`    |
//! | GET    | `/api/groups/files/{file_id}`           | member      | serve bytes as an attachment               |
//! | POST   | `/api/groups/files/{file_id}/delete`    | uploader/owner/admin | delete row + disk bytes → 204      |
//!
//! Access is never an existence oracle: non-members (and non-group
//! conversations) get a plain 404; an authenticated member who may not delete
//! gets 403. Bytes are stored verbatim under
//! `{JIUYUE_MEDIA_DIR}/group-files/{conversation_id}/{file_id}.{ext}` — no
//! sniffing, no transcoding, any file type is allowed (unlike M8 media).
//!
//! `group.updated` is pushed to members after upload/delete so clients refetch
//! the listing; the frame is never persisted and the REST surface stays
//! authoritative.

use crate::auth::extract::AuthUser;
use crate::error::AppError;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Serialize;
use std::path::{Path as StdPath, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// Free per-group file storage: 1 GiB, expressed in bytes.
pub const FREE_GROUP_BYTES: i64 = 1024 * 1024 * 1024;

/// How long a temporary (over-quota) upload stays valid.
pub const TEMPORARY_TTL_DAYS: i64 = 7;

/// A listing returns at most this many newest non-expired entries.
const LIST_MAX: i64 = 200;

/// First sweeper pass fires this long after boot.
const SWEEP_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Interval between sweeper passes.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Whether an `expires_at` timestamp lies strictly in the past of `now`.
///
/// `None` (permanent) is never expired; an `expires_at` exactly equal to `now`
/// is not yet expired (the DB sweeper uses the same strict `<`).
pub(crate) fn is_expired(expires_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    expires_at.is_some_and(|exp| exp < now)
}

fn rfc3339(value: OffsetDateTime) -> Result<String, AppError> {
    value.format(&Rfc3339).map_err(AppError::internal)
}

/// Declared `Content-Type` reduced to its essence; any type is accepted, and a
/// missing/invalid/empty header falls back to `application/octet-stream`.
fn declared_mime(raw: Option<&HeaderValue>) -> String {
    raw.and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|essence| !essence.is_empty())
        .map(|essence| essence.chars().take(255).collect::<String>())
        .unwrap_or_else(|| "application/octet-stream".to_owned())
}

/// On-disk extension derived from the sanitized original name: keep only ASCII
/// alphanumerics (portable across Windows/macOS/Linux), lowercase, cap 16
/// chars, fall back to `bin`.
fn extension_of(name: &str) -> String {
    let raw_ext = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    let cleaned: String = raw_ext
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(16)
        .collect();
    if cleaned.is_empty() {
        "bin".to_owned()
    } else {
        cleaned.to_ascii_lowercase()
    }
}

/// Percent-encodes every non-unreserved byte (RFC 3986) for `filename*`.
fn percent_encode_utf8(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        let unreserved =
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Filename safe for a `Content-Disposition` header: `"`/`\` and control
/// characters become `_` (the stored name is already path-sanitized).
fn header_safe_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == '"' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Sum of non-expired file bytes for one conversation (the quota basis).
async fn current_usage(
    state: &AppState,
    conversation_id: i64,
    now: OffsetDateTime,
) -> Result<i64, AppError> {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(bytes), 0)::int8 FROM group_files \
         WHERE conversation_id = $1 AND (expires_at IS NULL OR expires_at > $2)",
    )
    .bind(conversation_id)
    .bind(now)
    .fetch_one(&state.pool)
    .await
    .map_err(AppError::internal)
}

/// Best-effort removal of a stored file and, when it becomes empty, its parent
/// directory. Returns whether the file itself was removed.
async fn remove_file_and_empty_dir(storage_path: &str) -> bool {
    let path = StdPath::new(storage_path);
    if let Err(err) = tokio::fs::remove_file(path).await {
        tracing::debug!(path = %storage_path, error = %err, "group file disk delete skipped");
        return false;
    }
    if let Some(parent) = path.parent() {
        // Only succeeds when the directory is empty; a non-empty dir errors
        // and is intentionally left alone.
        let _ = tokio::fs::remove_dir(parent).await;
    }
    true
}

// ---------------------------------------------------------------------------
// POST /{conversation_id}/files — raw streaming upload
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct UploadFileResponse {
    pub file_id: Uuid,
    pub name: String,
    pub mime: String,
    pub bytes: i64,
    pub created_at: String,
    /// RFC3339 for a temporary (over-quota) upload, `null` when permanent.
    pub expires_at: Option<String>,
}

pub async fn upload_file(
    State(state): State<AppState>,
    user: AuthUser,
    Path(conversation_id): Path<i64>,
    request: Request,
) -> Result<(StatusCode, Json<UploadFileResponse>), AppError> {
    // Member-only; non-member or non-group both answer a plain 404.
    if crate::groups::group_role(&state, conversation_id, user.0)
        .await?
        .is_none()
    {
        return Err(AppError::ResourceNotFound);
    }

    let (parts, body) = request.into_parts();
    let mime = declared_mime(parts.headers.get(CONTENT_TYPE));
    let name = crate::media::sanitize_file_name(
        parts
            .headers
            .get("x-file-name")
            .and_then(|value| value.to_str().ok()),
    );

    let cap = state.group_file_max_bytes;
    let now = OffsetDateTime::now_utc();
    // Quota is evaluated BEFORE this upload lands: usage >= 1 GiB makes the
    // new row temporary for 7 days, otherwise permanent (NULL).
    let usage = current_usage(&state, conversation_id, now).await?;
    let expires_at = if usage >= FREE_GROUP_BYTES {
        Some(now + time::Duration::days(TEMPORARY_TTL_DAYS))
    } else {
        None
    };

    let file_id = Uuid::now_v7();
    let ext = extension_of(&name);
    let dir: PathBuf = state
        .media_dir
        .join("group-files")
        .join(conversation_id.to_string());
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(AppError::internal)?;
    let path = dir.join(format!("{file_id}.{ext}"));

    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(AppError::internal)?;

    // Stream to disk, aborting the moment the running size crosses the cap so
    // an oversized body never sits whole in RAM.
    let mut stream = body.into_data_stream();
    let mut total: u64 = 0;
    loop {
        match stream.next().await {
            None => break,
            Some(Err(err)) => {
                drop(file);
                let _ = tokio::fs::remove_file(&path).await;
                return Err(AppError::internal(err));
            }
            Some(Ok(chunk)) => {
                total += chunk.len() as u64;
                if total > cap {
                    drop(file);
                    let _ = tokio::fs::remove_file(&path).await;
                    return Err(AppError::PayloadTooLarge);
                }
                if let Err(err) = file.write_all(&chunk).await {
                    drop(file);
                    let _ = tokio::fs::remove_file(&path).await;
                    return Err(AppError::internal(err));
                }
            }
        }
    }
    if total == 0 {
        drop(file);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(AppError::BadRequest("empty request body".to_owned()));
    }
    file.flush().await.map_err(AppError::internal)?;
    drop(file);

    let bytes = total as i64;
    let storage_path = path.to_string_lossy().into_owned();
    if let Err(err) = sqlx::query(
        "INSERT INTO group_files \
         (id, conversation_id, uploader_id, name, mime, bytes, storage_path, created_at, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(file_id)
    .bind(conversation_id)
    .bind(user.0)
    .bind(&name)
    .bind(&mime)
    .bind(bytes)
    .bind(&storage_path)
    .bind(now)
    .bind(expires_at)
    .execute(&state.pool)
    .await
    {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(AppError::internal(err));
    }

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(
        %file_id,
        %conversation_id,
        uploader = %user.0,
        bytes,
        temporary = expires_at.is_some(),
        "group file uploaded"
    );

    Ok((
        StatusCode::CREATED,
        Json(UploadFileResponse {
            file_id,
            name,
            mime,
            bytes,
            created_at: rfc3339(now)?,
            expires_at: expires_at.map(rfc3339).transpose()?,
        }),
    ))
}

// ---------------------------------------------------------------------------
// GET /{conversation_id}/files — usage + non-expired listing
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct UploaderInfo {
    pub user_id: Uuid,
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct GroupFileItem {
    pub file_id: Uuid,
    pub name: String,
    pub mime: String,
    pub bytes: i64,
    pub uploader: UploaderInfo,
    pub created_at: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListFilesResponse {
    pub usage_bytes: i64,
    pub quota_bytes: i64,
    pub files: Vec<GroupFileItem>,
}

pub async fn list_files(
    State(state): State<AppState>,
    user: AuthUser,
    Path(conversation_id): Path<i64>,
) -> Result<Json<ListFilesResponse>, AppError> {
    if crate::groups::group_role(&state, conversation_id, user.0)
        .await?
        .is_none()
    {
        return Err(AppError::ResourceNotFound);
    }

    let now = OffsetDateTime::now_utc();
    let usage = current_usage(&state, conversation_id, now).await?;

    type Row = (
        Uuid,
        String,
        String,
        i64,
        Uuid,
        String,
        String,
        OffsetDateTime,
        Option<OffsetDateTime>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT gf.id, gf.name, gf.mime, gf.bytes, gf.uploader_id, \
                u.username, u.display_name, gf.created_at, gf.expires_at \
         FROM group_files gf \
         JOIN users u ON u.id = gf.uploader_id \
         WHERE gf.conversation_id = $1 \
           AND (gf.expires_at IS NULL OR gf.expires_at > $2) \
         ORDER BY gf.created_at DESC, gf.id DESC \
         LIMIT $3",
    )
    .bind(conversation_id)
    .bind(now)
    .bind(LIST_MAX)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let mut files = Vec::with_capacity(rows.len());
    for (file_id, name, mime, bytes, uploader_id, username, display_name, created_at, expires_at) in
        rows
    {
        files.push(GroupFileItem {
            file_id,
            name,
            mime,
            bytes,
            uploader: UploaderInfo {
                user_id: uploader_id,
                display_name: crate::profile::effective_display_name(&display_name, &username),
                username,
            },
            created_at: rfc3339(created_at)?,
            expires_at: expires_at.map(rfc3339).transpose()?,
        });
    }

    Ok(Json(ListFilesResponse {
        usage_bytes: usage,
        quota_bytes: FREE_GROUP_BYTES,
        files,
    }))
}

// ---------------------------------------------------------------------------
// GET /files/{file_id} — member-only byte serving
// ---------------------------------------------------------------------------

pub async fn download_file(
    State(state): State<AppState>,
    user: AuthUser,
    Path(file_id): Path<Uuid>,
) -> Result<Response, AppError> {
    // Resolve conversation from the row; membership decides access, so neither
    // a missing row nor a foreign conversation ever leaks existence.
    let row: Option<(i64, String, String, String, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT conversation_id, name, mime, storage_path, expires_at \
         FROM group_files WHERE id = $1",
    )
    .bind(file_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;
    let Some((conversation_id, name, mime, storage_path, expires_at)) = row else {
        return Err(AppError::ResourceNotFound);
    };
    if is_expired(expires_at, OffsetDateTime::now_utc()) {
        return Err(AppError::ResourceNotFound);
    }
    if crate::groups::group_role(&state, conversation_id, user.0)
        .await?
        .is_none()
    {
        return Err(AppError::ResourceNotFound);
    }

    // Full body (no Range needed): files are small enough that streaming
    // ranges would buy nothing over a single read.
    let bytes = match tokio::fs::read(&storage_path).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(%file_id, error = %err, "group file row has no bytes on disk");
            return Err(AppError::ResourceNotFound);
        }
    };

    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
        let safe = header_safe_name(&name);
        let ascii_fallback: String = safe
            .chars()
            .map(|c| if c.is_ascii() { c } else { '_' })
            .collect();
        // RFC 6266/5987: keep an ASCII fallback AND the UTF-8 name so CJK
        // filenames survive the download instead of arriving mojibake'd.
        let disposition = format!(
            "attachment; filename=\"{}\"; filename*=UTF-8''{}",
            ascii_fallback,
            percent_encode_utf8(&safe)
        );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    Ok(response)
}

// ---------------------------------------------------------------------------
// POST /files/{file_id}/delete — uploader or owner/admin
// ---------------------------------------------------------------------------

pub async fn delete_file(
    State(state): State<AppState>,
    user: AuthUser,
    Path(file_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let row: Option<(i64, Uuid, String)> = sqlx::query_as(
        "SELECT conversation_id, uploader_id, storage_path FROM group_files WHERE id = $1",
    )
    .bind(file_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;
    let Some((conversation_id, uploader_id, storage_path)) = row else {
        return Err(AppError::ResourceNotFound);
    };

    // Membership first: a non-member gets a plain 404 (no oracle). A member
    // who is neither the uploader nor owner/admin gets an explicit 403.
    let role = crate::groups::group_role(&state, conversation_id, user.0).await?;
    let Some(role) = role else {
        return Err(AppError::ResourceNotFound);
    };
    let allowed = user.0 == uploader_id || role == "owner" || role == "admin";
    if !allowed {
        return Err(AppError::Forbidden(
            "only the uploader, the group owner or an admin may delete this file".to_owned(),
        ));
    }

    sqlx::query("DELETE FROM group_files WHERE id = $1")
        .bind(file_id)
        .execute(&state.pool)
        .await
        .map_err(AppError::internal)?;
    remove_file_and_empty_dir(&storage_path).await;

    crate::ws::broadcast_group_updated(&state, conversation_id).await;
    tracing::info!(%file_id, %conversation_id, actor = %user.0, "group file deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Expiry sweeper
// ---------------------------------------------------------------------------

/// Spawns the hourly expired-file sweeper (first pass ~1 minute after boot).
///
/// Each pass deletes rows whose `expires_at` is in the past and best-effort
/// removes their bytes + empty parent dirs. Any DB error is logged and the
/// loop continues: a transient outage must never kill the worker.
pub fn spawn_sweeper(state: AppState) {
    tokio::spawn(async move {
        let start = tokio::time::Instant::now() + SWEEP_INITIAL_DELAY;
        let mut ticker = tokio::time::interval_at(start, SWEEP_INTERVAL);
        loop {
            ticker.tick().await;
            match sweep_expired(&state).await {
                Ok((rows, files)) => {
                    tracing::debug!(rows, files, "group-file sweeper pass complete");
                }
                Err(err) => {
                    tracing::warn!(error = %format!("{err:#}"), "group-file sweeper pass failed");
                }
            }
        }
    });
}

/// One sweeper pass: returns `(rows_deleted, files_removed)`.
async fn sweep_expired(state: &AppState) -> anyhow::Result<(usize, usize)> {
    let paths: Vec<String> = sqlx::query_scalar(
        "DELETE FROM group_files \
         WHERE expires_at IS NOT NULL AND expires_at < now() \
         RETURNING storage_path",
    )
    .fetch_all(&state.pool)
    .await?;
    let rows = paths.len();
    let mut files = 0usize;
    for path in &paths {
        if remove_file_and_empty_dir(path).await {
            files += 1;
        }
    }
    Ok((rows, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).expect("valid timestamp")
    }

    #[test]
    fn is_expired_only_for_strictly_past_timestamps() {
        let now = at(1_700_000_000);
        assert!(!is_expired(None, now), "permanent files never expire");
        assert!(!is_expired(Some(now + time::Duration::seconds(1)), now));
        assert!(
            !is_expired(Some(now), now),
            "exactly now is not yet expired (matches SQL `expires_at < now()`)"
        );
        assert!(is_expired(Some(now - time::Duration::seconds(1)), now));
    }

    #[test]
    fn extension_is_sanitized_and_falls_back_to_bin() {
        assert_eq!(extension_of("photo.PNG"), "png");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of("no-extension"), "bin");
        assert_eq!(extension_of("trailing."), "bin");
        // Path separators never survive into an extension; invalid chars drop.
        assert_eq!(extension_of("weird.t:x*t"), "txt");
        // CJK extensions have no ASCII alphanumerics → bin.
        assert_eq!(extension_of("文件.文档"), "bin");
        assert_eq!(extension_of("long.ABCDEFGHIJKLMNOPQRSTUV"), "abcdefghijklmnop");
    }

    #[test]
    fn declared_mime_defaults_and_strips_params() {
        let value = |raw: &str| HeaderValue::from_str(raw).unwrap();
        assert_eq!(declared_mime(None), "application/octet-stream");
        assert_eq!(
            declared_mime(Some(&value("text/plain; charset=utf-8"))),
            "text/plain"
        );
        assert_eq!(declared_mime(Some(&value("  "))), "application/octet-stream");
        assert_eq!(
            declared_mime(Some(&value("application/pdf"))),
            "application/pdf"
        );
    }

    #[test]
    fn percent_encoded_filename_star_roundtrips_cjk() {
        // RFC 5987 form browsers read: filename*=UTF-8''%XX%XX...
        assert_eq!(percent_encode_utf8("a b.txt"), "a%20b.txt");
        assert_eq!(percent_encode_utf8("测试.txt"), "%E6%B5%8B%E8%AF%95.txt");
    }
    #[test]
    fn header_safe_name_strips_quotes_and_controls() {
        assert_eq!(header_safe_name(r#"a"b\c"#), "a_b_c");
        assert_eq!(header_safe_name("正常.png"), "正常.png");
    }
}
