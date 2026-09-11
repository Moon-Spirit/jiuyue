//! M2 message-experience integration tests: read receipts, typing relay,
//! recall (policy + state machine + tombstone sync), and reply quoting —
//! against the live local stack (`jiuyue_test` PG + Redis).
//!
//! Harness mirrors `sync_flow.rs` (global mutex + one-time schema reset +
//! per-test TRUNCATE + cross-binary PG advisory lock + ephemeral-port WS +
//! ping-filtering frame reader + `eventually` poller).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use jiuyue_protocol::{
    ErrorCode, Frame, MsgAck, MsgNew, MsgRecall, MsgRecalled, MsgSend, PROTOCOL_VERSION, Payload,
    ReadReceipt, ReadUpdate, SyncCursor, SyncReq, Typing, TypingState,
};
use jiuyue_server::state::AppState;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
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
const SILENCE_PROBE: Duration = Duration::from_millis(700);

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

async fn ws_connect(t: &TestApp, access: &str) -> WsClient {
    let (status, body) = send_http(&t.app, "POST", "/api/auth/ws-ticket", Some(access), None).await;
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
    .expect("send ok");
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
    serde_json::from_str::<Frame>(&text)
        .expect("wire frame must decode into jiuyue_protocol::Frame")
}

fn expect_error_code(frame: Frame, expected: ErrorCode) -> jiuyue_protocol::ErrorPayload {
    match frame.payload {
        Payload::Error(payload) => {
            assert_eq!(payload.code, expected, "error payload: {:?}", payload);
            payload
        }
        other => panic!("expected error frame, got {other:?}"),
    }
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

async fn send_and_ack(ws: &mut WsClient, conversation_id: i64, body: &str) -> (Uuid, i64) {
    send_and_ack_replying_to(ws, conversation_id, body, None).await
}

/// Sends one `msg.send` (optionally replying to a message id) and waits for
/// its ack. Returns the server-assigned `(message_id, seq)`.
async fn send_and_ack_replying_to(
    ws: &mut WsClient,
    conversation_id: i64,
    body: &str,
    reply_to: Option<Uuid>,
) -> (Uuid, i64) {
    let client_msg_id = Uuid::now_v7();
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id,
            body: body.to_owned(),
            reply_to,
            media: None,
            forward_of_message_id: None,
        }),
    };
    ws_send_text(ws, &serde_json::to_string(&frame).expect("serialize")).await;
    match ws_next_frame(ws).await.payload {
        Payload::MsgAck(MsgAck {
            message_id, seq, ..
        }) => (message_id, seq),
        other => panic!("expected msg.ack, got {other:?}"),
    }
}

async fn send_read_update(ws: &mut WsClient, conversation_id: i64, last_read_seq: i64) {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::ReadUpdate(ReadUpdate {
            conversation_id,
            last_read_seq,
        }),
    };
    ws_send_text(ws, &serde_json::to_string(&frame).expect("serialize")).await;
}

async fn send_typing(ws: &mut WsClient, conversation_id: i64, state: TypingState) {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::Typing(Typing {
            conversation_id,
            state,
            user_id: None,
        }),
    };
    ws_send_text(ws, &serde_json::to_string(&frame).expect("serialize")).await;
}

async fn send_recall(ws: &mut WsClient, conversation_id: i64, message_id: Uuid) {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgRecall(MsgRecall {
            conversation_id,
            message_id,
        }),
    };
    ws_send_text(ws, &serde_json::to_string(&frame).expect("serialize")).await;
}

async fn send_sync_req(ws: &mut WsClient, cursors: Vec<(i64, i64)>) {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::SyncReq(SyncReq {
            cursors: cursors
                .into_iter()
                .map(|(conversation_id, last_delivered_seq)| SyncCursor {
                    conversation_id,
                    last_delivered_seq,
                })
                .collect(),
        }),
    };
    ws_send_text(ws, &serde_json::to_string(&frame).expect("serialize")).await;
}

async fn expect_sync_res(ws: &mut WsClient) -> jiuyue_protocol::SyncRes {
    match ws_next_frame(ws).await.payload {
        Payload::SyncRes(res) => res,
        other => panic!("expected sync.res, got {other:?}"),
    }
}

/// M3: sync entries became untagged `SyncMessage`s (plain `msg.new` or
/// encrypted `e2ee.msg`). These tests only exercise plaintext conversations,
/// so unwrap the plain variant.
fn as_plain(entry: &jiuyue_protocol::SyncMessage) -> &jiuyue_protocol::MsgNew {
    match entry {
        jiuyue_protocol::SyncMessage::Plain(msg) => msg,
        other => panic!("expected a plain msg.new sync entry, got {other:?}"),
    }
}

async fn member_last_read_seq(pool: &PgPool, conversation_id: i64, user_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT last_read_seq FROM conversation_members \
         WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("member row")
}

// ---------------------------------------------------------------------------
// Read receipts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_update_persists_cursor_and_notifies_only_the_peer() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "rr-ada@example.com", "rrada").await;
    let (b_id, b_access) = register_user(&t, "rr-ben@example.com", "rrben").await;
    let conversation_id = create_conversation(&t, &a_access, "rrben").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    // A sends two messages; B receives both live.
    let (_m1, seq1) = send_and_ack(&mut ws_a, conversation_id, "r-one").await;
    let (_m2, seq2) = send_and_ack(&mut ws_a, conversation_id, "r-two").await;
    assert_eq!((seq1, seq2), (1, 2));
    let _first = ws_next_frame(&mut ws_b).await;
    let _second = ws_next_frame(&mut ws_b).await;

    // B reads through seq 2 → A gets exactly one receipt naming B.
    send_read_update(&mut ws_b, conversation_id, 2).await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::ReadReceipt(ReadReceipt {
            conversation_id: conv,
            user_id,
            last_read_seq,
        }) => {
            assert_eq!(conv, conversation_id);
            assert_eq!(user_id, b_id);
            assert_eq!(last_read_seq, 2);
        }
        other => panic!("expected read.receipt on peer, got {other:?}"),
    }

    // The reader itself is never echoed its own receipt.
    let echo = ws_next_text(&mut ws_b, SILENCE_PROBE).await;
    assert!(
        echo.is_none(),
        "reader must not receive a receipt, got {echo:?}"
    );

    // Cursor persisted with GREATEST semantics: a stale lower update cannot
    // regress it (and still echoes the effective value).
    send_read_update(&mut ws_b, conversation_id, 1).await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::ReadReceipt(ReadReceipt { last_read_seq, .. }) => {
            assert_eq!(last_read_seq, 2, "receipt echoes the effective cursor");
        }
        other => panic!("expected second read.receipt, got {other:?}"),
    }
    let persisted = member_last_read_seq(&t.pool, conversation_id, b_id).await;
    assert_eq!(persisted, 2);
    let sender_cursor = member_last_read_seq(&t.pool, conversation_id, a_id).await;
    assert_eq!(sender_cursor, 0, "non-reader cursors stay untouched");
}

#[tokio::test]
async fn read_update_from_non_member_is_rejected_and_closes() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "rr-nina@example.com", "rrnina").await;
    let (_b_id, _b_access) = register_user(&t, "rr-omar@example.com", "rromar").await;
    let outsider = register_user(&t, "rr-eve@example.com", "rrevel").await;
    let conversation_id = create_conversation(&t, &a_access, "rromar").await;

    let mut ws_eve = ws_connect(&t, &outsider.1).await;
    send_read_update(&mut ws_eve, conversation_id, 5).await;
    expect_error_code(ws_next_frame(&mut ws_eve).await, ErrorCode::Unauthorized);
    match ws_next_text(&mut ws_eve, READ_TIMEOUT).await {
        None => {}
        Some(raw) => panic!("expected close after unauthorized read.update, got {raw:?}"),
    }
}

// ---------------------------------------------------------------------------
// Typing indicators
// ---------------------------------------------------------------------------

#[tokio::test]
async fn typing_is_relayed_to_the_peer_with_sender_stamped_and_never_echoed() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "ty-ada@example.com", "tyada").await;
    let (_b_id, b_access) = register_user(&t, "ty-ben@example.com", "tyben").await;
    let conversation_id = create_conversation(&t, &a_access, "tyben").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    send_typing(&mut ws_a, conversation_id, TypingState::Start).await;
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::Typing(Typing {
            conversation_id: conv,
            user_id,
            state,
        }) => {
            assert_eq!(conv, conversation_id);
            assert_eq!(user_id, Some(a_id), "relay stamps the authenticated sender");
            assert_eq!(state, TypingState::Start);
        }
        other => panic!("expected typing notice on peer, got {other:?}"),
    }

    send_typing(&mut ws_a, conversation_id, TypingState::Stop).await;
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::Typing(Typing { user_id, state, .. }) => {
            assert_eq!(user_id, Some(a_id));
            assert_eq!(state, TypingState::Stop);
        }
        other => panic!("expected typing stop notice, got {other:?}"),
    }

    // The typer never hears its own signal back.
    let echo = ws_next_text(&mut ws_a, SILENCE_PROBE).await;
    assert!(echo.is_none(), "typer must not be echoed, got {echo:?}");
}

#[tokio::test]
async fn typing_publishes_fire_and_forget_envelope_on_conversation_channel() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "ty-iris@example.com", "tyiris").await;
    let (_b_id, _b_access) = register_user(&t, "ty-juan@example.com", "tyjuan").await;
    let conversation_id = create_conversation(&t, &a_access, "tyjuan").await;

    // Real subscriber wired BEFORE the typing frame so the publish cannot
    // slip past unnoticed.
    let redis_client = redis::Client::open(env_var("REDIS_URL").as_str()).expect("parse redis url");
    let mut pubsub = redis_client
        .get_async_pubsub()
        .await
        .expect("pubsub connection");
    pubsub
        .subscribe(format!("conv:{conversation_id}"))
        .await
        .expect("subscribe conv channel");
    let mut pubsub_stream = pubsub.on_message();

    let mut ws_a = ws_connect(&t, &a_access).await;
    send_typing(&mut ws_a, conversation_id, TypingState::Start).await;

    let published = tokio::time::timeout(READ_TIMEOUT, pubsub_stream.next())
        .await
        .expect("publish must arrive well within the timeout")
        .expect("pubsub stream open");
    let payload: Value = serde_json::from_str(
        published
            .get_payload::<String>()
            .expect("string payload")
            .as_str(),
    )
    .expect("typing envelope is JSON");
    assert_eq!(payload["kind"], json!("typing"));
    assert_eq!(payload["user_id"], json!(a_id.to_string()));
    assert_eq!(payload["state"], json!("start"));
}

#[tokio::test]
async fn typing_from_non_member_is_rejected_and_closes() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "ty-kate@example.com", "tykate").await;
    let (_b_id, _b_access) = register_user(&t, "ty-liam@example.com", "tyliam").await;
    let outsider = register_user(&t, "ty-eve@example.com", "tyeve").await;
    let conversation_id = create_conversation(&t, &a_access, "tyliam").await;

    let mut ws_eve = ws_connect(&t, &outsider.1).await;
    send_typing(&mut ws_eve, conversation_id, TypingState::Start).await;
    expect_error_code(ws_next_frame(&mut ws_eve).await, ErrorCode::Unauthorized);
    match ws_next_text(&mut ws_eve, READ_TIMEOUT).await {
        None => {}
        Some(raw) => panic!("expected close after unauthorized typing, got {raw:?}"),
    }
}

// ---------------------------------------------------------------------------
// Recall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_within_window_tombstones_message_and_sync_serves_no_body() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "rc-ada@example.com", "rcada").await;
    let (_b_id, b_access) = register_user(&t, "rc-ben@example.com", "rcben").await;
    let conversation_id = create_conversation(&t, &a_access, "rcben").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let (message_id, _seq) = send_and_ack(&mut ws_a, conversation_id, "recall-me").await;
    let _delivery = ws_next_frame(&mut ws_b).await;

    // Sender recalls inside the window.
    send_recall(&mut ws_a, conversation_id, message_id).await;

    // Broadcast reaches EVERYONE including the requester (self-confirmation).
    for (who, ws) in [("sender", &mut ws_a), ("peer", &mut ws_b)] {
        match ws_next_frame(ws).await.payload {
            Payload::MsgRecalled(MsgRecalled {
                conversation_id: conv,
                message_id: got,
            }) => {
                assert_eq!(conv, conversation_id, "{who}");
                assert_eq!(got, message_id, "{who}");
            }
            other => panic!("expected msg.recalled on {who}, got {other:?}"),
        }
    }

    // Tombstone persisted...
    let recalled_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE id = $1 AND recalled_at IS NOT NULL",
    )
    .bind(message_id)
    .fetch_one(&t.pool)
    .await
    .expect("count recalled");
    assert_eq!(recalled_count, 1);

    // ...and sync serves the tombstone WITHOUT any body content.
    send_sync_req(&mut ws_b, vec![(conversation_id, 0)]).await;
    let res = expect_sync_res(&mut ws_b).await;
    assert_eq!(res.messages.len(), 1);
    let entry = as_plain(&res.messages[0]);
    assert_eq!(entry.message_id, message_id);
    assert!(entry.recalled, "sync entry must carry the tombstone flag");
    assert!(
        entry.body.is_empty(),
        "recalled bodies are never served again: {:?}",
        entry.body
    );

    // Second recall hits the terminal tombstone → conflict.
    send_recall(&mut ws_a, conversation_id, message_id).await;
    expect_error_code(ws_next_frame(&mut ws_a).await, ErrorCode::Conflict);
}

#[tokio::test]
async fn recall_outside_window_is_rejected_with_conflict() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "rc-cara@example.com", "rccara").await;
    let (_b_id, _b_access) = register_user(&t, "rc-dave@example.com", "rcdave").await;
    let conversation_id = create_conversation(&t, &a_access, "rcdave").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let (message_id, _) = send_and_ack(&mut ws_a, conversation_id, "too-late").await;

    // Age the message beyond the 120s window via direct SQL (clock injection
    // happens server-side through sent_at).
    sqlx::query("UPDATE messages SET sent_at = now() - interval '10 minutes' WHERE id = $1")
        .bind(message_id)
        .execute(&t.pool)
        .await
        .expect("age message");

    send_recall(&mut ws_a, conversation_id, message_id).await;
    expect_error_code(ws_next_frame(&mut ws_a).await, ErrorCode::Conflict);

    let recalled_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE id = $1 AND recalled_at IS NOT NULL",
    )
    .bind(message_id)
    .fetch_one(&t.pool)
    .await
    .expect("count recalled");
    assert_eq!(recalled_count, 0, "expired recall must not tombstone");
}

#[tokio::test]
async fn recall_by_non_sender_is_rejected_but_connection_survives() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "rc-erin@example.com", "rcerin").await;
    let (_b_id, b_access) = register_user(&t, "rc-finn@example.com", "rcfinn").await;
    let conversation_id = create_conversation(&t, &a_access, "rcfinn").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;
    let (message_id, _) = send_and_ack(&mut ws_a, conversation_id, "not-yours").await;
    let _delivery = ws_next_frame(&mut ws_b).await;

    // B (member, but not the sender) tries to recall A's message.
    send_recall(&mut ws_b, conversation_id, message_id).await;
    expect_error_code(ws_next_frame(&mut ws_b).await, ErrorCode::Unauthorized);

    // Action-level denial: the socket stays open and usable.
    send_sync_req(&mut ws_b, vec![(conversation_id, 0)]).await;
    let res = expect_sync_res(&mut ws_b).await;
    assert_eq!(res.messages.len(), 1);
    assert!(!as_plain(&res.messages[0]).recalled);

    let recalled_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE id = $1 AND recalled_at IS NOT NULL",
    )
    .bind(message_id)
    .fetch_one(&t.pool)
    .await
    .expect("count recalled");
    assert_eq!(recalled_count, 0);
}

#[tokio::test]
async fn recall_from_non_member_is_rejected_and_closes() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "rc-gina@example.com", "rcgina").await;
    let (_b_id, _b_access) = register_user(&t, "rc-hugo@example.com", "rchugo").await;
    let outsider = register_user(&t, "rc-eve@example.com", "rceveq").await;
    let conversation_id = create_conversation(&t, &a_access, "rchugo").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let (message_id, _) = send_and_ack(&mut ws_a, conversation_id, "secret").await;

    let mut ws_eve = ws_connect(&t, &outsider.1).await;
    send_recall(&mut ws_eve, conversation_id, message_id).await;
    expect_error_code(ws_next_frame(&mut ws_eve).await, ErrorCode::Unauthorized);
    match ws_next_text(&mut ws_eve, READ_TIMEOUT).await {
        None => {}
        Some(raw) => panic!("expected close after unauthorized recall, got {raw:?}"),
    }
}

// ---------------------------------------------------------------------------
// Reply quoting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reply_send_validates_target_and_msg_new_carries_quote_metadata() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "rp-ada@example.com", "rpada").await;
    let (_b_id, b_access) = register_user(&t, "rp-ben@example.com", "rpben").await;
    let conversation_id = create_conversation(&t, &a_access, "rpben").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    // Quoted body longer than the 80-char preview cap.
    let long_body = "x".repeat(100);
    let (quoted_id, _seq) = send_and_ack(&mut ws_a, conversation_id, &long_body).await;
    let _delivery = ws_next_frame(&mut ws_b).await;

    let (reply_id, reply_seq) = send_and_ack_replying_to(
        &mut ws_a,
        conversation_id,
        "here is my answer",
        Some(quoted_id),
    )
    .await;
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::MsgNew(MsgNew {
            message_id,
            seq,
            reply_to_message_id,
            reply_to_sender_id,
            reply_to_body_preview,
            ..
        }) => {
            assert_eq!(message_id, reply_id);
            assert_eq!(seq, reply_seq);
            assert_eq!(reply_to_message_id, Some(quoted_id));
            assert_eq!(reply_to_sender_id, Some(a_id));
            let preview = reply_to_body_preview.expect("preview present");
            assert_eq!(preview.chars().count(), 80, "preview capped at 80 chars");
        }
        other => panic!("expected msg.new with quote metadata, got {other:?}"),
    }

    // Reply metadata persisted.
    let stored: Option<Uuid> = sqlx::query_scalar("SELECT reply_to FROM messages WHERE id = $1")
        .bind(reply_id)
        .fetch_one(&t.pool)
        .await
        .expect("fetch reply row");
    assert_eq!(stored, Some(quoted_id));
}

#[tokio::test]
async fn reply_to_foreign_or_unknown_message_is_bad_request_without_persisting() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "rp-cara@example.com", "rpcara").await;
    let (_b_id, _b_access) = register_user(&t, "rp-dave@example.com", "rpdave").await;
    let (_c_id, _c_access) = register_user(&t, "rp-eve@example.com", "rpever").await;
    let conversation_id = create_conversation(&t, &a_access, "rpdave").await;
    // A second conversation A↔C provides a REAL but foreign reply target.
    let other_conversation = create_conversation(&t, &a_access, "rpever").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let (foreign_id, _) = send_and_ack(&mut ws_a, other_conversation, "foreign").await;

    let count_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
            .bind(conversation_id)
            .fetch_one(&t.pool)
            .await
            .expect("count before");

    for (why, target) in [
        ("unknown message id", Uuid::now_v7()),
        ("message from another conversation", foreign_id),
    ] {
        let frame = Frame {
            v: PROTOCOL_VERSION,
            payload: Payload::MsgSend(MsgSend {
                conversation_id,
                client_msg_id: Uuid::now_v7(),
                body: "bad reply".to_owned(),
                reply_to: Some(target),
                media: None,
                forward_of_message_id: None,
            }),
        };
        ws_send_text(
            &mut ws_a,
            &serde_json::to_string(&frame).expect("serialize"),
        )
        .await;
        let err = expect_error_code(ws_next_frame(&mut ws_a).await, ErrorCode::BadRequest);
        assert!(
            err.message.contains("reply_to"),
            "{why}: rejection must name the violated field: {}",
            err.message
        );
    }

    let count_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
            .bind(conversation_id)
            .fetch_one(&t.pool)
            .await
            .expect("count after");
    assert_eq!(
        count_before, count_after,
        "rejected replies must not persist rows"
    );

    // The connection stays usable and the conversation's seq was not burned
    // by the rejected sends (validation happens before allocation).
    send_sync_req(&mut ws_a, vec![(conversation_id, 0)]).await;
    let res = expect_sync_res(&mut ws_a).await;
    assert!(res.messages.is_empty());
}
