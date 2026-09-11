//! M4 integration tests: offline-push abstraction end-to-end through the
//! Mock channel (`jiuyue_test` PG + Redis live stack).
//!
//! Harness mirrors `chat_flow.rs` (same advisory-lock key so the two
//! integration binaries never reset/race each other's schema) and keeps an
//! [`AppState`] handle so tests can read the Mock channel snapshot directly
//! in addition to the dev-only `/api/dev/push-log` HTTP surface.
//!
//! Covered behavior:
//! 1. A sends while B is fully offline (never connected) → exactly one mock
//!    envelope addressed to B's seeded device push_token with the clamped
//!    plaintext preview and the right conversation_id.
//! 2. Online members generate NO pushes.
//! 3. Recalled messages add NO push (Message kind fires on fresh msg.new
//!    only).
//! 4. `/api/dev/push-log` returns the recorded entries in dev builds.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use jiuyue_protocol::{Frame, MsgAck, MsgNew, MsgRecall, MsgSend, PROTOCOL_VERSION, Payload};
use jiuyue_server::push::{PREVIEW_MAX_CHARS, PushKind, PushPlatform, TOKEN_PREFIX_LEN};
use jiuyue_server::state::AppState;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower::ServiceExt;
use uuid::Uuid;

/// Serializes tests within this binary and guards the one-time schema reset.
/// Same key as every JiuYue test binary (cross-binary mutual exclusion).
static GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::const_new(()));
static SCHEMA_READY: AtomicBool = AtomicBool::new(false);

/// Arbitrary fixed key shared by every JiuYue test binary.
const TEST_ADVISORY_LOCK_KEY: i64 = 0x6A_75_59_55_00_01;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestApp {
    app: Router,
    pool: PgPool,
    port: u16,
    /// Kept so tests can inspect the Mock push channel directly.
    state: AppState,
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

    // Real TCP listener for WS traffic + an oneshot router for HTTP calls.
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
        app: jiuyue_server::build_router(state.clone()),
        pool,
        port,
        state,
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

async fn create_conversation(t: &TestApp, creator_access: &str, peer_username: &str) -> i64 {
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(creator_access),
        Some(json!({ "peer_username": peer_username })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["conversation_id"].as_i64().expect("numeric id")
}

async fn mint_ticket(t: &TestApp, access: &str) -> String {
    let (status, body) = send_http(&t.app, "POST", "/api/auth/ws-ticket", Some(access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["ticket"].as_str().expect("ticket").to_owned()
}

async fn ws_connect(t: &TestApp, access: &str) -> WsClient {
    let ticket = mint_ticket(t, access).await;
    let url = format!("ws://127.0.0.1:{}/ws?ticket={ticket}&platform=web", t.port);
    let (ws, _response) = tokio_tungstenite::connect_async(url)
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
    .expect("send ok");
}

async fn ws_next_frame(ws: &mut WsClient) -> Frame {
    let msg = tokio::time::timeout(READ_TIMEOUT, ws.next())
        .await
        .expect("timed out waiting for websocket frame")
        .expect("stream must stay open")
        .expect("ws stream healthy");
    let text = msg.to_text().expect("text frame").to_owned();
    serde_json::from_str::<Frame>(&text).expect("wire frame must decode into Frame")
}

/// Sends one plaintext message over A's socket and returns the ACKed
/// `(message_id, seq)` after verifying the persist-then-ack contract.
async fn send_and_ack(
    ws_a: &mut WsClient,
    conversation_id: i64,
    client_msg_id: Uuid,
    body: &str,
) -> (Uuid, i64) {
    let send_frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id,
            body: body.to_owned(),
            reply_to: None,
            media: None,
        }),
    };
    ws_send_text(
        ws_a,
        &serde_json::to_string(&send_frame).expect("serialize"),
    )
    .await;
    let ack = ws_next_frame(ws_a).await;
    match ack.payload {
        Payload::MsgAck(MsgAck {
            client_msg_id: got,
            message_id,
            seq,
            duplicate,
        }) => {
            assert_eq!(got, client_msg_id);
            assert!(!duplicate);
            (message_id, seq)
        }
        other => panic!("expected msg.ack, got {other:?}"),
    }
}

/// Seeds one device row with a push token directly via SQL (the recipient
/// never connects, so no device row exists otherwise).
async fn seed_push_device(t: &TestApp, user_id: Uuid, platform: &str, push_token: &str) -> Uuid {
    let device_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO devices (id, user_id, platform, push_token, last_seen_at) \
         VALUES ($1, $2, $3, $4, now())",
    )
    .bind(device_id)
    .bind(user_id)
    .bind(platform)
    .bind(push_token)
    .execute(&t.pool)
    .await
    .expect("seed device row");
    device_id
}

/// Polls the Mock snapshot until `predicate` holds (fire-and-forget dispatch
/// needs a beat to land); panics after ~5s.
async fn wait_for_mock<F>(t: &TestApp, predicate: F) -> Vec<jiuyue_server::push::MockEntry>
where
    F: Fn(&[jiuyue_server::push::MockEntry]) -> bool,
{
    for _ in 0..50 {
        let snapshot = t.state.push.mock().snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("mock push snapshot never satisfied the predicate");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn offline_member_gets_one_mock_envelope_with_preview_and_conversation() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "m4-ada@example.com", "m4ada").await;
    let (b_id, _b_access) = register_user(&t, "m4-ben@example.com", "m4ben").await;
    let conversation_id = create_conversation(&t, &a_access, "m4ben").await;

    // B is FULLY offline (never connects); only a seeded device row exists.
    const B_TOKEN: &str = "apns-b-token-0123456789abcdef";
    let b_device_id = seed_push_device(&t, b_id, "ios", B_TOKEN).await;

    let mut ws_a = ws_connect(&t, &a_access).await;

    // Body longer than the 40-char preview cap, mixing multibyte chars.
    let body = format!("离线推送预览测试：{}", "内容内容".repeat(12));
    let (message_id, _seq) = send_and_ack(&mut ws_a, conversation_id, Uuid::now_v7(), &body).await;

    let expected_preview: String = body.chars().take(PREVIEW_MAX_CHARS).collect();
    let snapshot = wait_for_mock(&t, |entries| {
        entries
            .iter()
            .any(|e| e.envelope.conversation_id == conversation_id)
    })
    .await;

    let matching: Vec<_> = snapshot
        .iter()
        .filter(|e| e.envelope.conversation_id == conversation_id)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one envelope for the conversation"
    );
    let entry = matching[0];
    assert_eq!(entry.platform, PushPlatform::Ios);
    assert_eq!(
        entry.token_prefix,
        B_TOKEN[..TOKEN_PREFIX_LEN],
        "only the prefix is retained"
    );
    assert_eq!(entry.envelope.to_user_id, b_id);
    assert_eq!(entry.envelope.device_id, b_device_id);
    assert_eq!(entry.envelope.kind, PushKind::Message);
    assert_eq!(
        entry.envelope.sender_username_preview, expected_preview,
        "preview must be the first 40 chars of the plaintext body"
    );
    assert!(!entry.dispatched_at_rfc3339.is_empty());

    // Dev-only debug surface reflects the same record.
    let (status, log) = send_http(&t.app, "GET", "/api/dev/push-log", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = log.as_array().expect("push-log JSON array");
    assert!(
        entries.iter().any(|e| {
            e["envelope"]["conversation_id"] == json!(conversation_id)
                && e["envelope"]["device_id"] == json!(b_device_id.to_string())
                && e["envelope"]["kind"] == json!("Message")
                && e["token_prefix"] == json!(&B_TOKEN[..TOKEN_PREFIX_LEN])
        }),
        "push-log must contain the recorded envelope: {log}"
    );

    // The pushed message id matches what the sender had ACKed (via data on
    // the wire shape this milestone carries conversation/device identity;
    // message linkage is proven by the single-envelope invariant above).
    let _ = message_id;
}

#[tokio::test]
async fn online_members_do_not_generate_pushes() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "m4-carl@example.com", "m4carl").await;
    let (b_id, b_access) = register_user(&t, "m4-dora@example.com", "m4dora").await;
    let conversation_id = create_conversation(&t, &a_access, "m4dora").await;

    // Even a seeded token must NOT be used while B is online.
    seed_push_device(&t, b_id, "ios", "apns-online-token-00112233").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let (_, _seq) = send_and_ack(&mut ws_a, conversation_id, Uuid::now_v7(), "online hello").await;

    // B receives the live fanout — proof the send pipeline completed.
    let delivered = ws_next_frame(&mut ws_b).await;
    match delivered.payload {
        Payload::MsgNew(MsgNew {
            conversation_id: conv,
            body,
            ..
        }) => {
            assert_eq!(conv, conversation_id);
            assert_eq!(body, "online hello");
        }
        other => panic!("expected msg.new, got {other:?}"),
    }

    // Settle window: any wrongly-spawned push would land here.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let snapshot = t.state.push.mock().snapshot();
    assert!(
        snapshot
            .iter()
            .all(|e| e.envelope.conversation_id != conversation_id),
        "online recipients must never be pushed: {snapshot:?}"
    );
}

#[tokio::test]
async fn recalled_messages_add_no_extra_push() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "m4-erin@example.com", "m4erin").await;
    let (b_id, _b_access) = register_user(&t, "m4-finn@example.com", "m4finn").await;
    let conversation_id = create_conversation(&t, &a_access, "m4finn").await;
    seed_push_device(&t, b_id, "android-fcm", "fcm-recall-token-aabbccdd").await;

    let mut ws_a = ws_connect(&t, &a_access).await;

    let (message_id, _seq) = send_and_ack(
        &mut ws_a,
        conversation_id,
        Uuid::now_v7(),
        "will be recalled",
    )
    .await;

    let snapshot = wait_for_mock(&t, |entries| {
        entries
            .iter()
            .any(|e| e.envelope.conversation_id == conversation_id)
    })
    .await;
    let pushes_before_recall = snapshot
        .iter()
        .filter(|e| e.envelope.conversation_id == conversation_id)
        .count();
    assert_eq!(
        pushes_before_recall, 1,
        "the fresh message itself pushed once"
    );

    // Recall inside the policy window; the echoed msg.recalled frame proves
    // the whole recall pipeline (persist + broadcast) has settled.
    let recall = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgRecall(MsgRecall {
            conversation_id,
            message_id,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&recall).expect("serialize"),
    )
    .await;
    let echoed = ws_next_frame(&mut ws_a).await;
    assert!(
        matches!(echoed.payload, Payload::MsgRecalled(_)),
        "expected msg.recalled echo, got {:?}",
        echoed.payload
    );

    // Recall broadcasts are registry-only: no additional envelope.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let snapshot = t.state.push.mock().snapshot();
    let pushes_after_recall = snapshot
        .iter()
        .filter(|e| e.envelope.conversation_id == conversation_id)
        .count();
    assert_eq!(
        pushes_after_recall, 1,
        "recalls must not generate pushes: {snapshot:?}"
    );
}
