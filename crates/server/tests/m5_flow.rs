//! M5 friend-system integration tests: request lifecycle (send → inbox →
//! accept → friend lists → unfriend), duplicate/self/not-found policies,
//! decline + cancel + re-send, and the fire-and-forget `friend.requested` /
//! `friend.accepted` wire relays — against the live local stack (`jiuyue_test`
//! PG + Redis).
//!
//! Harness mirrors `m2_flow.rs` / `sync_flow.rs` (global mutex + one-time
//! schema reset + per-test TRUNCATE + cross-binary PG advisory lock +
//! ephemeral-port WS + ping-filtering frame reader).

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use jiuyue_protocol::{Frame, FriendAccepted, FriendRequested, Payload, UserIdentity};
use jiuyue_server::state::AppState;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

/// Serializes tests within this binary and guards the one-time schema reset.
static GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::const_new(()));
static SCHEMA_READY: AtomicBool = AtomicBool::new(false);

/// Arbitrary fixed key shared by every JiuYue test binary.
const TEST_ADVISORY_LOCK_KEY: i64 = 0x6A_75_59_55_00_01;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const SILENCE_PROBE: Duration = Duration::from_millis(700);

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestApp {
    app: Router,
    /// Direct DB access for assertions that bypass the REST surface.
    _pool: PgPool,
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
         conversations, conversation_members, messages, \
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
        _pool: pool,
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

fn error_code(body: &Value) -> &str {
    body["error"].as_str().expect("machine code present")
}

async fn register_user(t: &TestApp, email: &str, username: &str) -> (Uuid, String) {
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
    (user_id, access)
}

/// Sends `POST /api/friends/requests` and returns `(request_id, status, body)`.
async fn send_friend_request(
    t: &TestApp,
    access: &str,
    username: &str,
) -> (Option<Uuid>, StatusCode, Value) {
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/friends/requests",
        Some(access),
        Some(json!({ "username": username })),
    )
    .await;
    let request_id = body["request_id"]
        .as_str()
        .and_then(|raw| Uuid::parse_str(raw).ok());
    (request_id, status, body)
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

// ---------------------------------------------------------------------------
// Lifecycle: send → inbox → accept → friend lists → unfriend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn friend_lifecycle_send_accept_list_unfriend() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "fr-alice@example.com", "fralice").await;
    let (b_id, b_access) = register_user(&t, "fr-bob@example.com", "frbob").await;

    // A sends a request to B.
    let (request_id, status, body) = send_friend_request(&t, &a_access, "frbob").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let request_id = request_id.expect("created request carries an id");
    assert_eq!(
        body["to"]["user_id"].as_str().expect("to.user_id"),
        b_id.to_string(),
        "response names the resolved peer"
    );
    assert_eq!(body["to"]["username"], json!("frbob"));

    // B's inbox shows exactly one incoming request from A.
    let (status, body) = send_http(&t.app, "GET", "/api/friends/requests", Some(&b_access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let incoming = body["incoming"].as_array().expect("incoming array");
    assert_eq!(incoming.len(), 1, "{body}");
    assert_eq!(
        incoming[0]["request_id"].as_str().expect("request_id"),
        request_id.to_string()
    );
    assert_eq!(
        incoming[0]["from"]["user_id"].as_str().expect("from.user_id"),
        a_id.to_string()
    );
    assert_eq!(incoming[0]["from"]["username"], json!("fralice"));
    assert!(incoming[0]["created_at"].is_string(), "RFC3339 created_at");
    assert!(
        body["outgoing"].as_array().expect("outgoing array").is_empty(),
        "B sent nothing: {body}"
    );

    // A's outbox mirrors it.
    let (status, body) = send_http(&t.app, "GET", "/api/friends/requests", Some(&a_access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let outgoing = body["outgoing"].as_array().expect("outgoing array");
    assert_eq!(outgoing.len(), 1, "{body}");
    assert_eq!(
        outgoing[0]["request_id"].as_str().expect("request_id"),
        request_id.to_string()
    );
    assert_eq!(
        outgoing[0]["to"]["user_id"].as_str().expect("to.user_id"),
        b_id.to_string()
    );

    // B accepts → response names A (the person just befriended).
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{request_id}/accept"),
        Some(&b_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["friend"]["user_id"].as_str().expect("friend.user_id"),
        a_id.to_string()
    );
    assert_eq!(body["friend"]["username"], json!("fralice"));

    // Both friend lists show the symmetric edge.
    for (access, peer_id, peer_username) in [
        (&a_access, b_id, "frbob"),
        (&b_access, a_id, "fralice"),
    ] {
        let (status, body) = send_http(&t.app, "GET", "/api/friends", Some(access), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let friends = body.as_array().expect("friend list array");
        assert_eq!(friends.len(), 1, "{body}");
        assert_eq!(friends[0]["user_id"].as_str().expect("user_id"), peer_id.to_string());
        assert_eq!(friends[0]["username"], json!(peer_username));
        assert!(friends[0]["since"].is_string(), "RFC3339 since");
    }

    // Unfriend removes BOTH rows; both lists end up empty.
    let (status, body) = send_http(
        &t.app,
        "DELETE",
        &format!("/api/friends/{b_id}"),
        Some(&a_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    for access in [&a_access, &b_access] {
        let (status, body) = send_http(&t.app, "GET", "/api/friends", Some(access), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.as_array().expect("array").is_empty(), "{body}");
    }
}

// ---------------------------------------------------------------------------
// Duplicate / self / unknown-peer policies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_pending_conflicts_in_both_directions_and_unknown_peer_404s() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_c_id, c_access) = register_user(&t, "dup-carol@example.com", "dupcarol").await;
    let (_d_id, d_access) = register_user(&t, "dup-dave@example.com", "dupdave").await;

    let (_, status, body) = send_friend_request(&t, &c_access, "dupdave").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Same-direction duplicate.
    let (_, status, body) = send_friend_request(&t, &c_access, "dupdave").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "request_already_pending");

    // Reverse direction counts as a duplicate too.
    let (_, status, body) = send_friend_request(&t, &d_access, "dupcarol").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "request_already_pending");

    // Unknown username → machine-coded 404.
    let (_, status, body) = send_friend_request(&t, &c_access, "nosuchpeer").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(error_code(&body), "peer_not_found");
}

#[tokio::test]
async fn self_request_and_blank_username_are_rejected() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "self-amy@example.com", "selfamy").await;

    let (_, status, body) = send_friend_request(&t, &a_access, "selfamy").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(error_code(&body), "self_request");

    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/friends/requests",
        Some(&a_access),
        Some(json!({ "username": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(error_code(&body), "bad_request");
}

// ---------------------------------------------------------------------------
// Decline / cancel / re-send
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declined_request_can_be_resent_and_cancelled_request_too() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_e_id, e_access) = register_user(&t, "dec-erin@example.com", "decerin").await;
    let (_f_id, f_access) = register_user(&t, "dec-finn@example.com", "decfinn").await;

    // Erin → Finn, Finn declines.
    let (rid, status, body) = send_friend_request(&t, &e_access, "decfinn").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rid = rid.expect("request id");
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid}/decline"),
        Some(&f_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // Inbox/outbox drained after the decline.
    let (_, body) = send_http(&t.app, "GET", "/api/friends/requests", Some(&f_access), None).await;
    assert!(body["incoming"].as_array().expect("array").is_empty(), "{body}");
    let (_, body) = send_http(&t.app, "GET", "/api/friends/requests", Some(&e_access), None).await;
    assert!(body["outgoing"].as_array().expect("array").is_empty(), "{body}");

    // Re-send after decline is ALLOWED (row flipped back to pending).
    let (rid2, status, body) = send_friend_request(&t, &e_access, "decfinn").await;
    assert_eq!(status, StatusCode::CREATED, "declined pair may re-request: {body}");
    let rid2 = rid2.expect("request id");
    let (_, body) = send_http(&t.app, "GET", "/api/friends/requests", Some(&f_access), None).await;
    let incoming = body["incoming"].as_array().expect("array");
    assert_eq!(incoming.len(), 1, "{body}");
    assert_eq!(incoming[0]["request_id"].as_str().expect("id"), rid2.to_string());

    // Sender cancel: Gina → Hank, Gina cancels before Hank reacts.
    let (_g_id, g_access) = register_user(&t, "can-gina@example.com", "cangina").await;
    let (_h_id, h_access) = register_user(&t, "can-hank@example.com", "canhank").await;
    let (rid3, status, body) = send_friend_request(&t, &g_access, "canhank").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rid3 = rid3.expect("request id");
    let (status, body) = send_http(
        &t.app,
        "DELETE",
        &format!("/api/friends/requests/{rid3}"),
        Some(&g_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, body) = send_http(&t.app, "GET", "/api/friends/requests", Some(&h_access), None).await;
    assert!(body["incoming"].as_array().expect("array").is_empty(), "{body}");
    let (_, body) = send_http(&t.app, "GET", "/api/friends/requests", Some(&g_access), None).await;
    assert!(body["outgoing"].as_array().expect("array").is_empty(), "{body}");

    // Cancelled pair may also re-request.
    let (_, status, body) = send_friend_request(&t, &g_access, "canhank").await;
    assert_eq!(status, StatusCode::CREATED, "cancelled pair may re-request: {body}");

    // Decline the re-sent request once more: its id is terminal again until
    // yet another re-send flips the row back (accepting a declined request
    // answers 404, never resurrects it).
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid2}/decline"),
        Some(&f_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid2}/accept"),
        Some(&f_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(error_code(&body), "not_found");
}

// ---------------------------------------------------------------------------
// Recipient-only authorization on accept/decline/cancel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accept_is_recipient_only_and_double_accept_conflicts() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_i_id, i_access) = register_user(&t, "acc-ivy@example.com", "accivy").await;
    let (_j_id, j_access) = register_user(&t, "acc-jack@example.com", "accjack").await;
    let (_k_id, k_access) = register_user(&t, "acc-kate@example.com", "acckate").await;

    let (rid, status, body) = send_friend_request(&t, &i_access, "accjack").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rid = rid.expect("request id");

    // Outsider cannot accept (existence hidden behind a generic 404).
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid}/accept"),
        Some(&k_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(error_code(&body), "not_found");

    // The SENDER cannot accept their own request either.
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid}/accept"),
        Some(&i_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Unknown request id → 404.
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{}/accept", Uuid::now_v7()),
        Some(&j_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Recipient accepts; second accept hits already_friends.
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid}/accept"),
        Some(&j_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid}/accept"),
        Some(&j_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "already_friends");

    // Decline after acceptance is also gone (no longer pending).
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid}/decline"),
        Some(&j_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Requesting again once befriended conflicts as already_friends.
    let (_, status, body) = send_friend_request(&t, &i_access, "accjack").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "already_friends");
}

// ---------------------------------------------------------------------------
// Wire frames: friend.requested / friend.accepted reach live sockets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn friend_wire_frames_reach_live_sockets_only() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "wire-alice@example.com", "wirealice").await;
    let (b_id, b_access) = register_user(&t, "wire-bob@example.com", "wirebob").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    // REST send → recipient socket gets friend.requested naming the sender.
    let (rid, status, body) = send_friend_request(&t, &a_access, "wirebob").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rid = rid.expect("request id");
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::FriendRequested(FriendRequested {
            request_id,
            from: UserIdentity { user_id, username },
        }) => {
            assert_eq!(request_id, rid);
            assert_eq!(user_id, a_id, "relay stamps the authenticated sender");
            assert_eq!(username, "wirealice");
        }
        other => panic!("expected friend.requested on recipient socket, got {other:?}"),
    }

    // REST accept → ORIGINAL SENDER socket gets friend.accepted naming the accepter.
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/friends/requests/{rid}/accept"),
        Some(&b_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::FriendAccepted(FriendAccepted {
            friend: UserIdentity { user_id, username },
        }) => {
            assert_eq!(user_id, b_id, "the frame names WHO accepted");
            assert_eq!(username, "wirebob");
        }
        other => panic!("expected friend.accepted on sender socket, got {other:?}"),
    }

    // Fire-and-forget means exactly one frame each — nothing else follows.
    let extra_a = ws_next_text(&mut ws_a, SILENCE_PROBE).await;
    assert!(extra_a.is_none(), "sender socket must stay silent, got {extra_a:?}");
    let extra_b = ws_next_text(&mut ws_b, SILENCE_PROBE).await;
    assert!(extra_b.is_none(), "recipient socket must stay silent, got {extra_b:?}");
}
