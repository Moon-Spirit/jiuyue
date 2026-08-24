//! Ticket 04 integration tests: drive the real router over HTTP semantics via
//! `tower::ServiceExt::oneshot` against the live local PostgreSQL (`jiuyue_test`)
//! and Redis.
//!
//! Isolation strategy: the whole binary shares one global async mutex; the first
//! entrant resets the schema (DROP + fresh migrate), every test truncates all
//! tables up front. Tests therefore run serialized but fully isolated.
//!
//! Cross-binary isolation: cargo runs several integration-test binaries in
//! parallel against the same database. Every test holds a session-level PG
//! advisory lock for its whole lifetime (released automatically when the
//! guard connection drops), so a schema reset in one binary can never race a
//! running test in another.

use axum::http::{Request, StatusCode};
use axum::{body::Body, Router};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use tower::ServiceExt;

use jiuyue_server::auth::ws_ticket;
use jiuyue_server::error::AppError;
use jiuyue_server::state::AppState;

/// Serializes tests and guards the one-time schema reset.
static GATE: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::const_new(()));
static SCHEMA_READY: AtomicBool = AtomicBool::new(false);

/// Arbitrary fixed key shared by every JiuYue test binary.
const TEST_ADVISORY_LOCK_KEY: i64 = 0x6A_75_59_55_00_01;

struct TestApp {
    app: Router,
    state: AppState,
    pool: PgPool,
    redis: redis::aio::ConnectionManager,
    /// Holds the advisory lock; dropping it releases the lock (session end).
    _lock_conn: PgConnection,
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env var {name}"))
}

async fn test_app() -> TestApp {
    dotenvy::dotenv().ok();

    let mut lock_conn = PgConnection::connect(&env_var("TEST_DATABASE_URL"))
        .await
        .expect("connect lock session");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEST_ADVISORY_LOCK_KEY)
        .execute(&mut lock_conn)
        .await
        .expect("acquire cross-binary test lock");

    if !SCHEMA_READY.load(Ordering::SeqCst) {
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&env_var("TEST_DATABASE_URL"))
            .await
            .expect("connect TEST_DATABASE_URL");
        sqlx::query("DROP SCHEMA public CASCADE")
            .execute(&admin)
            .await
            .expect("drop schema");
        sqlx::query("CREATE SCHEMA public")
            .execute(&admin)
            .await
            .expect("create schema");
        jiuyue_server::MIGRATOR
            .run(&admin)
            .await
            .expect("run migrations");
        admin.close().await;
        SCHEMA_READY.store(true, Ordering::SeqCst);
    }

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&env_var("TEST_DATABASE_URL"))
        .await
        .expect("connect TEST_DATABASE_URL");
    sqlx::query(
        "TRUNCATE users, auth_identities, devices, refresh_tokens, \
         conversations, conversation_members, messages RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate tables");
    let redis_client = redis::Client::open(env_var("REDIS_URL").as_str()).expect("parse redis url");
    let redis = redis::aio::ConnectionManager::new(redis_client)
        .await
        .expect("connect redis");
    let state = AppState::new(pool.clone(), redis.clone(), env_var("JIUYUE_JWT_SECRET"));
    TestApp {
        app: jiuyue_server::build_router(state.clone()),
        state,
        pool,
        redis,
        _lock_conn: lock_conn,
    }
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder
        .header("content-type", "application/json")
        .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
        .expect("build request");
    let response = app.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, json)
}

fn generic_401_body() -> Value {
    json!({
        "error": "invalid_credentials",
        "message": "invalid credentials"
    })
}

/// Register-path code failures share ONE body between wrong-code and
/// unknown-target (no enumeration leak) but use a DEDICATED machine code so
/// clients can render an actionable hint instead of "wrong password".
fn generic_code_body() -> Value {
    json!({
        "error": "invalid_or_expired_code",
        "message": "invalid or expired verification code"
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn email_register_login_refresh_and_ws_ticket_roundtrip() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    // request-code (email)
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "email", "target": "user@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["expires_in_secs"], 300);

    // register with dev code, mixed-case target proves normalization
    let (status, reg) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email",
            "target": "User@Example.com",
            "code": "000000",
            "username": "alice",
            "password": "password123"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reg}");
    assert_eq!(reg["username"], "alice");
    assert_eq!(reg["expires_in"], 900);
    let user_id: uuid::Uuid = reg["user_id"].as_str().unwrap().parse().unwrap();
    let access = reg["access_token"].as_str().unwrap().to_string();
    let refresh1 = reg["refresh_token"].as_str().unwrap().to_string();

    // login by username (case-insensitive) and by email (case-insensitive)
    let (status, login_user) = send(
        &t.app,
        "POST",
        "/api/auth/login",
        None,
        Some(json!({"identifier": "ALICE", "password": "password123"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login_user}");
    let (status, login_email) = send(
        &t.app,
        "POST",
        "/api/auth/login",
        None,
        Some(json!({"identifier": "USER@EXAMPLE.COM", "password": "password123"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{login_email}");

    // refresh rotates
    let (status, rotated) = send(
        &t.app,
        "POST",
        "/api/auth/refresh",
        None,
        Some(json!({"refresh_token": refresh1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let refresh2 = rotated["refresh_token"].as_str().unwrap();
    assert_ne!(refresh2, refresh1);

    // old refresh token is dead
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/refresh",
        None,
        Some(json!({"refresh_token": refresh1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // ws-ticket requires a valid bearer...
    let (status, body) = send(&t.app, "POST", "/api/auth/ws-ticket", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/ws-ticket",
        Some("not.a.jwt"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // ...and yields a single-use ticket bound to the user
    let (status, ticket_body) = send(
        &t.app,
        "POST",
        "/api/auth/ws-ticket",
        Some(&access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ticket_body}");
    let ticket = ticket_body["ticket"].as_str().unwrap();

    let consumed = ws_ticket::consume(&t.state, ticket).await.unwrap();
    assert_eq!(consumed, user_id);
    let replay = ws_ticket::consume(&t.state, ticket).await.unwrap_err();
    assert!(
        matches!(replay, AppError::InvalidCredentials),
        "second redemption must fail, got {replay:?}"
    );
}

#[tokio::test]
async fn phone_channel_accepts_dev_code_and_wrong_code_is_generic_401() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "phone", "target": "+8613800138000"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // dev code 000000 accepted on the phone channel
    let (status, reg) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "phone",
            "target": "+8613800138000",
            "code": "000000",
            "username": "bob",
            "password": "password123"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reg}");

    // wrong code on an UNKNOWN target
    let (status_unknown, body_unknown) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "phone",
            "target": "+8613900000000",
            "code": "111111",
            "username": "mallory",
            "password": "password123"
        })),
    )
    .await;
    assert_eq!(status_unknown, StatusCode::UNAUTHORIZED, "{body_unknown}");

    // wrong code on a KNOWN target (fresh code requested, then bogus attempt)
    let (status, _) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "phone", "target": "+8613800138000"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status_known, body_known) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "phone",
            "target": "+8613800138000",
            "code": "999999",
            "username": "bobby",
            "password": "password123"
        })),
    )
    .await;
    assert_eq!(status_known, StatusCode::UNAUTHORIZED, "{body_known}");

    // identical generic bodies - no account-existence leak; register code
    // failures carry their own machine code (distinct from login credentials)
    assert_eq!(body_unknown, generic_code_body());
    assert_eq!(body_known, generic_code_body());
}

#[tokio::test]
async fn duplicate_username_and_duplicate_identity_binding_return_409() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    for (target, username) in [
        ("carol_a@example.com", "carol"),
        ("dave@example.com", "dave"),
    ] {
        let (status, _) = send(
            &t.app,
            "POST",
            "/api/auth/request-code",
            None,
            Some(json!({"channel": "email", "target": target})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(
            &t.app,
            "POST",
            "/api/auth/register",
            None,
            Some(json!({
                "channel": "email", "target": target, "code": "000000",
                "username": username, "password": "password123"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // duplicate username
    let (status, _) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "email", "target": "erin@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "erin@example.com", "code": "000000",
            "username": "carol", "password": "password123"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "username_taken");

    // duplicate identity binding (same email, new username)
    let (status, _) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "email", "target": "carol_a@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "CAROL_A@example.com", "code": "000000",
            "username": "carol_two", "password": "password123"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "identity_already_bound");
}

#[tokio::test]
async fn weak_password_and_invalid_username_rejected_422_with_reason() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (status, _) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "email", "target": "frank@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // weak password
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "frank@example.com", "code": "000000",
            "username": "frank", "password": "short"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "validation_error");
    let message = body["message"].as_str().unwrap().to_lowercase();
    assert!(message.contains("password"), "reason missing: {message}");
    assert!(message.contains("8"), "min length missing: {message}");

    // invalid username shape
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "frank@example.com", "code": "000000",
            "username": "Bad Name!", "password": "password123"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "validation_error");
    assert!(body["message"].as_str().unwrap().contains("username"));
}

#[tokio::test]
async fn refresh_rotation_invalidates_old_token_and_rejects_garbage() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (status, _) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "email", "target": "grace@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, reg) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "grace@example.com", "code": "000000",
            "username": "grace", "password": "password123"
        })),
    )
    .await;
    let r1 = reg["refresh_token"].as_str().unwrap().to_string();

    // rotate: r1 -> r2
    let (status, r2_body) = send(
        &t.app,
        "POST",
        "/api/auth/refresh",
        None,
        Some(json!({"refresh_token": r1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r2_body}");
    let r2 = r2_body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(r1, r2);

    // replaying r1 fails; chain continues with r2
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/refresh",
        None,
        Some(json!({"refresh_token": r1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, r3_body) = send(
        &t.app,
        "POST",
        "/api/auth/refresh",
        None,
        Some(json!({"refresh_token": r2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{r3_body}");

    // malformed token
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/refresh",
        None,
        Some(json!({"refresh_token": "definitely-not-a-token"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn ws_ticket_is_single_use() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (status, _) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "email", "target": "heidi@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, reg) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "heidi@example.com", "code": "000000",
            "username": "heidi", "password": "password123"
        })),
    )
    .await;
    let access = reg["access_token"].as_str().unwrap();

    let (status, ticket_body) = send(
        &t.app,
        "POST",
        "/api/auth/ws-ticket",
        Some(access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ticket_body}");
    let ticket = ticket_body["ticket"].as_str().unwrap();

    // Redemption happens during the WS upgrade handshake (ticket 05); the
    // library-level GETDEL consumer is exercised here directly.
    let first = ws_ticket::consume(&t.state, ticket).await;
    assert!(first.is_ok(), "first redemption must succeed: {first:?}");
    let second = ws_ticket::consume(&t.state, ticket).await;
    assert!(
        matches!(second, Err(AppError::InvalidCredentials)),
        "second redemption must fail: {second:?}"
    );
}

#[tokio::test]
async fn expired_ws_ticket_fails() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    // Choice documented per ticket: expiry is tested by seeding Redis directly
    // with a 1-second TTL instead of waiting out the production 300s TTL.
    let user_id = uuid::Uuid::now_v7();
    let ticket = "expired-ticket-seeded-by-test";
    let mut conn = t.redis.clone();
    redis::cmd("SETEX")
        .arg(format!("ws_ticket:{ticket}"))
        .arg(1u64)
        .arg(user_id.to_string())
        .query_async::<()>(&mut conn)
        .await
        .expect("seed ticket");

    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;

    let result = ws_ticket::consume(&t.state, ticket).await;
    assert!(
        matches!(result, Err(AppError::InvalidCredentials)),
        "expired ticket must fail: {result:?}"
    );
}

#[tokio::test]
async fn migrations_run_idempotently() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let count_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&t.pool)
            .await
            .expect("count migrations");

    // Running the migrator repeatedly must be a no-op, not an error.
    jiuyue_server::MIGRATOR.run(&t.pool).await.expect("rerun 1");
    jiuyue_server::MIGRATOR.run(&t.pool).await.expect("rerun 2");

    let count_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&t.pool)
            .await
            .expect("count migrations");

    assert!(count_before >= 1);
    assert_eq!(count_before, count_after, "migrations re-applied!");

    // Sanity: core tables exist and are queryable.
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&t.pool)
        .await
        .expect("query users");
    assert_eq!(users, 0);
}

#[tokio::test]
async fn login_failures_do_not_leak_account_existence() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (status, _) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "email", "target": "ivan@example.com"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, reg) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "ivan@example.com", "code": "000000",
            "username": "ivan", "password": "password123"
        })),
    )
    .await;
    assert!(reg["user_id"].is_string());

    // known account + wrong password vs unknown account: byte-identical 401
    let (status_known, body_known) = send(
        &t.app,
        "POST",
        "/api/auth/login",
        None,
        Some(json!({"identifier": "ivan", "password": "wrong-password"})),
    )
    .await;
    let (status_ghost, body_ghost) = send(
        &t.app,
        "POST",
        "/api/auth/login",
        None,
        Some(json!({"identifier": "ghost_user", "password": "whatever123"})),
    )
    .await;

    assert_eq!(status_known, StatusCode::UNAUTHORIZED);
    assert_eq!(status_ghost, StatusCode::UNAUTHORIZED);
    assert_eq!(body_known, generic_401_body());
    assert_eq!(body_ghost, generic_401_body());
}

#[tokio::test]
async fn login_response_includes_profile_for_client_display() {
    let t = test_app().await;
    let (status, reg) = send(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({ "channel": "email", "target": "profile@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reg}");
    let (status, reg) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "profile@example.com", "code": "000000",
            "username": "profileuser", "password": "password123"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reg}");
    let user_id = reg["user_id"].as_str().expect("user_id").to_owned();

    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/login",
        None,
        Some(json!({ "identifier": "profileuser", "password": "password123" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user_id"], Value::String(user_id), "{body}");
    assert_eq!(body["username"], "profileuser");
}
#[tokio::test]
async fn register_with_bad_code_returns_dedicated_machine_code() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (status, body) = send(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": "codefail@example.com", "code": "111111",
            "username": "codefail", "password": "password123"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], "invalid_or_expired_code", "{body}");
}