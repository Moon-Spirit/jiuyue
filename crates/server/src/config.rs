//! Environment-driven configuration (loaded from `.env` via dotenvy).

use anyhow::Context;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    pub jwt_secret: String,
    pub bind_addr: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL is required")?,
            redis_url: std::env::var("REDIS_URL").context("REDIS_URL is required")?,
            jwt_secret: std::env::var("JIUYUE_JWT_SECRET")
                .context("JIUYUE_JWT_SECRET is required")?,
            bind_addr: std::env::var("JIUYUE_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into()),
        })
    }
}
