//! Ticket 05 integration tests: direct-conversation creation + the full
//! WebSocket send path against the live local stack (`jiuyue_test` PG +
//! Redis).
//!
//! Harness mirrors `auth_flow.rs` (global mutex + one-time schema reset +
//! per-test TRUNCATE) and adds the same cross-binary PG advisory lock so the
//! two integration binaries never reset/race each other's schema.
//!
//! WS clients are real `tokio-tungstenite` connections against a real
//! `axum::serve` listener bound to an ephemeral `127.0.0.1:0` port; every
//! received wire frame is deserialized into `jiuyue_protocol::Frame` so the
//! assertions pin the actual protocol shapes.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use jiuyue_protocol::{ErrorCode, Frame, MsgAck, MsgNew, MsgSend, Payload, PROTOCOL_VERSION};
use jiuyue_server::crypto::BodyCipher;
use jiuyue_server::state::AppState;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Error as WsError;
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

fn ws_url(t: &TestApp, ticket: &str) -> String {
    format!("ws://127.0.0.1:{}/ws?ticket={ticket}&platform=web", t.port)
}

async fn ws_connect(t: &TestApp, access: &str) -> WsClient {
    let ticket = mint_ticket(t, access).await;
    let (ws, _response) = tokio_tungstenite::connect_async(ws_url(t, &ticket))
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

async fn ws_next_raw(ws: &mut WsClient) -> Option<WsMessage> {
    match tokio::time::timeout(READ_TIMEOUT, ws.next()).await {
        Ok(item) => item.map(|result| result.expect("ws stream healthy")),
        Err(_) => panic!("timed out waiting for websocket frame"),
    }
}

/// Reads the next wire frame and decodes it into the protocol crate's Frame.
async fn ws_next_frame(ws: &mut WsClient) -> Frame {
    let msg = ws_next_raw(ws).await.expect("stream must stay open");
    let text = msg.to_text().expect("text frame").to_owned();
    serde_json::from_str::<Frame>(&text).expect("wire frame must decode into jiuyue_protocol::Frame")
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

/// Creates the direct conversation `creator -> peer_username` over HTTP.
async fn create_conversation(t: &TestApp, creator_access: &str, peer_username: &str) -> Value {
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(creator_access),
        Some(json!({ "peer_username": peer_username })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_direct_message_delivers_ack_and_live_msg_new() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "ada@example.com", "ada").await;
    let (b_id, b_access) = register_user(&t, "ben@example.com", "ben").await;

    // A creates the direct conversation with B.
    let conv = create_conversation(&t, &a_access, "ben").await;
    let conversation_id = conv["conversation_id"].as_i64().expect("numeric id");
    assert!(conversation_id > 0);
    assert_eq!(conv["created"], json!(true));
    assert_eq!(conv["peer"]["user_id"], json!(b_id.to_string()));
    assert_eq!(conv["peer"]["username"], json!("ben"));

    // B's create-or-get lands on the same row, flagged created=false.
    let conv_again = create_conversation(&t, &b_access, "ada").await;
    assert_eq!(conv_again["conversation_id"], json!(conversation_id));
    assert_eq!(conv_again["created"], json!(false));
    assert_eq!(conv_again["peer"]["user_id"], json!(a_id.to_string()));

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    // A sends; ACK must carry server identity + seq (persist-then-ack).
    let client_msg_id = Uuid::now_v7();
    let send_frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id,
            body: "\u{5728}\u{5417}\u{ff1f}".to_owned(),
            reply_to: None,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&send_frame).expect("serialize"),
    )
    .await;

    let ack = ws_next_frame(&mut ws_a).await;
    let acked_message_id = match ack.payload {
        Payload::MsgAck(MsgAck {
            client_msg_id: got_cmid,
            message_id,
            seq,
            duplicate,
        }) => {
            assert_eq!(got_cmid, client_msg_id);
            assert_eq!(
                seq, 1,
                "first message in a fresh conversation gets seq 1"
            );
            assert!(!duplicate);
            message_id
        }
        other => panic!("expected msg.ack, got {other:?}"),
    };

    // B receives the live fanout with identical identity.
    let delivered = ws_next_frame(&mut ws_b).await;
    match delivered.payload {
        Payload::MsgNew(MsgNew {
            message_id,
            conversation_id: conv,
            seq,
            sender_id,
            body,
            sent_at,
            ..
        }) => {
            assert_eq!(message_id, acked_message_id);
            assert_eq!(conv, conversation_id);
            assert_eq!(seq, 1);
            assert_eq!(sender_id, a_id);
            assert_eq!(body, "\u{5728}\u{5417}\u{ff1f}");
            OffsetDateTime::parse(&sent_at, &Rfc3339).expect("sent_at is RFC 3339");
        }
        other => panic!("expected msg.new, got {other:?}"),
    }

    // A second distinct message continues the monotonic sequence.
    let second = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: "second".to_owned(),
            reply_to: None,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&second).expect("serialize"),
    )
    .await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(ack) => {
            assert_eq!(ack.seq, 2, "seq strictly monotonic per conversation");
            assert!(!ack.duplicate);
        }
        other => panic!("expected second msg.ack, got {other:?}"),
    }
    let _ = ws_next_frame(&mut ws_b).await; // B sees the second delivery too
}

#[tokio::test]
async fn duplicate_client_msg_id_is_idempotent_down_to_one_db_row() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "ida@example.com", "ida").await;
    let (_b_id, _b_access) = register_user(&t, "ivan@example.com", "ivan").await;
    let conv = create_conversation(&t, &a_access, "ivan").await;
    let conversation_id = conv["conversation_id"].as_i64().expect("numeric id");

    let mut ws_a = ws_connect(&t, &a_access).await;

    const ATTEMPTS: usize = 3;
    let client_msg_id = Uuid::now_v7();
    let mut first_identity: Option<(Uuid, i64)> = None;
    for attempt in 0..ATTEMPTS {
        let frame = Frame {
            v: PROTOCOL_VERSION,
            payload: Payload::MsgSend(MsgSend {
                conversation_id,
                client_msg_id,
                body: "exactly once".to_owned(),
                reply_to: None,
            }),
        };
        ws_send_text(
            &mut ws_a,
            &serde_json::to_string(&frame).expect("serialize"),
        )
        .await;
        match ws_next_frame(&mut ws_a).await.payload {
            Payload::MsgAck(MsgAck {
                message_id,
                seq,
                duplicate,
                ..
            }) => {
                let identity = (message_id, seq);
                match first_identity {
                    None => {
                        assert!(!duplicate, "first attempt is fresh");
                        first_identity = Some(identity);
                    }
                    Some(original) => {
                        assert_eq!(
                            identity, original,
                            "every retry must echo the original id/seq"
                        );
                        assert!(duplicate, "attempt {attempt} must be flagged duplicate");
                    }
                }
            }
            other => panic!("expected msg.ack on attempt {attempt}, got {other:?}"),
        }
    }

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
        .bind(conversation_id)
        .fetch_one(&t.pool)
        .await
        .expect("count messages");
    assert_eq!(rows, 1, "N sends with one client_msg_id must store one row");

    // The duplicate path rolls back its speculative seq bump: no phantom gaps.
    let last_seq: i64 = sqlx::query_scalar("SELECT last_seq FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_one(&t.pool)
        .await
        .expect("read last_seq");
    assert_eq!(last_seq, 1, "duplicates must not advance the counter");
}

#[tokio::test]
async fn body_is_ciphertext_at_rest_and_decrypts_back_to_plaintext() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "lena@example.com", "lena").await;
    let (_b_id, _b_access) = register_user(&t, "oscar@example.com", "oscar").await;
    let conv = create_conversation(&t, &a_access, "oscar").await;
    let conversation_id = conv["conversation_id"].as_i64().expect("numeric id");

    let mut ws_a = ws_connect(&t, &a_access).await;
    let plaintext = "top-secret-\u{4f60}\u{597d}-plaintext";
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: plaintext.to_owned(),
            reply_to: None,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&frame).expect("serialize"),
    )
    .await;
    let _ack = ws_next_frame(&mut ws_a).await;

    let (body_enc, key_id): (Vec<u8>, String) =
        sqlx::query_as("SELECT body_enc, key_id FROM messages WHERE conversation_id = $1")
            .bind(conversation_id)
            .fetch_one(&t.pool)
            .await
            .expect("fetch stored message");
    assert_eq!(key_id, "v1", "at-rest scheme tag");
    assert_ne!(
        body_enc, plaintext.as_bytes(),
        "DB column must be ciphertext"
    );
    assert!(
        !contains_subslice(&body_enc, plaintext.as_bytes()),
        "plaintext bytes must not appear anywhere in the stored blob"
    );

    // Reading side decrypts back with the documented derivation.
    let cipher = BodyCipher::new(&env_var("JIUYUE_MASTER_KEY"));
    let decrypted = cipher.decrypt(&body_enc).expect("decrypt stored body");
    assert_eq!(decrypted, plaintext);
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[tokio::test]
async fn non_member_cannot_send_and_connection_is_closed() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "nina@example.com", "nina").await;
    let (_b_id, _b_access) = register_user(&t, "omar@example.com", "omar").await;
    let outsider = register_user(&t, "eve@example.com", "eve").await;
    let conv = create_conversation(&t, &a_access, "omar").await;
    let conversation_id = conv["conversation_id"].as_i64().expect("numeric id");

    let mut ws_eve = ws_connect(&t, &outsider.1).await;
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: "let me in".to_owned(),
            reply_to: None,
        }),
    };
    ws_send_text(
        &mut ws_eve,
        &serde_json::to_string(&frame).expect("serialize"),
    )
    .await;

    expect_error_code(ws_next_frame(&mut ws_eve).await, ErrorCode::Unauthorized);
    // Server closes right after the authorization error.
    match ws_next_raw(&mut ws_eve).await {
        None => {}
        Some(WsMessage::Close(_)) => {}
        other => panic!("expected close after unauthorized send, got {other:?}"),
    }

    // Nothing was persisted for the rejected sender.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
        .bind(conversation_id)
        .fetch_one(&t.pool)
        .await
        .expect("count messages");
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn self_chat_is_rejected_with_400() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "pia@example.com", "pia").await;
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(&a_access),
        Some(json!({ "peer_username": "pia" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!("bad_request"));
}

#[tokio::test]
async fn unknown_peer_returns_404_peer_not_found() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "quinn@example.com", "quinn").await;
    let (status, body) = send_http(
        &t.app,
        "POST",
        "/api/conversations",
        Some(&a_access),
        Some(json!({ "peer_username": "ghost_user" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], json!("peer_not_found"));
}

#[tokio::test]
async fn one_ticket_across_two_simultaneous_connections_rejects_the_second() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "rita@example.com", "rita").await;
    let ticket = mint_ticket(&t, &a_access).await;
    let url = ws_url(&t, &ticket);

    let (first, second) = tokio::join!(
        tokio_tungstenite::connect_async(url.as_str()),
        tokio_tungstenite::connect_async(url.as_str()),
    );

    let mut upgrades = 0;
    for result in [first, second] {
        match result {
            Ok((_ws, _resp)) => upgrades += 1,
            Err(WsError::Http(response)) => {
                assert_eq!(
                    response.status().as_u16(),
                    401,
                    "loser must be rejected during the HTTP upgrade"
                );
            }
            Err(err) => panic!("unexpected failure kind: {err}"),
        }
    }
    assert_eq!(upgrades, 1, "exactly one connection may consume the ticket");
}

#[tokio::test]
async fn unsupported_version_frame_gets_bad_request_then_close() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "sara@example.com", "sara").await;
    let mut ws_a = ws_connect(&t, &a_access).await;

    ws_send_text(&mut ws_a, r#"{"v":2,"t":"msg.send","d":{}}"#).await;
    expect_error_code(ws_next_frame(&mut ws_a).await, ErrorCode::BadRequest);
    match ws_next_raw(&mut ws_a).await {
        None => {}
        Some(WsMessage::Close(_)) => {}
        other => panic!("connection must close after version mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn unauthenticated_ws_upgrades_are_rejected_with_401() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    for url in [
        format!(
            "ws://127.0.0.1:{}/ws?ticket=bogus-ticket&platform=web",
            t.port
        ),
        format!("ws://127.0.0.1:{}/ws?platform=web", t.port),
    ] {
        let result = tokio_tungstenite::connect_async(url.as_str()).await;
        match result {
            Err(WsError::Http(response)) => {
                assert_eq!(response.status().as_u16(), 401, "url: {url}");
            }
            Ok(_) => panic!("unauthenticated upgrade must fail: {url}"),
            Err(err) => panic!("unexpected failure kind for {url}: {err}"),
        }
    }
}

#[tokio::test]
async fn unknown_frame_type_is_answered_without_closing() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "tara@example.com", "tara").await;
    let (_b_id, _b_access) = register_user(&t, "victor@example.com", "victor").await;
    let conv = create_conversation(&t, &a_access, "victor").await;
    let conversation_id = conv["conversation_id"].as_i64().expect("numeric id");

    let mut ws_a = ws_connect(&t, &a_access).await;

    // Unknown type: protocol crate maps it into Error/UnknownType; the server
    // answers and keeps the connection usable.
    ws_send_text(
        &mut ws_a,
        r#"{"v":1,"t":"holo.render","d":{"scene":"lobby"}}"#,
    )
    .await;
    expect_error_code(ws_next_frame(&mut ws_a).await, ErrorCode::UnknownType);

    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: "still here".to_owned(),
            reply_to: None,
        }),
    };
    ws_send_text(
        &mut ws_a,
        &serde_json::to_string(&frame).expect("serialize"),
    )
    .await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(ack) => assert!(!ack.duplicate),
        other => panic!("connection must survive unknown frames, got {other:?}"),
    }
}

#[tokio::test]
async fn conversation_list_returns_memberships_with_peer_info() {
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "list-a@example.com", "lista").await;
    let (_b_id, b_access) = register_user(&t, "list-b@example.com", "listb").await;
    let conv = create_conversation(&t, &a_access, "listb").await;

    // Creator sees the conversation with peer info pointing at listb.
    let (status, body) = send_http(&t.app, "GET", "/api/conversations", Some(&a_access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("array body");
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["conversation_id"], conv["conversation_id"]);
    assert_eq!(items[0]["kind"], "direct");
    assert_eq!(items[0]["peer"]["username"], "listb");
    assert_eq!(items[0]["last_seq"], 0);
    assert_eq!(items[0]["last_delivered_seq"], 0);

    // The peer sees the same conversation with the creator as peer.
    let (status, body) = send_http(&t.app, "GET", "/api/conversations", Some(&b_access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("array body");
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["peer"]["username"], "lista");

    // A user without conversations gets an empty array, not an error.
    let (_c_id, c_access) = register_user(&t, "list-c@example.com", "listc").await;
    let (status, body) = send_http(&t.app, "GET", "/api/conversations", Some(&c_access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(0), "{body}");

    // Unauthenticated requests are rejected.
    let (status, _) = send_http(&t.app, "GET", "/api/conversations", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
