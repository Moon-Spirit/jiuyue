//! The `users` schema's constraints, exercised against a real PostgreSQL.
//!
//! Violations are asserted by SQLSTATE *and* constraint name, so a row that
//! failed for the wrong reason still fails the test.

use sqlx::Row;
use sqlx::types::time::OffsetDateTime;

use crate::support::{NewUser, TestDb, assert_violation, fixed_timestamp, insert_user, ulid};

#[tokio::test]
async fn duplicate_email_is_rejected() {
    let db = TestDb::start().await;

    insert_user(
        db.pool(),
        &NewUser::password_account(&ulid('1'), "alice", "alice@example.com"),
    )
    .await
    .expect("the first row must insert");

    let error = insert_user(
        db.pool(),
        &NewUser::password_account(&ulid('2'), "alice_two", "alice@example.com"),
    )
    .await
    .expect_err("a duplicate email must be rejected");

    assert_violation(&error, "23505", "users_email_key");

    db.cleanup().await;
}

#[tokio::test]
async fn duplicate_username_is_rejected() {
    let db = TestDb::start().await;

    insert_user(
        db.pool(),
        &NewUser::password_account(&ulid('3'), "bob", "bob@example.com"),
    )
    .await
    .expect("the first row must insert");

    let error = insert_user(
        db.pool(),
        &NewUser::password_account(&ulid('4'), "bob", "bob_two@example.com"),
    )
    .await
    .expect_err("a duplicate username must be rejected");

    assert_violation(&error, "23505", "users_username_key");

    db.cleanup().await;
}

#[tokio::test]
async fn uppercase_email_violates_the_lowercase_check() {
    let db = TestDb::start().await;

    let error = insert_user(
        db.pool(),
        &NewUser::password_account(&ulid('5'), "carol", "Carol@Example.com"),
    )
    .await
    .expect_err("an email that is not lowercase must be rejected");

    assert_violation(&error, "23514", "users_email_lowercase");

    db.cleanup().await;
}

#[tokio::test]
async fn invalid_username_format_is_rejected() {
    let db = TestDb::start().await;

    let too_long = "a".repeat(33);
    let invalid = [
        "UPPER",
        "ab",
        "has space",
        "has-dash",
        "has.dot",
        too_long.as_str(),
    ];

    for (index, username) in invalid.iter().enumerate() {
        let id = ulid(char::from_digit(index as u32, 10).expect("index fits a base-10 digit"));
        let email = format!("invalid{index}@example.com");

        let error = insert_user(db.pool(), &NewUser::password_account(&id, username, &email))
            .await
            .expect_err("an invalid username must be rejected");

        assert_violation(&error, "23514", "users_username_format");
    }

    db.cleanup().await;
}

#[tokio::test]
async fn valid_user_round_trips() {
    let db = TestDb::start().await;

    let id = ulid('7');
    let verified_at = fixed_timestamp();
    let password_hash = "$argon2id$v=19$m=19456,t=2,p=1$test$test";

    insert_user(
        db.pool(),
        &NewUser {
            id: &id,
            username: "round_trip",
            email: "round_trip@example.com",
            display_name: "Round Trip",
            password_hash: Some(password_hash),
            email_verified_at: Some(verified_at),
        },
    )
    .await
    .expect("a valid row must insert");

    let row = sqlx::query(
        "SELECT id::text AS id, username, email, password_hash, display_name, \
                avatar_url, email_verified_at, created_at, updated_at \
         FROM users WHERE id = $1",
    )
    .bind(&id)
    .fetch_one(db.pool())
    .await
    .expect("the inserted row must be selectable");

    assert_eq!(row.get::<String, _>("id"), id);
    assert_eq!(row.get::<String, _>("username"), "round_trip");
    assert_eq!(row.get::<String, _>("email"), "round_trip@example.com");
    assert_eq!(row.get::<String, _>("display_name"), "Round Trip");
    assert_eq!(
        row.get::<Option<String>, _>("password_hash"),
        Some(password_hash.to_owned())
    );
    assert_eq!(row.get::<Option<String>, _>("avatar_url"), None);
    assert_eq!(
        row.get::<Option<OffsetDateTime>, _>("email_verified_at"),
        Some(verified_at)
    );

    // Both timestamp columns are NOT NULL with a database-side default: decoding
    // them into a non-optional `OffsetDateTime` fails if either were NULL.
    let created_at = row.get::<OffsetDateTime, _>("created_at");
    let updated_at = row.get::<OffsetDateTime, _>("updated_at");
    assert!(
        created_at <= updated_at,
        "created_at must not be after updated_at"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn oauth_only_account_may_omit_password_and_verification() {
    let db = TestDb::start().await;

    let id = ulid('8');
    insert_user(
        db.pool(),
        &NewUser::oauth_account(&id, "oauth_user", "oauth@example.com"),
    )
    .await
    .expect("an OAuth-only account without a password must insert");

    let row = sqlx::query("SELECT password_hash, email_verified_at FROM users WHERE id = $1")
        .bind(&id)
        .fetch_one(db.pool())
        .await
        .expect("the OAuth-only row must be selectable");

    assert_eq!(row.get::<Option<String>, _>("password_hash"), None);
    assert_eq!(
        row.get::<Option<OffsetDateTime>, _>("email_verified_at"),
        None
    );

    db.cleanup().await;
}
