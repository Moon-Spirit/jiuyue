//! M3 integration tests: secret-chat server support against the live local
//! stack (`jiuyue_test` PG + Redis).
//!
//! Harness mirrors `chat_flow.rs` (global mutex + one-time schema reset +
//! per-test TRUNCATE + cross-binary PG advisory lock); the TRUNCATE list
//! additionally clears `e2ee_identities`.
//!
//! Covered flows:
//! 1. Key distribution: upload → claim-one → drain → `409 no_one_time_keys`.
//! 2. Secret conversation create-or-get on the sorted pair key.
//! 3. `e2ee.msg` full loop A→B: identical ciphertext on the wire AND
//!    verbatim ciphertext bytes at rest (`kind='e2ee'`), idempotent resend.
//! 4. Recall works on secret-conversation messages (tombstone broadcast).
//! 5. Reconnect sync replays stored rows as `SyncMessage` entries
//!    (plaintext `msg.new` + encrypted `e2ee.msg`) in seq order.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use jiuyue_protocol::{
    E2eeMsg, ErrorCode, Frame, MsgAck, MsgNew, MsgRecall, MsgRecalled, MsgSend, PROTOCOL_VERSION,
    Payload, SyncCursor, SyncMessage, SyncReq,
};
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
    /// Holds the advisory lock; dropping it releases the lock (session end).
    _lock_conn: PgConnection,
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env var {name}"))
}

async fn test_app() -> TestApp {
    dotenvy::dotenv().ok();

    // Surface server-side logs (incl. internal-error chains) in test output.
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
         conversations, conversation_members, messages, e2ee_identities RESTART IDENTITY CASCADE",
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

/// Registers a fresh user through the public HTTP flow; returns (id, access).
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

async fn mint_ticket(t: &TestApp, access: &str) -> String {
    let (status, body) = send_http(&t.app, "POST", "/api/auth/ws-ticket", Some(access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["ticket"].as_str().expect("ticket").to_owned()
}

async fn ws_connect(t: &TestApp, access: &str) -> WsClient {
    let ticket = mint_ticket(t, access).await;
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

/// Reads the next wire frame and decodes it into the protocol crate's Frame.
async fn ws_next_frame(ws: &mut WsClient) -> Frame {
    let msg = tokio::time::timeout(READ_TIMEOUT, ws.next())
        .await
        .expect("timed out waiting for websocket frame")
        .expect("stream must stay open")
        .expect("ws stream healthy");
    let text = msg.to_text().expect("text frame").to_owned();
    serde_json::from_str::<Frame>(&text)
        .expect("wire frame must decode into jiuyue_protocol::Frame")
}

/// Creates a conversation of the given kind over HTTP; returns the body.
async fn create_conversation(
    t: &TestApp,
    creator_access: &str,
    peer_username: &str,
    kind: Option<&str>,
) -> Value {
    let mut payload = json!({ "peer_username": peer_username });
    if let Some(kind) = kind {
        payload["kind"] = json!(kind);
    }
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(creator_access),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn key_upload_then_fetch_claims_exactly_one_key_until_exhaustion() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "m3-kia@example.com", "kia").await;
    let (_b_id, b_access) = register_user(&t, "m3-kib@example.com", "kib").await;

    // Unauthenticated uploads are rejected outright.
    let (status, _) = send_http(
        &t.app,
        "POST",
        "/api/e2ee/keys/upload",
        None,
        Some(json!({"identity_key": "ik", "one_time_keys": []})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Upload a bundle of two one-time keys.
    let (status, uploaded) = send_http(
        &t.app,
        "POST",
        "/api/e2ee/keys/upload",
        Some(&a_access),
        Some(json!({"identity_key": "ik-base64-a", "one_time_keys": ["otk-one", "otk-two"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{uploaded}");
    assert_eq!(uploaded["one_time_key_count"], json!(2));
    let device_id = uploaded["device_id"]
        .as_str()
        .expect("device_id")
        .to_owned();

    // Re-upload targeting the SAME device replaces the bundle (upsert row).
    let (status, re) = send_http(
        &t.app,
        "POST",
        &format!("/api/e2ee/keys/upload?device_id={device_id}"),
        Some(&a_access),
        Some(json!({"identity_key": "ik-base64-a2", "one_time_keys": ["otk-alpha", "otk-beta"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{re}");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM e2ee_identities WHERE user_id = $1")
        .bind(_a_id)
        .fetch_one(&t.pool)
        .await
        .expect("count bundles");
    assert_eq!(rows, 1, "upsert keyed by device keeps exactly one row");

    // A device_id owned by someone else is rejected.
    let (status, err) = send_http(
        &t.app,
        "POST",
        &format!("/api/e2ee/keys/upload?device_id={}", Uuid::now_v7()),
        Some(&b_access),
        Some(json!({"identity_key": "x", "one_time_keys": []})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");

    // Claiming pops exactly ONE key per fetch (the freshest stored first).
    let (status, bundle) =
        send_http(&t.app, "GET", "/api/e2ee/keys/kia", Some(&b_access), None).await;
    assert_eq!(status, StatusCode::OK, "{bundle}");
    assert_eq!(bundle["identity_key"], json!("ik-base64-a2"));
    assert_eq!(bundle["one_time_key"], json!("otk-beta"));
    assert!(bundle["user_id"].as_str().is_some());
    assert_eq!(bundle["device_id"], json!(device_id));

    let (status, bundle2) =
        send_http(&t.app, "GET", "/api/e2ee/keys/kia", Some(&b_access), None).await;
    assert_eq!(status, StatusCode::OK, "{bundle2}");
    assert_eq!(bundle2["one_time_key"], json!("otk-alpha"));

    // Pool drained → machine-readable conflict.
    let (status, err) = send_http(&t.app, "GET", "/api/e2ee/keys/kia", Some(&b_access), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
    assert_eq!(err["error"], json!("no_one_time_keys"));

    // Unknown username → plain 404.
    let (status, err) = send_http(
        &t.app,
        "GET",
        "/api/e2ee/keys/nobody",
        Some(&b_access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{err}");

    // Unauthenticated fetches are rejected.
    let (status, _) = send_http(&t.app, "GET", "/api/e2ee/keys/kia", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn secret_conversations_create_or_get_on_the_same_pair() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "m3-mia@example.com", "mia").await;
    let (_b_id, b_access) = register_user(&t, "m3-noah@example.com", "noah").await;

    let created = create_conversation(&t, &a_access, "noah", Some("secret")).await;
    let conversation_id = created["conversation_id"].as_i64().expect("numeric id");
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["kind"], json!("secret"));

    // The peer's create-or-get lands on the SAME secret row.
    let again = create_conversation(&t, &b_access, "mia", Some("secret")).await;
    assert_eq!(again["conversation_id"], json!(conversation_id));
    assert_eq!(again["created"], json!(false));
    assert_eq!(again["kind"], json!("secret"));

    // Invalid kinds are rejected with 400.
    let (status, err) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(&a_access),
        Some(json!({"peer_username": "noah", "kind": "group"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(err["error"], json!("bad_request"));

    // Direct and secret stay SEPARATE conversations for the same pair.
    let direct = create_conversation(&t, &a_access, "noah", None).await;
    assert_ne!(direct["conversation_id"], json!(conversation_id));
    assert_eq!(direct["kind"], json!("direct"));
}

#[tokio::test]
async fn e2ee_msg_loops_end_to_end_and_stores_verbatim_ciphertext() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "m3-sela@example.com", "sela").await;
    let (_b_id, b_access) = register_user(&t, "m3-kai@example.com", "kai").await;
    let conv = create_conversation(&t, &a_access, "kai", Some("secret")).await;
    let conversation_id = conv["conversation_id"].as_i64().expect("numeric id");

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let client_msg_id = Uuid::now_v7();
    let ciphertext = "b2xrLXByZS1rZXktY2lwaGVydGV4dA==";
    let send_frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::E2eeMsg(E2eeMsg {
            conversation_id,
            client_msg_id,
            ciphertext: ciphertext.to_owned(),
            message_type: 0,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&send_frame).expect("serialize"),
    )
    .await;

    // Sender gets the standard persist-then-ack ACK.
    let acked_message_id = match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(MsgAck {
            client_msg_id: got,
            message_id,
            seq,
            duplicate,
        }) => {
            assert_eq!(got, client_msg_id);
            assert_eq!(seq, 1, "first message in a fresh conversation gets seq 1");
            assert!(!duplicate);
            message_id
        }
        other => panic!("expected msg.ack, got {other:?}"),
    };

    // The peer receives the SAME e2ee.msg mirrored back, ciphertext equal.
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::E2eeMsg(E2eeMsg {
            conversation_id: got_conv,
            client_msg_id: got_cmid,
            ciphertext: got_ct,
            message_type: got_mt,
        }) => {
            assert_eq!(got_conv, conversation_id);
            assert_eq!(got_cmid, client_msg_id);
            assert_eq!(
                got_ct, ciphertext,
                "ciphertext must survive the relay verbatim"
            );
            assert_eq!(got_mt, 0);
        }
        other => panic!("expected mirrored e2ee.msg, got {other:?}"),
    }

    // Ciphertext-at-rest contract: body_enc holds the RAW STRING BYTES of
    // the ciphertext, tagged kind='e2ee' / key_id='e2ee:<message_type>'.
    let (body_enc, key_id, kind): (Vec<u8>, String, String) =
        sqlx::query_as("SELECT body_enc, key_id, kind FROM messages WHERE conversation_id = $1")
            .bind(conversation_id)
            .fetch_one(&t.pool)
            .await
            .expect("fetch stored message");
    assert_eq!(
        body_enc,
        ciphertext.as_bytes(),
        "stored bytes must equal the ciphertext string bytes verbatim"
    );
    assert_eq!(kind, "e2ee");
    assert_eq!(key_id, "e2ee:0");
    assert!(
        !contains_subslice(&body_enc, acked_message_id.as_bytes()),
        "no unrelated plaintext material leaks into the stored blob"
    );

    // Duplicate resend: identical ACK semantics, still exactly one row.
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&send_frame).expect("serialize"),
    )
    .await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(ack) => {
            assert!(ack.duplicate);
            assert_eq!(ack.message_id, acked_message_id);
            assert_eq!(ack.seq, 1);
        }
        other => panic!("expected duplicate msg.ack, got {other:?}"),
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
        .bind(conversation_id)
        .fetch_one(&t.pool)
        .await
        .expect("count messages");
    assert_eq!(rows, 1, "duplicate e2ee.msg must dedupe down to one row");
    let last_seq: i64 = sqlx::query_scalar("SELECT last_seq FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_one(&t.pool)
        .await
        .expect("read last_seq");
    assert_eq!(last_seq, 1, "duplicates must not advance the counter");
}

#[tokio::test]
async fn recall_works_on_secret_conversation_messages() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "m3-rosa@example.com", "rosa").await;
    let (_b_id, b_access) = register_user(&t, "m3-milo@example.com", "milo").await;
    let conv = create_conversation(&t, &a_access, "milo", Some("secret")).await;
    let conversation_id = conv["conversation_id"].as_i64().expect("numeric id");

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let send_frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::E2eeMsg(E2eeMsg {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            ciphertext: "cmVjYWxsYWJsZS1jaXBoZXJ0ZXh0".to_owned(),
            message_type: 1,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&send_frame).expect("serialize"),
    )
    .await;
    let message_id = match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(ack) => {
            assert!(!ack.duplicate);
            ack.message_id
        }
        other => panic!("expected msg.ack, got {other:?}"),
    };
    let _mirror = ws_next_frame(&mut ws_b).await; // B sees the live delivery

    // Sender recalls within the window; the tombstone broadcasts to ALL
    // members including the requester.
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
    for (who, ws) in [("sender", &mut ws_a), ("peer", &mut ws_b)] {
        match ws_next_frame(ws).await.payload {
            Payload::MsgRecalled(MsgRecalled {
                conversation_id: got_conv,
                message_id: got_msg,
            }) => {
                assert_eq!(got_conv, conversation_id, "{who}");
                assert_eq!(got_msg, message_id, "{who}");
            }
            other => panic!("expected msg.recalled for {who}, got {other:?}"),
        }
    }

    let recalled_at: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT recalled_at FROM messages WHERE id = $1")
            .bind(message_id)
            .fetch_one(&t.pool)
            .await
            .expect("fetch recalled_at");
    assert!(recalled_at.is_some(), "tombstone must be persisted");

    // Double recall is refused as a conflict (tombstone is terminal).
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&recall).expect("serialize"),
    )
    .await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::Error(payload) => assert_eq!(payload.code, ErrorCode::Conflict),
        other => panic!("expected conflict error on double recall, got {other:?}"),
    }
}

#[tokio::test]
async fn reconnect_sync_replays_stored_e2ee_frames_in_order() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "m3-nadia@example.com", "nadia").await;
    let (_b_id, b_access) = register_user(&t, "m3-omri@example.com", "omri").await;
    let conv = create_conversation(&t, &a_access, "omri", Some("secret")).await;
    let conversation_id = conv["conversation_id"].as_i64().expect("numeric id");

    // B never connects while A sends one plaintext + one e2ee message.
    let mut ws_a = ws_connect(&t, &a_access).await;
    let plain = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: "plain-hello".to_owned(),
            reply_to: None,
            media: None,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&plain).expect("serialize"),
    )
    .await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(ack) => assert_eq!(ack.seq, 1),
        other => panic!("expected msg.ack, got {other:?}"),
    }

    let sent_ciphertext = "c3luYy1yZXBsYXktY2lwaGVydGV4dA==";
    let encrypted = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::E2eeMsg(E2eeMsg {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            ciphertext: sent_ciphertext.to_owned(),
            message_type: 1,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&encrypted).expect("serialize"),
    )
    .await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(ack) => assert_eq!(ack.seq, 2),
        other => panic!("expected msg.ack, got {other:?}"),
    }

    // B reconnects and catches up from zero.
    let mut ws_b = ws_connect(&t, &b_access).await;
    let sync_req = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::SyncReq(SyncReq {
            cursors: vec![SyncCursor {
                conversation_id,
                last_delivered_seq: 0,
            }],
        }),
    };
    ws_send_text(
        &mut ws_b,
        &serde_json::to_string(&sync_req).expect("serialize"),
    )
    .await;

    match ws_next_frame(&mut ws_b).await.payload {
        Payload::SyncRes(res) => {
            assert!(res.complete, "two rows fit well inside one batch");
            assert_eq!(res.messages.len(), 2, "{res:?}");
            match (&res.messages[0], &res.messages[1]) {
                (
                    SyncMessage::Plain(MsgNew { seq, body, .. }),
                    SyncMessage::Encrypted(E2eeMsg {
                        ciphertext,
                        message_type,
                        ..
                    }),
                ) => {
                    assert_eq!(*seq, 1);
                    assert_eq!(body, "plain-hello");
                    assert_eq!(
                        ciphertext, sent_ciphertext,
                        "replayed ciphertext must be byte-identical"
                    );
                    assert_eq!(*message_type, 1);
                }
                mixed => panic!("unexpected sync entry mix: {mixed:?}"),
            }
        }
        other => panic!("expected sync.res, got {other:?}"),
    }
}
