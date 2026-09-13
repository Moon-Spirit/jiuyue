//! Shared application state.

use crate::code_store::CodeStore;
use crate::crypto::BodyCipher;
use crate::push::PushService;
use crate::rtc::CallRegistry;
use crate::ws::{ConnRegistry, HeartbeatConfig};
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;

/// Default per-kind upload caps (bytes) when the env override is absent.
pub const DEFAULT_MEDIA_MAX_IMAGE_BYTES: u64 = 15 * 1024 * 1024;
pub const DEFAULT_MEDIA_MAX_VIDEO_BYTES: u64 = 200 * 1024 * 1024;

/// Default cap for a single group-file upload (bytes) when the env override
/// is absent (`JIUYUE_GROUP_FILE_MAX_BYTES`).
pub const DEFAULT_GROUP_FILE_MAX_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub redis: redis::aio::ConnectionManager,
    pub codes: CodeStore,
    pub jwt_secret: Arc<String>,
    /// Live WS connections (user -> device -> outbound channel), shared by
    /// all connection tasks; fanout targets registered devices only.
    pub registry: Arc<ConnRegistry>,
    /// AES-256-GCM at-rest cipher. Key derivation: SHA-256 of the UTF-8
    /// bytes of `JIUYUE_MASTER_KEY` (see `crypto` module docs).
    pub cipher: BodyCipher,
    /// Server-initiated WS heartbeat intervals, parsed once from
    /// `JIUYUE_WS_PING_SECS` / `JIUYUE_WS_TIMEOUT_SECS` (defaults 30/60).
    /// Public so tests can shrink them without env mutation.
    pub heartbeat: HeartbeatConfig,
    /// Region-aware offline-push facade (`push` module). The Mock channel
    /// inside is always the last-resort sink; the dev-only `/api/dev/push-log`
    /// endpoint reads its snapshot.
    pub push: Arc<PushService>,
    /// Root directory for uploaded media. Defaults to `./data/media`
    /// (`JIUYUE_MEDIA_DIR` override); files live at
    /// `{media_dir}/{yyyy}/{mm}/{id}.{ext}`. `Arc` so cloning `AppState`
    /// (every request) stays cheap; public so tests can point it at a temp
    /// dir without mutating process-global env.
    pub media_dir: Arc<PathBuf>,
    /// Maximum accepted image upload size in bytes
    /// (`JIUYUE_MEDIA_MAX_IMAGE_BYTES`, default 15 MiB).
    pub media_max_image_bytes: u64,
    /// Maximum accepted video upload size in bytes
    /// (`JIUYUE_MEDIA_MAX_VIDEO_BYTES`, default 200 MiB).
    pub media_max_video_bytes: u64,
    /// Maximum accepted size of one group-file upload in bytes
    /// (`JIUYUE_GROUP_FILE_MAX_BYTES`, default 200 MiB). The per-group free
    /// quota (1 GiB) is a separate, fixed domain constant and is NOT tuned by
    /// this cap — the cap only bounds a single file, streamed then aborted.
    pub group_file_max_bytes: u64,
    /// M14a in-memory RTC call registry: which conversation has an active
    /// call, who is in it, and since when. Process-local and ephemeral — no
    /// call history is persisted (media never touches the server).
    pub calls: Arc<CallRegistry>,
}

/// Reads a positive `u64` byte cap from the environment; missing, non-numeric
/// or zero values fall back to `default`.
fn parse_bytes_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

impl AppState {
    pub fn new(
        pool: PgPool,
        redis: redis::aio::ConnectionManager,
        jwt_secret: impl Into<String>,
    ) -> Self {
        // Loaded from the environment so existing call sites (main.rs, test
        // harnesses) stay source-compatible; dotenvy must have run first.
        let master_key = std::env::var("JIUYUE_MASTER_KEY").unwrap_or_default();
        if master_key.is_empty() {
            tracing::warn!(
                "JIUYUE_MASTER_KEY is not set; message bodies would be encrypted \
                 under a publicly-derivable key — configure it before production"
            );
        }
        let media_dir =
            std::env::var("JIUYUE_MEDIA_DIR").unwrap_or_else(|_| "./data/media".to_owned());
        Self {
            pool,
            redis,
            codes: CodeStore::new(),
            jwt_secret: Arc::new(jwt_secret.into()),
            registry: Arc::new(ConnRegistry::new()),
            cipher: BodyCipher::new(&master_key),
            heartbeat: HeartbeatConfig::from_env(),
            push: Arc::new(PushService::from_env()),
            media_dir: Arc::new(PathBuf::from(media_dir)),
            media_max_image_bytes: parse_bytes_env(
                "JIUYUE_MEDIA_MAX_IMAGE_BYTES",
                DEFAULT_MEDIA_MAX_IMAGE_BYTES,
            ),
            media_max_video_bytes: parse_bytes_env(
                "JIUYUE_MEDIA_MAX_VIDEO_BYTES",
                DEFAULT_MEDIA_MAX_VIDEO_BYTES,
            ),
            group_file_max_bytes: parse_bytes_env(
                "JIUYUE_GROUP_FILE_MAX_BYTES",
                DEFAULT_GROUP_FILE_MAX_BYTES,
            ),
            calls: Arc::new(CallRegistry::new()),
        }
    }
}
