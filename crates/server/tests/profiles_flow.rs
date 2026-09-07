//! M7 profile + XP integration tests.
//!
//! Coverage (mirroring the frozen rules the frontend renders in parallel):
//!
//! * profile GET (self + others, 404 on unknown, bearer-gated) and the
//!   display_name→username fallback;
//! * profile PATCH (set/clear display_name + bio + avatar, 1–24 char display
//!   name, 200-char bio cap, curated-emoji whitelist → 422 on a stranger);
//! * the XP economy end-to-end: +20 daily-login bonus exactly once per UTC
//!   day (two ws opens on the same day never double-grant), +10 XP per 100
//!   chars of a sent plaintext message with a hard 200/day cap, and secret
//!   (`e2ee`) messages earning nothing;
//! * level recomputation on award + level/title/xp/xp_to_next projection on
//!   the profile endpoint;
//! * additive peer identity (`display_name`/`avatar`) riding on conversation
//!   create/list, friend requests/list, and user search.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use jiuyue_protocol::{E2eeMsg, Frame, MsgAck, MsgSend, Payload, PROTOCOL_VERSION};
use jiuyue_server::state::AppState;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower::ServiceExt;
use uuid::Uuid;

/// Serializes tests within this binary and guards the one-time schema reset.
static GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::const_new(()));
static SCHEMA_READY: AtomicBool = AtomicBool::new(false);

/// Arbitrary fixed key shared by every JiuYue test binary.
const TEST_ADVISORY_LOCK_KEY: i64 = 0x6A_75_59_55_00_01;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Generous ceiling for fire-and-forget XP side effects (spawned award tasks).
const EVENTUALLY_TIMEOUT: Duration = Duration::from_secs(15);

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestApp {
    app: Router,
    pool: PgPool,
    port: u16,
    /// Holds the advisory lock; dropping it releases the lock (session end).
    _lock_conn: PgConnection,
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env var {name}"))
}

async fn test_app() -> TestApp {
    dotenvy::dotenv().ok();

    static LOG_INIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOG_INIT.swap(true, Ordering::SeqCst) {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("jiuyue_server=debug"))
            .try_init()
            .ok();
    }

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
        .max_connections(8)
        .connect(&env_var("TEST_DATABASE_URL"))
        .await
        .expect("connect TEST_DATABASE_URL");
    sqlx::query(
        "TRUNCATE users, auth_identities, devices, refresh_tokens, \
         conversations, conversation_members, messages, xp_accounts, \
         friend_requests, friendships RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate tables");
    let redis_client = redis::Client::open(env_var("REDIS_URL").as_str()).expect("parse redis url");
    let redis = redis::aio::ConnectionManager::new(redis_client)
        .await
        .expect("connect redis");
    let state = AppState::new(pool.clone(), redis.clone(), env_var("JIUYUE_JWT_SECRET"));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    let served = jiuyue_server::build_router(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, served)
            .await
            .expect("server task failed");
    });

    TestApp {
        app: jiuyue_server::build_router(state),
        pool,
        port,
        _lock_conn: lock_conn,
    }
}

async fn send_http(
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

async fn register_user(t: &TestApp, email: &str, username: &str) -> (Uuid, String, i64) {
    let (status, _) = send_http(
        &t.app,
        "POST",
        "/api/auth/request-code",
        None,
        Some(json!({"channel": "email", "target": email})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "request-code for {email}");
    let (status, reg) = send_http(
        &t.app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({
            "channel": "email", "target": email, "code": "000000",
            "username": username, "password": "password123"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reg}");
    let user_id: Uuid = reg["user_id"]
        .as_str()
        .expect("user_id")
        .parse()
        .expect("uuid");
    let access = reg["access_token"].as_str().expect("access").to_owned();
    let uid: i64 = reg["uid"].as_i64().expect("uid");
    (user_id, access, uid)
}

async fn ws_connect(t: &TestApp, access: &str) -> WsClient {
    let (status, body) =
        send_http(&t.app, "POST", "/api/auth/ws-ticket", Some(access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ticket = body["ticket"].as_str().expect("ticket").to_owned();
    let url = format!("ws://127.0.0.1:{}/ws?ticket={ticket}&platform=web", t.port);
    let (ws, _response) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .expect("websocket upgrade succeeds");
    ws
}

async fn ws_send_text(ws: &mut WsClient, text: &str) {
    tokio::time::timeout(
        READ_TIMEOUT,
        ws.send(WsMessage::Text(text.to_owned().into())),
    )
    .await
    .expect("send timeout")
    .expect("ws send");
}

/// Next TEXT frame or `None` on timeout/close; skips Ping/Pong noise.
async fn ws_next_text(ws: &mut WsClient, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(msg))) => {
                if msg.is_text() {
                    return Some(msg.into_text().expect("text payload").to_string());
                }
                if msg.is_close() {
                    return None;
                }
            }
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_elapsed) => return None,
        }
    }
}

async fn ws_next_frame(ws: &mut WsClient) -> Frame {
    let text = ws_next_text(ws, READ_TIMEOUT)
        .await
        .expect("stream must deliver a text frame");
    serde_json::from_str::<Frame>(&text).expect("wire frame must decode into jiuyue_protocol::Frame")
}

/// Creates a direct conversation over HTTP; returns its id.
async fn create_conversation(t: &TestApp, creator: &str, peer_username: &str) -> i64 {
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(creator),
        Some(json!({ "peer_username": peer_username })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["conversation_id"].as_i64().expect("numeric id")
}

/// Sends one `msg.send` over an open socket; returns after the ack lands.
async fn send_and_ack(ws: &mut WsClient, conversation_id: i64, body: &str) -> Uuid {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: body.to_owned(),
            reply_to: None,
        }),
    };
    ws_send_text(ws, &serde_json::to_string(&frame).expect("serialize")).await;
    match ws_next_frame(ws).await.payload {
        Payload::MsgAck(MsgAck { message_id, .. }) => message_id,
        other => panic!("expected msg.ack, got {other:?}"),
    }
}

/// Polls `cond` until true or `timeout` elapses (fire-and-forget XP tasks).
async fn eventually<F, Fut>(mut cond: F, timeout: Duration, what: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if cond().await {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Reads the raw xp_accounts row for `user_id` (test shortcut). Returns
/// `(xp, level, last_daily_bonus_date.day(), msg_xp_today)`.
async fn xp_row(t: &TestApp, user_id: Uuid) -> (i64, i32, Option<u8>, i32) {
    let row: (i64, i32, Option<time::Date>, i32) = sqlx::query_as(
        "SELECT xp, level, last_daily_bonus_date, msg_xp_today \
         FROM xp_accounts WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&t.pool)
    .await
    .expect("xp_accounts row exists");
    let (xp, level, last_bonus, msg_today) = row;
    (xp, level, last_bonus.map(|d| d.day()), msg_today)
}

// ---------------------------------------------------------------------------
// Profile GET / PATCH
// ---------------------------------------------------------------------------

#[tokio::test]
async fn profile_get_serves_self_and_others_with_username_fallback() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access, a_uid) = register_user(&t, "pf-a@example.com", "pfalice").await;
    let (b_id, b_access, b_uid) = register_user(&t, "pf-b@example.com", "pfbob").await;

    // Fresh user: empty display_name/bio/avatar, level 1, 0 XP, xp_to_next 50.
    for (target_id, token, uid, username) in [
        (a_id, &a_access, a_uid, "pfalice"),
        (b_id, &b_access, b_uid, "pfbob"),
    ] {
        let (status, body) = send_http(
            &t.app,
            "GET",
            &format!("/api/users/profile/{target_id}"),
            Some(token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["user_id"].as_str().expect("user_id"), target_id.to_string());
        assert_eq!(body["uid"], json!(uid));
        assert_eq!(body["username"], username);
        assert_eq!(body["display_name"], username, "empty display_name falls back to username");
        assert_eq!(body["bio"], "");
        assert_eq!(body["avatar"], "");
        assert_eq!(body["level"], 1);
        assert_eq!(body["title"], "土块");
        assert_eq!(body["xp"], 0);
        assert_eq!(body["xp_to_next"], 50);
    }

    // Bearer required.
    let (status, _) = send_http(
        &t.app,
        "GET",
        &format!("/api/users/profile/{a_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Unknown target → machine-coded 404.
    let (status, body) = send_http(
        &t.app,
        "GET",
        &format!("/api/users/profile/{}", Uuid::now_v7()),
        Some(&a_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "not_found");

    // Peer can read A's profile too (same route, no privacy split).
    let (status, body) = send_http(
        &t.app,
        "GET",
        &format!("/api/users/profile/{a_id}"),
        Some(&b_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["username"], "pfalice");
}

#[tokio::test]
async fn profile_patch_updates_and_clears_fields() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access, _a_uid) = register_user(&t, "up-a@example.com", "upalice").await;
    let (_b_id, b_access, _b_uid) = register_user(&t, "up-b@example.com", "upbob").await;

    // Set everything in one shot.
    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&a_access),
        Some(json!({
            "display_name": "  小矿工  ",
            "bio": "digging for diamonds",
            "avatar": "⛏️",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "小矿工", "display_name is trimmed");
    assert_eq!(body["bio"], "digging for diamonds");
    assert_eq!(body["avatar"], "⛏️");
    assert_eq!(body["username"], "upalice");

    // Partial patch touches only the present field.
    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&a_access),
        Some(json!({ "bio": "now deeper" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "小矿工", "display_name untouched");
    assert_eq!(body["bio"], "now deeper");

    // Explicit empty clears display_name → profile falls back to username.
    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&a_access),
        Some(json!({ "display_name": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "upalice", "cleared display_name falls back to username");
    assert_eq!(body["avatar"], "⛏️", "other fields survive a display_name clear");

    // Persistence is visible to a third party reading the profile.
    let (status, body) = send_http(
        &t.app,
        "GET",
        &format!("/api/users/profile/{a_id}"),
        Some(&b_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "upalice");
    assert_eq!(body["bio"], "now deeper");
    assert_eq!(body["avatar"], "⛏️");
}

#[tokio::test]
async fn profile_patch_validates_lengths_and_avatar_membership() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access, _a_uid) = register_user(&t, "pv-a@example.com", "pvlarry").await;

    // display_name > 24 chars → 422.
    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&a_access),
        Some(json!({ "display_name": "x".repeat(25) })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "validation_error");

    // bio > 200 chars → 422.
    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&a_access),
        Some(json!({ "bio": "字".repeat(201) })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "validation_error");

    // Avatar outside the curated set → 422.
    for bad in ["🐶", "🤖", "https://evil.example/a.png", "⛏️x"] {
        let (status, body) = send_http(
            &t.app,
            "PATCH",
            "/api/users/profile",
            Some(&a_access),
            Some(json!({ "avatar": bad })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "avatar {bad:?}: {body}");
        assert_eq!(body["error"], "validation_error");
    }

    // 24-char display_name and 200-char bio are exactly accepted.
    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&a_access),
        Some(json!({
            "display_name": "d".repeat(24),
            "bio": "字".repeat(200),
            "avatar": "💎",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "d".repeat(24));
}

#[tokio::test]
async fn patch_is_self_only_even_for_known_peers() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access, _a_uid) = register_user(&t, "so-a@example.com", "soalice").await;
    let (b_id, b_access, _b_uid) = register_user(&t, "so-b@example.com", "sobob").await;

    // The PATCH route has no path parameter, so it always edits the caller's
    // own row. A's patch must never touch B's profile.
    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&a_access),
        Some(json!({ "display_name": "impersonator" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = send_http(
        &t.app,
        "GET",
        &format!("/api/users/profile/{b_id}"),
        Some(&b_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "sobob", "peer profile never touched");
}

// ---------------------------------------------------------------------------
// XP economy: daily login bonus + message XP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daily_login_bonus_grants_20_xp_once_per_utc_day() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access, _a_uid) = register_user(&t, "xp-a@example.com", "xpalice").await;

    // First connection of the day → +20, row created lazily.
    let _ws_a = ws_connect(&t, &a_access).await;
    eventually(
        || async {
            let (xp, level, bonus_day, _msg) = xp_row(&t, a_id).await;
            xp == 20 && level == 1 && bonus_day.is_some()
        },
        EVENTUALLY_TIMEOUT,
        "daily login bonus on first connect",
    )
    .await;

    // Second + third connections same UTC day → no double grant.
    for _ in 0..2 {
        let ws = ws_connect(&t, &a_access).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(ws);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (xp, level, _bonus_day, _msg) = xp_row(&t, a_id).await;
    assert_eq!(xp, 20, "daily bonus is once per UTC day, never compounded");
    assert_eq!(level, 1);

    // The profile endpoint reflects it.
    let (_, body) = send_http(
        &t.app,
        "GET",
        &format!("/api/users/profile/{a_id}"),
        Some(&a_access),
        None,
    )
    .await;
    assert_eq!(body["xp"], 20);
    assert_eq!(body["level"], 1);
    assert_eq!(body["title"], "土块");
    assert_eq!(body["xp_to_next"], 30, "50 req - 20 progress = 30 left");
}

#[tokio::test]
async fn message_xp_is_10_per_100_chars_and_short_messages_earn_nothing() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access, _a_uid) = register_user(&t, "mx-a@example.com", "mxalice").await;
    let (_b_id, _b_access, _b_uid) = register_user(&t, "mx-b@example.com", "mxbob").await;
    let conv = create_conversation(&t, &a_access, "mxbob").await;
    let mut ws_a = ws_connect(&t, &a_access).await;

    // Waits for the daily bonus to land so XP deltas are deterministic.
    eventually(
        || async {
            let (xp, _, _, _) = xp_row(&t, a_id).await;
            xp == 20
        },
        EVENTUALLY_TIMEOUT,
        "daily bonus before messaging assertions",
    )
    .await;

    // < 100 chars → nothing.
    let _ = send_and_ack(&mut ws_a, conv, "short").await;
    eventually(
        || async {
            let (xp, _, _, msg_today) = xp_row(&t, a_id).await;
            xp == 20 && msg_today == 0
        },
        EVENTUALLY_TIMEOUT,
        "short message stays at daily-bonus XP",
    )
    .await;

    // 300 chars → exactly +30 (3 full 100-char blocks).
    let _ = send_and_ack(&mut ws_a, conv, &"x".repeat(300)).await;
    eventually(
        || async {
            let (xp, _, _, msg_today) = xp_row(&t, a_id).await;
            xp == 20 + 30 && msg_today == 30
        },
        EVENTUALLY_TIMEOUT,
        "300-char message awards 30 XP",
    )
    .await;

    // 250 chars → only 2 full blocks (+20), never 3 (floor).
    let _ = send_and_ack(&mut ws_a, conv, &"y".repeat(250)).await;
    eventually(
        || async {
            let (xp, _, _, msg_today) = xp_row(&t, a_id).await;
            xp == 20 + 30 + 20 && msg_today == 50
        },
        EVENTUALLY_TIMEOUT,
        "250-char message awards 20 XP",
    )
    .await;

    // Duplicate resend (same client_msg_id) must NOT double-award: only the
    // fresh delivery spawns an XP task.
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id: conv,
            client_msg_id: Uuid::now_v7(),
            body: "z".repeat(100),
            reply_to: None,
        }),
    };
    let wire = serde_json::to_string(&frame).expect("serialize");
    ws_send_text(&mut ws_a, &wire).await;
    let _ = ws_next_frame(&mut ws_a).await; // ack 1
    ws_send_text(&mut ws_a, &wire).await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(ack) => assert!(ack.duplicate, "second delivery is a duplicate"),
        other => panic!("expected duplicate msg.ack, got {other:?}"),
    }
    // The fresh send's XP task may still be in flight; the duplicate must not
    // award anything BEYOND that single fresh-send award (+10).
    eventually(
        || async {
            let (xp, _, _, msg_today) = xp_row(&t, a_id).await;
            xp == 20 + 30 + 20 + 10 && msg_today == 60
        },
        EVENTUALLY_TIMEOUT,
        "duplicate delivery awards nothing extra",
    )
    .await;
}

#[tokio::test]
async fn message_xp_caps_at_200_per_day() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access, _a_uid) = register_user(&t, "cap-a@example.com", "capalice").await;
    let (_b_id, _b_access, _b_uid) = register_user(&t, "cap-b@example.com", "capbob").await;
    let conv = create_conversation(&t, &a_access, "capbob").await;
    let mut ws_a = ws_connect(&t, &a_access).await;

    // Daily bonus lands first (deterministic baseline).
    eventually(
        || async {
            let (xp, _, _, _) = xp_row(&t, a_id).await;
            xp == 20
        },
        EVENTUALLY_TIMEOUT,
        "daily bonus before cap assertions",
    )
    .await;

    // 3 × 5000-char messages → each would earn 500 XP raw; the messaging
    // budget (separate from the daily bonus) must clamp at 200 total.
    for _ in 0..3 {
        let _ = send_and_ack(&mut ws_a, conv, &"m".repeat(5000)).await;
    }
    eventually(
        || async {
            let (xp, _, _, msg_today) = xp_row(&t, a_id).await;
            xp == 20 + 200 && msg_today == 200
        },
        EVENTUALLY_TIMEOUT,
        "messaging XP hard-caps at 200/day",
    )
    .await;

    // One more giant message adds nothing more today.
    let _ = send_and_ack(&mut ws_a, conv, &"n".repeat(5000)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (xp, level, _, msg_today) = xp_row(&t, a_id).await;
    assert_eq!(xp, 220, "no XP beyond the daily cap");
    assert_eq!(msg_today, 200);
    // Total 220 XP → cumulative thresholds 166 (level 4) / 233 (level 5).
    assert_eq!(level, 4);

    let (_, body) = send_http(
        &t.app,
        "GET",
        &format!("/api/users/profile/{a_id}"),
        Some(&a_access),
        None,
    )
    .await;
    assert_eq!(body["xp"], 220);
    assert_eq!(body["level"], 4);
    assert_eq!(body["title"], "土块", "band 1-5 is 土块");
    assert_eq!(body["xp_to_next"], 13, "req(4)=67 minus progress (220-166=54)");
}

#[tokio::test]
async fn secret_chat_messages_earn_zero_xp() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access, _a_uid) = register_user(&t, "se-a@example.com", "sealice").await;
    let (_b_id, _b_access, _b_uid) = register_user(&t, "se-b@example.com", "sebob").await;
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(&a_access),
        Some(json!({ "peer_username": "sebob", "kind": "secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let secret_conv = body["conversation_id"].as_i64().expect("numeric id");
    let mut ws_a = ws_connect(&t, &a_access).await;

    // Wait for the daily bonus so a later miss is attributable to secret-ness.
    eventually(
        || async {
            let (xp, _, _, _) = xp_row(&t, a_id).await;
            xp == 20
        },
        EVENTUALLY_TIMEOUT,
        "daily bonus in secret-chat test",
    )
    .await;

    // A 5000-char E2EE message would award 500 XP in a direct chat — here it
    // must award nothing (the server cannot read the plaintext).
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::E2eeMsg(E2eeMsg {
            conversation_id: secret_conv,
            client_msg_id: Uuid::now_v7(),
            ciphertext: "x".repeat(5000),
            message_type: 0,
        }),
    };
    ws_send_text(&mut ws_a, &serde_json::to_string(&frame).expect("serialize")).await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(_) => {}
        other => panic!("expected msg.ack for e2ee send, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (xp, _, _, msg_today) = xp_row(&t, a_id).await;
    assert_eq!(xp, 20, "secret-chat message earns 0 XP");
    assert_eq!(msg_today, 0);
}

#[tokio::test]
async fn level_is_stored_recomputed_from_accumulated_xp() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access, _a_uid) = register_user(&t, "lv-a@example.com", "lviana").await;
    let (_b_id, _b_access, _b_uid) = register_user(&t, "lv-b@example.com", "lvbob").await;
    let conv = create_conversation(&t, &a_access, "lvbob").await;
    let mut ws_a = ws_connect(&t, &a_access).await;

    // Wait for +20 daily, then keep messaging until XP clearly crosses the
    // level-3 boundary (105) and beyond.
    eventually(
        || async {
            let (xp, _, _, _) = xp_row(&t, a_id).await;
            xp == 20
        },
        EVENTUALLY_TIMEOUT,
        "daily bonus in level test",
    )
    .await;

    // 5 × 400-char messages = 5 × 40 = 200 messaging XP → total 220 (level 4).
    for _ in 0..5 {
        let _ = send_and_ack(&mut ws_a, conv, &"q".repeat(400)).await;
    }
    eventually(
        || async {
            let (xp, level, _, _) = xp_row(&t, a_id).await;
            xp == 220 && level == 4
        },
        EVENTUALLY_TIMEOUT,
        "stored level recomputed to 4 after 220 XP",
    )
    .await;

    let (_, body) = send_http(
        &t.app,
        "GET",
        &format!("/api/users/profile/{a_id}"),
        Some(&a_access),
        None,
    )
    .await;
    assert_eq!(body["level"], 4);
    assert_eq!(body["title"], "土块", "220 XP is still band 1-5");
    assert_eq!(body["xp"], 220);
    assert_eq!(body["xp_to_next"], 13, "req(4)=67 minus progress (220-166=54)");
}

// ---------------------------------------------------------------------------
// display_name / avatar ride along on peer surfaces
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conversation_and_search_surfaces_carry_display_name_and_avatar() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access, _a_uid) = register_user(&t, "dd-a@example.com", "ddalice").await;
    let (b_id, b_access, b_uid) = register_user(&t, "dd-b@example.com", "ddbob").await;

    // B sets a display_name + avatar.
    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&b_access),
        Some(json!({ "display_name": "Blocky", "avatar": "🪨" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Conversation create names the peer with the effective handle.
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(&a_access),
        Some(json!({ "peer_username": "ddbob" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["peer"]["username"], "ddbob");
    assert_eq!(body["peer"]["display_name"], "Blocky");
    assert_eq!(body["peer"]["avatar"], "🪨");
    assert_eq!(body["peer"]["uid"], json!(b_uid));

    // Conversation list carries the same block.
    let (status, body) = send_http(&t.app, "GET", "/api/conversations", Some(&a_access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("array");
    let item = items
        .iter()
        .find(|i| i["peer"]["user_id"] == json!(b_id.to_string()))
        .expect("conversation with b");
    assert_eq!(item["peer"]["display_name"], "Blocky");
    assert_eq!(item["peer"]["avatar"], "🪨");

    // Search hits carry display_name + avatar.
    let (status, body) = send_http(
        &t.app,
        "GET",
        "/api/users/search?q=ddbob",
        Some(&a_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hits = body.as_array().expect("array");
    assert_eq!(hits.len(), 1, "{body}");
    assert_eq!(hits[0]["display_name"], "Blocky");
    assert_eq!(hits[0]["avatar"], "🪨");
}

#[tokio::test]
async fn friends_surfaces_carry_display_name_and_avatar() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access, _a_uid) = register_user(&t, "fr-a@example.com", "frava").await;
    let (b_id, b_access, _b_uid) = register_user(&t, "fr-b@example.com", "frbruce").await;

    let (status, body) = send_http(
        &t.app,
        "PATCH",
        "/api/users/profile",
        Some(&b_access),
        Some(json!({ "display_name": "Blocky", "avatar": "🪨" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Request → the `to` block names the peer with display_name/avatar.
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/friends/requests",
        Some(&a_access),
        Some(json!({ "username": "frbruce" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["to"]["display_name"], "Blocky");
    assert_eq!(body["to"]["avatar"], "🪨");
    let request_id = body["request_id"].as_str().expect("request_id").to_owned();

    // Inbox (B side) carries the sender's identity block.
    let (status, body) = send_http(
        &t.app,
        "GET",
        "/api/friends/requests",
        Some(&b_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let incoming = body["incoming"].as_array().expect("incoming");
    assert_eq!(incoming.len(), 1, "{body}");
    assert_eq!(incoming[0]["from"]["user_id"], json!(a_id.to_string()));
    assert_eq!(incoming[0]["from"]["username"], "frava");
    assert_eq!(incoming[0]["from"]["display_name"], "frava", "empty display falls back to username");
    assert_eq!(incoming[0]["from"]["avatar"], "");

    // Accept → friend list carries the friend's display_name/avatar.
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{request_id}/accept"),
        Some(&b_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["friend"]["display_name"], "frava");
    assert_eq!(body["friend"]["avatar"], "");

    let (status, body) = send_http(&t.app, "GET", "/api/friends", Some(&a_access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let friends = body.as_array().expect("friends array");
    assert_eq!(friends.len(), 1, "{body}");
    assert_eq!(friends[0]["user_id"], json!(b_id.to_string()));
    assert_eq!(friends[0]["username"], "frbruce");
    assert_eq!(friends[0]["display_name"], "Blocky");
    assert_eq!(friends[0]["avatar"], "🪨");
}
