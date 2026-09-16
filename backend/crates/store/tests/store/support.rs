//! Shared integration-test harness: one throwaway PostgreSQL schema per test.
//!
//! The database is never mocked — the constraints, the defaults and sqlx's
//! migration bookkeeping *are* the behaviour under test. Every test gets its own
//! schema, selected through the connection's `search_path`, so tests run in
//! parallel and can be re-run repeatedly with no manual cleanup.

use std::env;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgPoolOptions;
use sqlx::types::time::OffsetDateTime;
use sqlx::{Executor, PgPool, Row};

use jiuyue_store::Store;

/// Version (the filename timestamp) of the `users` migration.
pub const USERS_MIGRATION_VERSION: i64 = 20_260_917_120_000;

/// Fixed instant with microsecond precision, so a value can survive a round trip
/// through `timestamptz` unchanged.
pub fn fixed_timestamp() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("a valid fixed unix timestamp")
}

/// A valid 26-character ULID, with the last character supplied so a test can mint
/// as many distinct ids as it needs.
pub fn ulid(seed: char) -> String {
    format!("01JABC1234567890ABCDEFGHJ{seed}")
}

/// Panics with an actionable message when no database is configured.
///
/// Failing loudly is deliberate: a silently skipped integration test would let
/// the schema rot while the suite stayed green.
fn database_url() -> String {
    let url = env::var("TEST_DATABASE_URL")
        .ok()
        .or_else(|| env::var("DATABASE_URL").ok())
        .filter(|value| !value.trim().is_empty());

    url.unwrap_or_else(|| {
        panic!(
            "jiuyue-store integration tests need a real PostgreSQL database.\n\
             Set TEST_DATABASE_URL (preferred) or DATABASE_URL, for example:\n\
             \x20   postgres://jiuyue:jiuyue_dev_password@localhost:5432/jiuyue_test\n\
             Tests are never silently skipped."
        )
    })
}

/// A fresh, isolated schema migrated to latest, plus the [`Store`] pointed at it.
pub struct TestDb {
    store: Store,
    admin_url: String,
    schema: String,
}

impl TestDb {
    /// Create a uniquely named schema, then connect and migrate into it.
    pub async fn start() -> Self {
        let admin_url = database_url();
        let schema = unique_schema_name();

        admin_execute(&admin_url, &format!("CREATE SCHEMA \"{schema}\""))
            .await
            .expect("the test database must allow creating a schema");

        let store = Store::connect(&scoped_url(&admin_url, &schema))
            .await
            .expect("Store::connect must reach the test database");
        store
            .migrate()
            .await
            .expect("migrations must apply to an empty schema");

        Self {
            store,
            admin_url,
            schema,
        }
    }

    /// The store pointed at this test's schema.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The pool, for direct queries that exercise the schema.
    pub fn pool(&self) -> &PgPool {
        self.store.pool()
    }

    /// Drop this test's schema. Best effort: a failing test merely leaves a
    /// uniquely named schema behind, which never affects a later run.
    pub async fn cleanup(self) {
        self.store.pool().close().await;
        let statement = format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema);
        let _ = admin_execute(&self.admin_url, &statement).await;
    }
}

/// A process- and run-unique schema name, safe to interpolate as an identifier.
fn unique_schema_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);

    format!("test_{}_{sequence}_{nanos}", std::process::id())
}

/// Point a connection at one schema through the `search_path` startup option.
///
/// `options[search_path]=<schema>` is sqlx's URL form of `PGOPTIONS`, so every
/// pooled connection — including the ones the migrator opens — resolves
/// unqualified names to this schema and nowhere else.
fn scoped_url(base: &str, schema: &str) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}options[search_path]={schema}")
}

async fn admin_execute(admin_url: &str, statement: &str) -> Result<(), sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(admin_url)
        .await?;
    let result = pool.execute(statement).await;
    pool.close().await;
    result.map(|_| ())
}

/// Column metadata for a table in the test schema.
#[derive(Debug)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub is_nullable: String,
    pub character_maximum_length: Option<i32>,
}

impl ColumnInfo {
    /// Look a column up by name, panicking if the table does not have it.
    pub fn get<'a>(columns: &'a [ColumnInfo], name: &str) -> &'a ColumnInfo {
        columns
            .iter()
            .find(|column| column.name == name)
            .unwrap_or_else(|| panic!("table is missing column `{name}`"))
    }
}

/// Every column of `users`, in declaration order, from `information_schema`.
pub async fn users_columns(pool: &PgPool) -> Vec<ColumnInfo> {
    sqlx::query(
        "SELECT column_name, data_type, is_nullable, character_maximum_length \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'users' \
         ORDER BY ordinal_position",
    )
    .fetch_all(pool)
    .await
    .expect("querying information_schema.columns must succeed")
    .into_iter()
    .map(|row| ColumnInfo {
        name: row.get("column_name"),
        data_type: row.get("data_type"),
        is_nullable: row.get("is_nullable"),
        character_maximum_length: row.get("character_maximum_length"),
    })
    .collect()
}

/// The primary-key columns of a table in the test schema, in key order.
pub async fn primary_key_columns(pool: &PgPool, table: &str) -> Vec<String> {
    sqlx::query(
        "SELECT kcu.column_name \
         FROM information_schema.table_constraints AS tc \
         JOIN information_schema.key_column_usage AS kcu \
           ON tc.constraint_name = kcu.constraint_name \
          AND tc.table_schema = kcu.table_schema \
         WHERE tc.table_schema = current_schema() \
           AND tc.table_name = $1 \
           AND tc.constraint_type = 'PRIMARY KEY' \
         ORDER BY kcu.ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("querying information_schema.table_constraints must succeed")
    .into_iter()
    .map(|row| row.get("column_name"))
    .collect()
}

/// One row of sqlx's migration ledger.
#[derive(Debug)]
pub struct AppliedMigration {
    pub version: i64,
    pub description: String,
    pub success: bool,
}

/// Everything recorded in `_sqlx_migrations`, oldest first.
pub async fn applied_migrations(pool: &PgPool) -> Vec<AppliedMigration> {
    sqlx::query("SELECT version, description, success FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("querying _sqlx_migrations must succeed")
        .into_iter()
        .map(|row| AppliedMigration {
            version: row.get("version"),
            description: row.get("description"),
            success: row.get("success"),
        })
        .collect()
}

/// How many `.sql` files ship in `backend/migrations/`.
///
/// Comparing this with the ledger is how the suite proves nothing is pending.
pub fn embedded_migration_count() -> usize {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("failed to read `{directory:?}`: {error}"))
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "sql"))
        .count()
}

/// A `users` row to insert, so the insert helper keeps a single parameter.
#[derive(Debug)]
pub struct NewUser<'a> {
    pub id: &'a str,
    pub username: &'a str,
    pub email: &'a str,
    pub display_name: &'a str,
    pub password_hash: Option<&'a str>,
    pub email_verified_at: Option<OffsetDateTime>,
}

impl<'a> NewUser<'a> {
    /// A complete, valid password account.
    pub fn password_account(id: &'a str, username: &'a str, email: &'a str) -> Self {
        Self {
            id,
            username,
            email,
            display_name: "Test User",
            password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$test$test"),
            email_verified_at: Some(fixed_timestamp()),
        }
    }

    /// A valid account with no password, as an OAuth-only account has.
    pub fn oauth_account(id: &'a str, username: &'a str, email: &'a str) -> Self {
        Self {
            email_verified_at: None,
            password_hash: None,
            ..Self::password_account(id, username, email)
        }
    }
}

/// Insert a `users` row, returning the raw database outcome.
pub async fn insert_user(pool: &PgPool, user: &NewUser<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, display_name, email_verified_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(user.id)
    .bind(user.username)
    .bind(user.email)
    .bind(user.password_hash)
    .bind(user.display_name)
    .bind(user.email_verified_at)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Assert that an insert failed with a specific SQLSTATE and constraint name.
///
/// Checking the constraint name, not just the error, keeps the assertion honest:
/// a failure for the wrong reason still fails the test.
pub fn assert_violation(error: &sqlx::Error, expected_code: &str, expected_constraint: &str) {
    match error {
        sqlx::Error::Database(database_error) => {
            assert_eq!(
                database_error.code().as_deref(),
                Some(expected_code),
                "unexpected SQLSTATE for: {database_error}"
            );
            assert_eq!(
                database_error.constraint(),
                Some(expected_constraint),
                "unexpected constraint for: {database_error}"
            );
        }
        other => panic!("expected a database constraint error, got: {other:?}"),
    }
}
