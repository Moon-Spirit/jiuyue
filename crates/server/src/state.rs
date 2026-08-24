//! Shared application state.

use crate::code_store::CodeStore;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub redis: redis::aio::ConnectionManager,
    pub codes: CodeStore,
    pub jwt_secret: Arc<String>,
}

impl AppState {
    pub fn new(
        pool: PgPool,
        redis: redis::aio::ConnectionManager,
        jwt_secret: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            redis,
            codes: CodeStore::new(),
            jwt_secret: Arc::new(jwt_secret.into()),
        }
    }
}
