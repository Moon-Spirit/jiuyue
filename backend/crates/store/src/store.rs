//! The connection pool and the embedded migrator.

use std::str::FromStr;
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use crate::error::StoreError;

/// Migrations embedded at compile time from `backend/migrations/`.
///
/// The path is relative to this crate's manifest directory
/// (`backend/crates/store`), so it resolves to `backend/migrations`. Embedding
/// means the deployed binary carries its own migrations: the server can migrate
/// at boot on a box where no source tree exists, and the build itself never
/// needs a reachable database.
static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// Upper bound on pooled connections.
///
/// Production is a single 2 vCPU / 2 GB box and PostgreSQL's own default is 100
/// connections, so a low single-digit pool leaves headroom for operators and
/// background jobs while still absorbing bursts. Scaling out adds processes,
/// each with its own small pool, rather than widening this one.
const MAX_CONNECTIONS: u32 = 8;

/// Connections kept warm so the first request after an idle period does not pay
/// for a TCP + TLS + auth round trip.
const MIN_CONNECTIONS: u32 = 1;

/// How long a caller waits for a free connection before giving up. Bounded so a
/// saturated pool surfaces as an error instead of an unbounded queue.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Close connections idle for this long, releasing server-side memory.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Recycle connections after this long, so a failover or a changed credential
/// is picked up without a restart.
const MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// A PostgreSQL connection pool plus the migrations that define its schema.
///
/// Cloning a `Store` clones a handle to the same pool; it does not open new
/// connections.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connect to PostgreSQL, building a bounded pool.
    ///
    /// Connecting is lazy in the sense that the pool is created up front but
    /// individual connections are opened on demand; this call still validates
    /// the URL and establishes `min_connections` connections, so a bad URL or
    /// bad credentials fail here rather than on the first query.
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        let options = PgConnectOptions::from_str(database_url).map_err(StoreError::InvalidUrl)?;

        let pool = PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .min_connections(MIN_CONNECTIONS)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .idle_timeout(IDLE_TIMEOUT)
            .max_lifetime(MAX_LIFETIME)
            .connect_with(options)
            .await
            .map_err(StoreError::Connect)?;

        Ok(Self { pool })
    }

    /// Apply every embedded migration that has not been applied yet.
    ///
    /// Each migration runs in its own transaction and is recorded in
    /// `_sqlx_migrations`, so calling this repeatedly is safe: already-applied
    /// migrations are skipped, and a migration whose recorded checksum no longer
    /// matches its file is refused rather than silently re-run.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await.map_err(StoreError::Migrate)
    }

    /// The underlying pool, for repositories and health checks.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
