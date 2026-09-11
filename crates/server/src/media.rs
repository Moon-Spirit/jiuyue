//! M8 media (image/video) HTTP surface: upload + public serving.
//!
//! | Method | Path             | Auth   | Semantics                                     |
//! |--------|------------------|--------|-----------------------------------------------|
//! | POST   | `/api/media`     | Bearer | Raw-body upload → 201 `{media_id, kind, ...}` |
//! | GET    | `/api/media/{id}`| public | Serve bytes, HTTP Range capable               |
//!
//! ## Upload
//!
//! Raw request body (NOT multipart) with a `Content-Type` from the accepted
//! set and an optional `X-File-Name`. The declared type is **not trusted**:
//! the first bytes are magic-byte sniffed and any disagreement with
//! `Content-Type` is rejected with `415 unsupported_type`. Size caps are
//! enforced **while streaming** (a 1 GiB upload is aborted as soon as the
//! running byte count crosses the cap) so videos never sit whole in RAM.
//! Bytes are stored verbatim under `{JIUYUE_MEDIA_DIR}/{yyyy}/{mm}/{id}.{ext}`
//! — no composition/transcoding/re-encoding ever happens.
//!
//! ## Serving is a capability URL
//!
//! `GET /api/media/{id}` is deliberately **unauthenticated**: the media id is
//! an unguessable UUIDv7, so possession of the URL is the capability, exactly
//! like Telegram CDN links. That is what lets a plain `<video src=...>` tag
//! seek without attaching an `Authorization` header. No listing/discovery
//! endpoint exists, so ids are only learned by receiving a message that
//! carries them.

use crate::auth::extract::AuthUser;
use crate::error::AppError;
use crate::state::AppState;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use futures_util::StreamExt;
use serde::Serialize;
use std::path::PathBuf;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

/// Bytes read up-front to sniff the format (all supported magics fit in 12).
const SNIFF_LEN: usize = 16;

/// A sniffed media kind + canonical mime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Detected {
    kind: &'static str,
    mime: &'static str,
}

/// Magic-byte type detection. Returns `None` for anything unsupported (or
/// truncated); callers map that to `415`.
fn sniff(prefix: &[u8]) -> Option<Detected> {
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if prefix.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(Detected {
            kind: "image",
            mime: "image/png",
        });
    }
    // JPEG: FF D8 FF
    if prefix.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Detected {
            kind: "image",
            mime: "image/jpeg",
        });
    }
    // GIF: "GIF87a" | "GIF89a"
    if prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a") {
        return Some(Detected {
            kind: "image",
            mime: "image/gif",
        });
    }
    // WebP: "RIFF" .... "WEBP" (bytes 0-3 and 8-11)
    if prefix.len() >= 12 && &prefix[0..4] == b"RIFF" && &prefix[8..12] == b"WEBP" {
        return Some(Detected {
            kind: "image",
            mime: "image/webp",
        });
    }
    // WebM / Matroska EBML header: 1A 45 DF A3
    if prefix.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(Detected {
            kind: "video",
            mime: "video/webm",
        });
    }
    // ISO base media file format: `ftyp` box at offset 4, brand at 8..12.
    if prefix.len() >= 12 && &prefix[4..8] == b"ftyp" {
        let brand = String::from_utf8_lossy(&prefix[8..12]);
        let brand = brand.trim_end();
        return match brand {
            "isom" | "iso2" | "mp41" | "mp42" => Some(Detected {
                kind: "video",
                mime: "video/mp4",
            }),
            // `qt  ` and `M4V ` trim to these.
            "qt" | "M4V" => Some(Detected {
                kind: "video",
                mime: "video/quicktime",
            }),
            _ => None,
        };
    }
    None
}

/// Parses the declared `Content-Type` down to one of the accepted canonical
/// mimes (drops `; charset=...` params, lowercases). Anything else → `None`
/// (caller answers `415`).
fn canonical_content_type(raw: Option<&HeaderValue>) -> Option<&'static str> {
    let essence = raw?
        .to_str()
        .ok()?
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/webp" => Some("image/webp"),
        "image/gif" => Some("image/gif"),
        "video/mp4" => Some("video/mp4"),
        "video/quicktime" => Some("video/quicktime"),
        "video/webm" => Some("video/webm"),
        _ => None,
    }
}

/// On-disk extension for a canonical mime.
fn ext_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "video/mp4" => Some("mp4"),
        "video/quicktime" => Some("mov"),
        "video/webm" => Some("webm"),
        _ => None,
    }
}

/// Sanitizes the optional `X-File-Name`: strips path separators and control
/// characters, caps at 120 chars, falls back to `"file"` when empty.
fn sanitize_file_name(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "file".to_owned();
    };
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .take(120)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "file".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// `POST /api/media` response.
#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub media_id: Uuid,
    pub kind: String,
    pub mime: String,
    pub bytes: i64,
    pub file_name: String,
}

/// `POST /api/media` — Bearer-authenticated streaming upload.
pub async fn upload(
    State(state): State<AppState>,
    user: AuthUser,
    request: Request,
) -> Result<(StatusCode, Json<UploadResponse>), AppError> {
    let (parts, body) = request.into_parts();
    let declared = canonical_content_type(parts.headers.get(CONTENT_TYPE))
        .ok_or(AppError::UnsupportedMediaType)?;
    let cap = if declared.starts_with("image/") {
        state.media_max_image_bytes
    } else {
        state.media_max_video_bytes
    };
    let file_name = sanitize_file_name(
        parts
            .headers
            .get("x-file-name")
            .and_then(|value| value.to_str().ok()),
    );

    let mut stream = body.into_data_stream();

    // Phase 1: buffer just enough bytes to sniff the format, enforcing the
    // cap as we go. Any excess beyond SNIFF_LEN is held in `carry` so no
    // byte is lost when a single chunk overshoots the sniff window.
    let mut prefix: Vec<u8> = Vec::with_capacity(SNIFF_LEN);
    let mut carry: Option<Bytes> = None;
    let mut total: u64 = 0;
    while prefix.len() < SNIFF_LEN {
        match stream.next().await {
            None => break,
            Some(Err(err)) => return Err(AppError::internal(err)),
            Some(Ok(chunk)) => {
                total += chunk.len() as u64;
                if total > cap {
                    return Err(AppError::PayloadTooLarge);
                }
                if prefix.len() < SNIFF_LEN {
                    let need = SNIFF_LEN - prefix.len();
                    if chunk.len() <= need {
                        prefix.extend_from_slice(&chunk);
                    } else {
                        prefix.extend_from_slice(&chunk[..need]);
                        carry = Some(chunk.slice(need..));
                    }
                } else {
                    carry = Some(chunk);
                }
            }
        }
    }

    if prefix.is_empty() {
        return Err(AppError::BadRequest("empty request body".to_owned()));
    }
    let detected = sniff(&prefix).ok_or(AppError::UnsupportedMediaType)?;
    if detected.mime != declared {
        return Err(AppError::UnsupportedMediaType);
    }
    let ext = ext_for_mime(detected.mime)
        .ok_or_else(|| AppError::internal(anyhow::anyhow!("unmapped mime `{}`", detected.mime)))?;

    // Open the destination only after the type is proven, so rejected
    // uploads never create files.
    let now = OffsetDateTime::now_utc();
    let dir: PathBuf = state
        .media_dir
        .join(format!("{:04}", now.year()))
        .join(format!("{:02}", u8::from(now.month())));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(AppError::internal)?;
    let media_id = Uuid::now_v7();
    let path = dir.join(format!("{media_id}.{ext}"));
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(AppError::internal)?;
    file.write_all(&prefix).await.map_err(AppError::internal)?;
    if let Some(excess) = carry {
        file.write_all(&excess).await.map_err(AppError::internal)?;
    }

    // Phase 2: stream the remainder straight to disk, aborting early the
    // moment the running size crosses the cap.
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
    file.flush().await.map_err(AppError::internal)?;
    drop(file);

    let bytes = total as i64;
    if let Err(err) = sqlx::query(
        "INSERT INTO media (id, owner_id, kind, mime, bytes, file_name) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(media_id)
    .bind(user.0)
    .bind(detected.kind)
    .bind(detected.mime)
    .bind(bytes)
    .bind(&file_name)
    .execute(&state.pool)
    .await
    {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(AppError::internal(err));
    }

    tracing::info!(%media_id, owner = %user.0, kind = detected.kind, bytes, "media uploaded");
    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            media_id,
            kind: detected.kind.to_owned(),
            mime: detected.mime.to_owned(),
            bytes,
            file_name,
        }),
    ))
}

/// `GET /api/media/{id}` — public, Range-capable byte serving.
pub async fn serve(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Result<impl IntoResponse, AppError> {
    // The path is reconstructed from `(created_at, mime)` — the schema never
    // stores it, so a row is the single source of truth for where its bytes
    // live.
    let row: Option<(String, OffsetDateTime, String)> =
        sqlx::query_as("SELECT mime, created_at, file_name FROM media WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .map_err(AppError::internal)?;
    let Some((mime, created_at, file_name)) = row else {
        return Err(AppError::ResourceNotFound);
    };
    let ext = ext_for_mime(&mime).ok_or_else(|| {
        AppError::internal(anyhow::anyhow!("media {id} has unmapped mime `{mime}`"))
    })?;
    let path = state
        .media_dir
        .join(format!("{:04}", created_at.year()))
        .join(format!("{:02}", u8::from(created_at.month())))
        .join(format!("{id}.{ext}"));

    // `ServeFile` handles the single-range protocol (206 + Content-Range,
    // Accept-Ranges: bytes) and streams from disk without buffering.
    let mut service = ServeFile::new(&path);
    let response = service
        .try_call(request)
        .await
        .map_err(AppError::internal)?;
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&mime).map_err(AppError::internal)?,
    );
    let disposition = format!("inline; filename=\"{}\"", file_name.replace('"', "_"));
    parts.headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(AppError::internal)?,
    );
    parts.headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    Ok(Response::from_parts(parts, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    const GIF: &[u8] = b"GIF89a.....";
    const WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
    const WEBM: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3, 0x01, 0x00];
    fn mp4_brand(brand: &[u8; 4]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 0x18];
        out.extend_from_slice(b"ftyp");
        out.extend_from_slice(brand);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out
    }

    #[test]
    fn sniff_recognises_every_supported_magic() {
        assert_eq!(sniff(PNG).unwrap().mime, "image/png");
        assert_eq!(sniff(JPEG).unwrap().mime, "image/jpeg");
        assert_eq!(sniff(GIF).unwrap().mime, "image/gif");
        assert_eq!(sniff(WEBP).unwrap().mime, "image/webp");
        assert_eq!(sniff(WEBM).unwrap().mime, "video/webm");
        assert_eq!(sniff(&mp4_brand(b"isom")).unwrap().mime, "video/mp4");
        assert_eq!(sniff(&mp4_brand(b"qt  ")).unwrap().mime, "video/quicktime");
        assert_eq!(sniff(&mp4_brand(b"M4V ")).unwrap().mime, "video/quicktime");
        assert_eq!(sniff(&mp4_brand(b"qt  ")).unwrap().kind, "video");
    }

    #[test]
    fn sniff_rejects_unknown_brand_and_garbage() {
        assert!(sniff(&mp4_brand(b"zzzz")).is_none());
        assert!(sniff(b"not media at all").is_none());
        assert!(sniff(&[]).is_none());
        // "ftyp" present but too short to read a full brand.
        assert!(sniff(b"\x00\x00\x00\x18ftyp").is_none());
    }

    #[test]
    fn canonical_content_type_strips_params_and_rejects_unknown() {
        let value = |raw: &str| HeaderValue::from_str(raw).unwrap();
        assert_eq!(
            canonical_content_type(Some(&value("image/png"))),
            Some("image/png")
        );
        assert_eq!(
            canonical_content_type(Some(&value(" IMAGE/PNG ; charset=utf-8"))),
            Some("image/png")
        );
        assert_eq!(
            canonical_content_type(Some(&value("video/quicktime"))),
            Some("video/quicktime")
        );
        assert_eq!(
            canonical_content_type(Some(&value("application/pdf"))),
            None
        );
        assert_eq!(canonical_content_type(None), None);
    }

    #[test]
    fn sanitize_file_name_strips_dangerous_chars_and_caps_length() {
        assert_eq!(sanitize_file_name(None), "file");
        assert_eq!(sanitize_file_name(Some("   ")), "file");
        assert_eq!(
            sanitize_file_name(Some("../../etc/passwd")),
            "....etcpasswd"
        );
        assert_eq!(sanitize_file_name(Some("a\\b/c.png")), "abc.png");
        assert_eq!(sanitize_file_name(Some("with\nnewline\t")), "withnewline");
        assert_eq!(
            sanitize_file_name(Some(&"x".repeat(500))).chars().count(),
            120
        );
    }

    #[test]
    fn ext_mapping_matches_mime() {
        assert_eq!(ext_for_mime("image/jpeg"), Some("jpg"));
        assert_eq!(ext_for_mime("video/quicktime"), Some("mov"));
        assert_eq!(ext_for_mime("application/pdf"), None);
    }
}
