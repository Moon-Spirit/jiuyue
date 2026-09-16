//! Migration behaviour, exercised against a real PostgreSQL.
//!
//! Each test runs in its own schema created from scratch, so "empty database"
//! means an empty database, not a shared one another test has already touched.

use crate::support::{
    ColumnInfo, TestDb, USERS_MIGRATION_VERSION, applied_migrations, embedded_migration_count,
    primary_key_columns, users_columns,
};

/// The exact column set the first schema promises, in declaration order.
const EXPECTED_COLUMNS: [(&str, &str, &str); 9] = [
    ("id", "character", "NO"),
    ("username", "text", "NO"),
    ("email", "text", "NO"),
    ("password_hash", "text", "YES"),
    ("display_name", "text", "NO"),
    ("avatar_url", "text", "YES"),
    ("email_verified_at", "timestamp with time zone", "YES"),
    ("created_at", "timestamp with time zone", "NO"),
    ("updated_at", "timestamp with time zone", "NO"),
];

#[tokio::test]
async fn migrating_an_empty_database_creates_the_users_table() {
    let db = TestDb::start().await;

    let columns = users_columns(db.pool()).await;
    let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
    let expected: Vec<&str> = EXPECTED_COLUMNS.iter().map(|(name, _, _)| *name).collect();
    assert_eq!(names, expected, "users must have exactly these columns");

    for (name, data_type, is_nullable) in EXPECTED_COLUMNS {
        let column = ColumnInfo::get(&columns, name);
        assert_eq!(column.data_type, data_type, "wrong type for `{name}`");
        assert_eq!(
            column.is_nullable, is_nullable,
            "wrong nullability for `{name}`"
        );
    }

    let id = ColumnInfo::get(&columns, "id");
    assert_eq!(
        id.character_maximum_length,
        Some(26),
        "the ULID column must be CHAR(26)"
    );

    assert_eq!(
        primary_key_columns(db.pool(), "users").await,
        ["id"],
        "id must be the primary key"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn migrating_twice_is_idempotent() {
    let db = TestDb::start().await;

    let before = applied_migrations(db.pool()).await;
    assert!(
        !before.is_empty(),
        "the first migrate must have applied something"
    );

    db.store()
        .migrate()
        .await
        .expect("a second migrate must succeed");

    let after = applied_migrations(db.pool()).await;
    assert_eq!(
        after.len(),
        before.len(),
        "a second migrate must apply nothing new"
    );
    assert_eq!(
        embedded_migration_count(),
        after.len(),
        "every embedded migration must be applied - nothing may be pending"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn applied_migrations_are_recorded_and_discoverable() {
    let db = TestDb::start().await;

    let applied = applied_migrations(db.pool()).await;
    assert!(
        applied.iter().all(|migration| migration.success),
        "every applied migration must be recorded as successful"
    );

    let users = applied
        .iter()
        .find(|migration| migration.version == USERS_MIGRATION_VERSION)
        .expect("the users migration must be discoverable in _sqlx_migrations");
    assert_eq!(users.description, "users");

    db.cleanup().await;
}
