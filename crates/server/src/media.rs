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

/// Container family detected by magic bytes.
///
/// A family pins the CONTAINER only, never the audio-vs-video distinction:
/// EBML (webm), Ogg and ISO-BMFF (mp4 family) can each carry either an audio
/// or a video track, and the bytes alone cannot tell them apart reliably
/// (e.g. an audio-only `.m4a` and a `.mp4` share the `ftyp` box). The
/// declared `Content-Type` therefore refines the family into a canonical mime
/// in [`resolve_mime`], and the `kind` is derived from that mime prefix. The
/// declarer and the sniffer must agree on the FAMILY envelope; disagreement
/// is a `415`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Png,
    Jpeg,
    Gif,
    Webp,
    /// EBML header (webm/matroska): `video/webm` | `audio/webm`.
    Ebml,
    /// Ogg container: accepted only as `audio/ogg` in this build.
    Ogg,
    /// ISO base media file format (`ftyp` box): mp4/m4a/quicktime family.
    IsoBmff,
}

/// Magic-byte FAMILY detection. Returns `None` for anything unsupported (or
/// truncated); callers map that to `415`.
fn sniff_family(prefix: &[u8]) -> Option<Family> {
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if prefix.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(Family::Png);
    }
    // JPEG: FF D8 FF
    if prefix.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Family::Jpeg);
    }
    // GIF: "GIF87a" | "GIF89a"
    if prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a") {
        return Some(Family::Gif);
    }
    // WebP: "RIFF" .... "WEBP" (bytes 0-3 and 8-11)
    if prefix.len() >= 12 && &prefix[0..4] == b"RIFF" && &prefix[8..12] == b"WEBP" {
        return Some(Family::Webp);
    }
    // WebM / Matroska EBML header: 1A 45 DF A3
    if prefix.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(Family::Ebml);
    }
    // Ogg container: "OggS"
    if prefix.starts_with(b"OggS") {
        return Some(Family::Ogg);
    }
    // ISO base media file format: `ftyp` box at offset 4.
    if prefix.len() >= 12 && &prefix[4..8] == b"ftyp" {
        return Some(Family::IsoBmff);
    }
    None
}

/// Resolves the canonical mime from a (container family, declared mime) pair,
/// or `None` when the declared type does not belong to the sniffed family.
///
/// Audio-vs-video is decided HERE, by the declared MIME prefix, because the
/// container bytes cannot: `(Ebml, "audio/webm")` and `(Ebml, "video/webm")`
/// sniff identically, as do `(IsoBmff, "audio/mp4")` and
/// `(IsoBmff, "video/mp4")`.
fn resolve_mime(family: Family, declared: &str) -> Option<&'static str> {
    match (family, declared) {
        (Family::Png, "image/png") => Some("image/png"),
        (Family::Jpeg, "image/jpeg") => Some("image/jpeg"),
        (Family::Gif, "image/gif") => Some("image/gif"),
        (Family::Webp, "image/webp") => Some("image/webp"),
        (Family::Ebml, "video/webm") => Some("video/webm"),
        (Family::Ebml, "audio/webm") => Some("audio/webm"),
        (Family::Ogg, "audio/ogg") => Some("audio/ogg"),
        (Family::IsoBmff, "video/mp4") => Some("video/mp4"),
        (Family::IsoBmff, "video/quicktime") => Some("video/quicktime"),
        (Family::IsoBmff, "audio/mp4") => Some("audio/mp4"),
        _ => None,
    }
}

/// Canonical `kind` derived from a resolved mime: `audio/*` → `"audio"`,
/// everything else image/video by prefix.
fn kind_for_mime(mime: &str) -> &'static str {
    if mime.starts_with("audio/") {
        "audio"
    } else if mime.starts_with("image/") {
        "image"
    } else {
        "video"
    }
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
        "audio/webm" => Some("audio/webm"),
        "audio/ogg" => Some("audio/ogg"),
        "audio/mp4" => Some("audio/mp4"),
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
        "audio/webm" => Some("webm"),
        "audio/ogg" => Some("ogg"),
        "audio/mp4" => Some("m4a"),
        _ => None,
    }
}

/// Sanitizes the optional `X-File-Name`: percent-decodes it first (clients
/// encode UTF-8 names because header values are Latin-1 only), then strips
/// path separators and control characters, caps at 120 chars, falls back to
/// `"file"` when empty.
fn sanitize_file_name(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "file".to_owned();
    };
    let decoded = percent_decode_utf8(raw);
    let cleaned: String = decoded
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

/// Decodes `%XX` escapes into their bytes, then re-interprets the result as
/// UTF-8. Malformed escapes and invalid UTF-8 pass through verbatim, so old
/// clients that sent raw Latin-1 names keep working unchanged.
fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
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
    // Size cap: audio deliberately shares the VIDEO cap (200 MiB default,
    // `JIUYUE_MEDIA_MAX_VIDEO_BYTES`). Voice clips are small in practice, and
    // reusing the existing cap avoids inventing a second env knob for a
    // per-kind limit nobody asked to tune. Only images get the tighter cap.
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
    // Declarer/sniffer must agree on the container FAMILY; the declared mime
    // refines it into the canonical type (and thus the audio-vs-video kind).
    let family = sniff_family(&prefix).ok_or(AppError::UnsupportedMediaType)?;
    let mime = resolve_mime(family, declared).ok_or(AppError::UnsupportedMediaType)?;
    let kind = kind_for_mime(mime);
    let ext = ext_for_mime(mime)
        .ok_or_else(|| AppError::internal(anyhow::anyhow!("unmapped mime `{mime}`")))?;

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
    .bind(kind)
    .bind(mime)
    .bind(bytes)
    .bind(&file_name)
    .execute(&state.pool)
    .await
    {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(AppError::internal(err));
    }

    tracing::info!(%media_id, owner = %user.0, kind, bytes, "media uploaded");
    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            media_id,
            kind: kind.to_owned(),
            mime: mime.to_owned(),
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
    const OGG: &[u8] = b"OggS\x00\x02\x00\x00\x00\x00\x00\x00";
    fn mp4_brand(brand: &[u8; 4]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 0x18];
        out.extend_from_slice(b"ftyp");
        out.extend_from_slice(brand);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out
    }

    #[test]
    fn sniff_recognises_every_supported_family_and_resolves_its_mimes() {
        assert_eq!(sniff_family(PNG), Some(Family::Png));
        assert_eq!(sniff_family(JPEG), Some(Family::Jpeg));
        assert_eq!(sniff_family(GIF), Some(Family::Gif));
        assert_eq!(sniff_family(WEBP), Some(Family::Webp));
        assert_eq!(sniff_family(WEBM), Some(Family::Ebml));
        assert_eq!(sniff_family(OGG), Some(Family::Ogg));
        assert_eq!(sniff_family(&mp4_brand(b"isom")), Some(Family::IsoBmff));

        assert_eq!(resolve_mime(Family::Png, "image/png"), Some("image/png"));
        assert_eq!(resolve_mime(Family::Jpeg, "image/jpeg"), Some("image/jpeg"));
        assert_eq!(resolve_mime(Family::Gif, "image/gif"), Some("image/gif"));
        assert_eq!(resolve_mime(Family::Webp, "image/webp"), Some("image/webp"));
        // EBML carries both audio and video webm; declared mime decides.
        assert_eq!(resolve_mime(Family::Ebml, "video/webm"), Some("video/webm"));
        assert_eq!(resolve_mime(Family::Ebml, "audio/webm"), Some("audio/webm"));
        assert_eq!(resolve_mime(Family::Ogg, "audio/ogg"), Some("audio/ogg"));
        assert_eq!(
            resolve_mime(Family::IsoBmff, "video/mp4"),
            Some("video/mp4")
        );
        assert_eq!(
            resolve_mime(Family::IsoBmff, "video/quicktime"),
            Some("video/quicktime")
        );
        assert_eq!(resolve_mime(Family::IsoBmff, "audio/mp4"), Some("audio/mp4"));

        // Family disagreement is a hard reject (e.g. audio declared over PNG).
        assert_eq!(resolve_mime(Family::Png, "audio/webm"), None);
        assert_eq!(resolve_mime(Family::Ogg, "video/webm"), None);
    }

    #[test]
    fn kind_is_derived_from_the_resolved_mime_prefix() {
        assert_eq!(kind_for_mime("image/png"), "image");
        assert_eq!(kind_for_mime("video/mp4"), "video");
        assert_eq!(kind_for_mime("audio/webm"), "audio");
        assert_eq!(kind_for_mime("audio/ogg"), "audio");
        assert_eq!(kind_for_mime("audio/mp4"), "audio");
    }

    #[test]
    fn sniff_rejects_garbage_and_truncated_headers() {
        assert!(sniff_family(b"not media at all").is_none());
        assert!(sniff_family(&[]).is_none());
        // "ftyp" present but too short to read the box header fully.
        assert!(sniff_family(b"\x00\x00\x00\x18ftyp").is_none());
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
            canonical_content_type(Some(&value("audio/webm"))),
            Some("audio/webm")
        );
        assert_eq!(
            canonical_content_type(Some(&value(" AUDIO/OGG ; codecs=opus"))),
            Some("audio/ogg")
        );
        assert_eq!(
            canonical_content_type(Some(&value("audio/mp4"))),
            Some("audio/mp4")
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
    fn sanitize_file_name_percent_decodes_utf8_names() {
        // "中文.png" percent-encoded — what the web client sends now, because
        // header values are Latin-1 and raw CJK throws in the browser.
        assert_eq!(sanitize_file_name(Some("%E4%B8%AD%E6%96%87.png")), "中文.png");
        // Plain ASCII (old clients) passes through unchanged.
        assert_eq!(sanitize_file_name(Some("photo.png")), "photo.png");
        // Malformed escapes stay verbatim rather than erroring.
        assert_eq!(sanitize_file_name(Some("a%ZZb.png")), "a%ZZb.png");
        // Decoded path separators are still stripped afterwards (defense kept).
        assert_eq!(
            sanitize_file_name(Some("%2E%2E%2Fetc%2Fpasswd")),
            "..etcpasswd"
        );
    }

    #[test]
    fn ext_mapping_matches_mime() {
        assert_eq!(ext_for_mime("image/jpeg"), Some("jpg"));
        assert_eq!(ext_for_mime("video/quicktime"), Some("mov"));
        assert_eq!(ext_for_mime("audio/webm"), Some("webm"));
        assert_eq!(ext_for_mime("audio/ogg"), Some("ogg"));
        assert_eq!(ext_for_mime("audio/mp4"), Some("m4a"));
        assert_eq!(ext_for_mime("application/pdf"), None);
    }
}
