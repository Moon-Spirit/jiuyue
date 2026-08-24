//! Shared application state.

use crate::code_store::CodeStore;
use crate::crypto::BodyCipher;
use crate::ws::ConnRegistry;
use sqlx::PgPool;
use std::sync::Arc;

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
        Self {
            pool,
            redis,
            codes: CodeStore::new(),
            jwt_secret: Arc::new(jwt_secret.into()),
            registry: Arc::new(ConnRegistry::new()),
            cipher: BodyCipher::new(&master_key),
        }
    }
}
