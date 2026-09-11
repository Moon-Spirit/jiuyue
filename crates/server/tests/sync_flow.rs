//! Ticket 06 integration tests: delivery reliability — heartbeat liveness,
//! per-device/per-conversation delivery cursors, offline gap-fill via
//! `sync.req`/`sync.res`, multi-device self-echo, backpressure under a
//! saturated recipient channel, and the Redis `conv:{id}` publish hook.
//!
//! Harness mirrors `chat_flow.rs` (global mutex + one-time schema reset +
//! per-test TRUNCATE + cross-binary PG advisory lock + ephemeral-port WS).
//! Two additions:
//!
//! * the test keeps a handle to the [`AppState`] so tests can introspect the
//!   [`ConnRegistry`] directly (heartbeat-eviction assertions) and shrink
//!   the heartbeat intervals via the public `state.heartbeat` field instead
//!   of process-global env mutation;
//! * frame readers skip server-initiated Ping/Pong control frames so tests
//!   stay correct even with sub-second heartbeat intervals.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use jiuyue_protocol::{
    Frame, MsgAck, MsgNew, MsgSend, PROTOCOL_VERSION, Payload, SyncCursor, SyncReq,
};
use jiuyue_server::state::AppState;
use jiuyue_server::ws::HeartbeatConfig;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use std::future::Future;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
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
/// Generous ceiling for fire-and-forget side effects (cursor writes,
/// registry eviction) that tests poll for.
const EVENTUALLY_TIMEOUT: Duration = Duration::from_secs(15);

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestApp {
    app: Router,
    pool: PgPool,
    /// Live state handle: same instance the served router uses (cloned), so
    /// registry/cipher introspection sees real connection state.
    state: AppState,
    port: u16,
    /// Holds the advisory lock; dropping it releases the lock (session end).
    _lock_conn: PgConnection,
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing required env var {name}"))
}

async fn test_app() -> TestApp {
    test_app_with_heartbeat(HeartbeatConfig::default()).await
}

async fn test_app_with_heartbeat(heartbeat: HeartbeatConfig) -> TestApp {
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
    let mut state = AppState::new(pool.clone(), redis.clone(), env_var("JIUYUE_JWT_SECRET"));
    // Shrinkable without env mutation: each test decides its own heartbeat.
    state.heartbeat = heartbeat;

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
        state,
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

/// Next TEXT frame as raw JSON string, skipping Ping/Pong/Binary heartbeat
/// noise; `None` on timeout or close.
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
                // Ping/Pong/Binary: transport noise, keep waiting.
            }
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_elapsed) => return None,
        }
    }
}

/// Reads the next wire frame and decodes it into the protocol crate's Frame.
async fn ws_next_frame(ws: &mut WsClient) -> Frame {
    let text = ws_next_text(ws, READ_TIMEOUT)
        .await
        .expect("stream must deliver a text frame");
    serde_json::from_str::<Frame>(&text)
        .expect("wire frame must decode into jiuyue_protocol::Frame")
}

/// Creates the direct conversation `creator -> peer_username` over HTTP.
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

/// Sends one `msg.send` and waits for its (non-duplicate) ack.
/// Returns the server-assigned `(message_id, seq)`.
async fn send_and_ack(ws: &mut WsClient, conversation_id: i64, body: &str) -> (Uuid, i64) {
    let client_msg_id = Uuid::now_v7();
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id,
            body: body.to_owned(),
            reply_to: None,
            media: None,
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

/// Sends one `sync.req` carrying `cursors`.
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

/// Decodes the next frame as `sync.res` (any other payload is a failure).
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

/// Polls `cond` until true or `timeout` elapses (for fire-and-forget side
/// effects: spawned cursor updates, eviction teardown, pub/sub delivery).
async fn eventually<F, Fut>(mut cond: F, timeout: Duration, what: &str)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if cond().await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition not met within {timeout:?}: {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Reads `(conversation_id, user_id)`'s persisted delivery cursor.
async fn member_cursor(pool: &PgPool, conversation_id: i64, user_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT last_delivered_seq FROM conversation_members \
         WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("member cursor row")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Acceptance 1 — B drops TCP mid-conversation (no close handshake), A keeps
/// sending, B reconnects with a fresh ticket and replays exactly the gap via
/// sync.req: missing seqs in order, nothing more, complete=true.
#[tokio::test]
async fn disconnect_mid_send_gap_is_filled_exactly_by_sync_req() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "gabriel@example.com", "gabriel").await;
    let (b_id, b_access) = register_user(&t, "holly@example.com", "holly").await;
    let conversation_id = create_conversation(&t, &a_access, "holly").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b1 = ws_connect(&t, &b_access).await;

    // Baseline: B is online and receives seq 1 live.
    let (m1_id, m1_seq) = send_and_ack(&mut ws_a, conversation_id, "m1").await;
    assert_eq!(m1_seq, 1);
    match ws_next_frame(&mut ws_b1).await.payload {
        Payload::MsgNew(MsgNew {
            message_id, seq, ..
        }) => {
            assert_eq!((message_id, seq), (m1_id, 1));
        }
        other => panic!("expected baseline msg.new, got {other:?}"),
    }

    // Abrupt drop: no WebSocket close handshake, just the socket going away.
    drop(ws_b1);
    eventually(
        || {
            let count = t.state.registry.user_device_count(b_id);
            async move { count == 0 }
        },
        EVENTUALLY_TIMEOUT,
        "server must notice B's abrupt disconnect and unregister it",
    )
    .await;

    // A keeps sending into the void: three messages land in the DB.
    let mut sent = Vec::new();
    for n in 2..=4 {
        let body = format!("m{n}");
        let (message_id, seq) = send_and_ack(&mut ws_a, conversation_id, &body).await;
        assert_eq!(seq, n, "seq must stay monotonic while B is offline");
        sent.push((message_id, seq, body));
    }

    // B reconnects on a brand-new ticket + connection and catches up from
    // its last delivered cursor (seq 1).
    let mut ws_b2 = ws_connect(&t, &b_access).await;
    send_sync_req(&mut ws_b2, vec![(conversation_id, 1)]).await;
    let res = expect_sync_res(&mut ws_b2).await;

    assert!(
        res.complete,
        "three missing messages are below the batch cap: {:?}",
        res.messages
    );
    let got: Vec<(i64, String)> = res
        .messages
        .iter()
        .map(as_plain)
        .map(|m| (m.seq, m.body.clone()))
        .collect();
    let want: Vec<(i64, String)> = sent
        .iter()
        .map(|(_, seq, body)| (*seq, body.clone()))
        .collect();
    assert_eq!(got, want, "exactly the missing seqs, in order");
    for entry in &res.messages {
        let m = as_plain(entry);
        assert_eq!(m.conversation_id, conversation_id);
        assert_eq!(m.sender_id, a_id);
    }
    assert_eq!(
        res.messages
            .iter()
            .map(as_plain)
            .map(|m| m.message_id)
            .collect::<Vec<_>>(),
        sent.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
        "sync replay preserves server message identity"
    );
}

/// Acceptance 2 — multi-device self-echo: A sends from device1; A's device2
/// receives the msg.new, the sending device does NOT (it got the ack), and
/// the peer still gets its copy.
#[tokio::test]
async fn self_echo_reaches_senders_other_devices_but_not_the_sending_device() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "irene@example.com", "irene").await;
    let (_b_id, b_access) = register_user(&t, "karl@example.com", "karl").await;
    let conversation_id = create_conversation(&t, &a_access, "karl").await;

    // Two simultaneous devices for A, one for B.
    let mut ws_a1 = ws_connect(&t, &a_access).await;
    let mut ws_a2 = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;
    eventually(
        || {
            let count = t.state.registry.user_device_count(a_id);
            async move { count == 2 }
        },
        EVENTUALLY_TIMEOUT,
        "A must have two registered devices",
    )
    .await;

    let (message_id, seq) = send_and_ack(&mut ws_a1, conversation_id, "echo-me").await;
    assert_eq!(seq, 1);

    // Self-echo lands on device2...
    match ws_next_frame(&mut ws_a2).await.payload {
        Payload::MsgNew(MsgNew {
            message_id: got_id,
            conversation_id: conv,
            seq: got_seq,
            sender_id,
            body,
            ..
        }) => {
            assert_eq!(got_id, message_id);
            assert_eq!(conv, conversation_id);
            assert_eq!(got_seq, 1);
            assert_eq!(sender_id, a_id);
            assert_eq!(body, "echo-me");
        }
        other => panic!("expected self-echo msg.new on device2, got {other:?}"),
    }
    // ...and the peer gets its normal copy.
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::MsgNew(MsgNew {
            message_id: got_id, ..
        }) => assert_eq!(got_id, message_id),
        other => panic!("expected msg.new on peer, got {other:?}"),
    }

    // The SENDING device must not hear its own message back (it already got
    // the authoritative ack); pings are filtered by the reader helper.
    let stray = ws_next_text(&mut ws_a1, Duration::from_millis(800)).await;
    assert!(
        stray.is_none(),
        "sending device must stay silent, got {stray:?}"
    );
}

/// Acceptance 3 — successful live delivery advances the persisted
/// `last_delivered_seq`; a follow-up sync.req at that cursor returns empty +
/// complete=true. The sender's own cursor stays put (its only device is the
/// sending one, so nothing was delivered to it).
#[tokio::test]
async fn delivered_frames_advance_persisted_cursor_and_later_sync_is_empty() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (a_id, a_access) = register_user(&t, "lina@example.com", "lina").await;
    let (b_id, b_access) = register_user(&t, "milan@example.com", "milan").await;
    let conversation_id = create_conversation(&t, &a_access, "milan").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let (id1, seq1) = send_and_ack(&mut ws_a, conversation_id, "c1").await;
    let (_id2, seq2) = send_and_ack(&mut ws_a, conversation_id, "c2").await;
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::MsgNew(MsgNew {
            message_id, seq, ..
        }) => {
            assert_eq!((message_id, seq), (id1, seq1));
        }
        other => panic!("expected live msg.new, got {other:?}"),
    }
    // Drain the second delivery too (order-preserving channel).
    let _second = ws_next_frame(&mut ws_b).await;
    assert_eq!(seq2, 2);

    // Fire-and-forget cursor write must land shortly after delivery.
    eventually(
        || {
            let pool = t.pool.clone();
            async move { member_cursor(&pool, conversation_id, b_id).await == 2 }
        },
        EVENTUALLY_TIMEOUT,
        "B's last_delivered_seq must advance to 2 after live delivery",
    )
    .await;
    assert_eq!(
        member_cursor(&t.pool, conversation_id, a_id).await,
        0,
        "sender's own cursor must not advance when its only device is the sending one"
    );

    // Catch-up at the advanced cursor: nothing left to replay.
    send_sync_req(&mut ws_b, vec![(conversation_id, 2)]).await;
    let res = expect_sync_res(&mut ws_b).await;
    assert!(res.messages.is_empty(), "{:?}", res.messages);
    assert!(res.complete);
}

/// Acceptance 4 — a connection that stops answering pings is evicted from
/// the registry within ping+timeout (+margin), and its devices.last_seen_at
/// is refreshed best-effort. Uses shrunk intervals via the state field.
#[tokio::test]
async fn silent_connection_is_evicted_after_heartbeat_timeout() {
    let _guard = GATE.lock().await;
    let t = test_app_with_heartbeat(HeartbeatConfig {
        ping_every: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(2),
    })
    .await;

    let (user_id, access) = register_user(&t, "nadia@example.com", "nadia").await;
    // Connected but about to go mute: once we stop polling this stream, the
    // tungstenite client can no longer auto-pong, simulating a dead peer —
    // while keeping the TCP socket open (so the eviction can ONLY come from
    // the heartbeat timer, not from a transport close).
    let silent_ws = ws_connect(&t, &access).await;
    eventually(
        || {
            let count = t.state.registry.user_device_count(user_id);
            async move { count == 1 }
        },
        EVENTUALLY_TIMEOUT,
        "device registered before going silent",
    )
    .await;
    let seen_at_connect: OffsetDateTime =
        sqlx::query_scalar("SELECT last_seen_at FROM devices WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&t.pool)
            .await
            .expect("device row");

    // Go silent: never touch `silent_ws` again until eviction is observed.
    // ping=1s + timeout=2s ⇒ eviction ~3s after connect; 20s is generous.
    eventually(
        || {
            let count = t.state.registry.user_device_count(user_id);
            async move { count == 0 }
        },
        Duration::from_secs(20),
        "silent connection must be evicted from the registry",
    )
    .await;

    // Best-effort liveness bookkeeping accompanies the eviction.
    eventually(
        || {
            let pool = t.pool.clone();
            async move {
                let seen: OffsetDateTime =
                    sqlx::query_scalar("SELECT last_seen_at FROM devices WHERE user_id = $1")
                        .bind(user_id)
                        .fetch_one(&pool)
                        .await
                        .expect("device row");
                seen > seen_at_connect
            }
        },
        EVENTUALLY_TIMEOUT,
        "devices.last_seen_at must be touched on heartbeat eviction",
    )
    .await;

    drop(silent_ws);
}

/// Acceptance 5 — backpressure: a connected-but-never-reading recipient has
/// its bounded channel saturate; the sender's pipeline must neither block
/// nor crash, and every one of its acks still arrives (drop-lag on the slow
/// consumer, DB cursor as recovery path).
#[tokio::test]
async fn saturated_recipient_channel_does_not_block_or_break_the_sender() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "oskar@example.com", "oskar").await;
    let (_b_id, b_access) = register_user(&t, "petra@example.com", "petra").await;
    let conversation_id = create_conversation(&t, &a_access, "petra").await;

    let ws_a = ws_connect(&t, &a_access).await;
    let ws_b = ws_connect(&t, &b_access).await; // never read from here on

    const TOTAL: usize = 600; // > OUTBOUND_CHANNEL_CAPACITY (512)

    // Split A's socket: a background task drains acks continuously so A's
    // own outbound queue never fills, while the main task pipelines sends.
    let (mut sink, stream) = ws_a.split();
    let ack_reader = tokio::spawn(async move {
        let mut stream = stream;
        let mut acks: Vec<MsgAck> = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
                Ok(Some(Ok(msg))) => {
                    if msg.is_text() {
                        let text = msg.into_text().expect("text payload");
                        if let Ok(frame) = serde_json::from_str::<Frame>(text.as_str())
                            && let Payload::MsgAck(ack) = frame.payload
                        {
                            acks.push(ack);
                            if acks.len() == TOTAL {
                                return acks;
                            }
                        }
                    } else if msg.is_close() {
                        return acks;
                    }
                }
                Ok(Some(Err(_))) | Ok(None) | Err(_) => return acks,
            }
        }
    });

    for n in 1..=TOTAL {
        let frame = Frame {
            v: PROTOCOL_VERSION,
            payload: Payload::MsgSend(MsgSend {
                conversation_id,
                client_msg_id: Uuid::now_v7(),
                body: format!("burst-{n}"),
                reply_to: None,
                media: None,
            }),
        };
        tokio::time::timeout(
            READ_TIMEOUT,
            sink.send(WsMessage::Text(
                serde_json::to_string(&frame).expect("serialize").into(),
            )),
        )
        .await
        .expect("send timeout")
        .expect("send ok");
    }

    let acks = tokio::time::timeout(EVENTUALLY_TIMEOUT * 4, ack_reader)
        .await
        .expect("ack reader must finish long before this ceiling")
        .expect("ack reader task");
    assert_eq!(
        acks.len(),
        TOTAL,
        "every send must be acked despite the saturated recipient"
    );
    for (n, ack) in acks.iter().enumerate() {
        assert_eq!(ack.seq, n as i64 + 1, "acks arrive in send order");
        assert!(!ack.duplicate);
    }

    // Nothing lost server-side: the DB is the source of truth.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
        .bind(conversation_id)
        .fetch_one(&t.pool)
        .await
        .expect("count messages");
    assert_eq!(rows, TOTAL as i64);
    let last_seq: i64 = sqlx::query_scalar("SELECT last_seq FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_one(&t.pool)
        .await
        .expect("read last_seq");
    assert_eq!(last_seq, TOTAL as i64);

    // The slow consumer kept whatever fit in its queue (first frames first);
    // dropped overflow is legal and recoverable via sync — never asserted.
    let mut ws_b = ws_b;
    let first_slow_frame = ws_next_text(&mut ws_b, READ_TIMEOUT).await;
    assert!(
        first_slow_frame.is_some(),
        "queued frames must still flush to the slow consumer"
    );
}

/// Acceptance 6 — the Redis notify hook publishes `conv:{id}` once per
/// message with the message id as pointer payload (real pubsub subscriber).
#[tokio::test]
async fn redis_publish_fires_per_message_on_conversation_channel() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "quentin@example.com", "quentin").await;
    let (_b_id, _b_access) = register_user(&t, "rosa@example.com", "rosa").await;
    let conversation_id = create_conversation(&t, &a_access, "rosa").await;

    // Real subscriber wired BEFORE the send so the publish cannot be missed.
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
    let (message_id, _seq) = send_and_ack(&mut ws_a, conversation_id, "notify-me").await;

    let published = tokio::time::timeout(READ_TIMEOUT, pubsub_stream.next())
        .await
        .expect("publish must arrive well within the timeout")
        .expect("pubsub stream open");
    let payload: String = published.get_payload().expect("string payload");
    assert_eq!(
        payload,
        message_id.to_string(),
        "publish payload is the message-id pointer"
    );
}

/// Documented anti-probing policy: cursors for conversations the caller does
/// not belong to (real-but-foreign and nonexistent alike) are silently
/// skipped — empty aggregated sync.res, complete=true, connection alive.
#[tokio::test]
async fn sync_req_skips_unauthorized_or_unknown_conversations_silently() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "svetlana@example.com", "svetlana").await;
    let (_b_id, _b_access) = register_user(&t, "tobias@example.com", "tobias").await;
    // Eve is a registered user but NOT a member of the conversation below.
    let (_eve_id, eve_access) = register_user(&t, "eve@example.com", "eve3").await;
    let conversation_id = create_conversation(&t, &a_access, "tobias").await;

    // Seed one real message so a leak would actually show up.
    let mut ws_a = ws_connect(&t, &a_access).await;
    let _ = send_and_ack(&mut ws_a, conversation_id, "secret").await;

    let mut ws_eve = ws_connect(&t, &eve_access).await;
    send_sync_req(&mut ws_eve, vec![(conversation_id, 0), (9_999_999_999, 0)]).await;
    let res = expect_sync_res(&mut ws_eve).await;
    assert!(
        res.messages.is_empty(),
        "no rows may leak: {:?}",
        res.messages
    );
    assert!(res.complete);

    // The connection survives (skip is not an error).
    let still_connected = ws_next_text(&mut ws_eve, Duration::from_millis(300)).await;
    assert!(
        still_connected.is_none(),
        "no error frame expected, got {still_connected:?}"
    );
}

/// Wire-shape decision check: multiple cursors aggregate into ONE sync.res,
/// ordered by (conversation_id, seq); partial cursors replay only the gap.
#[tokio::test]
async fn multi_cursor_sync_aggregates_into_one_ordered_response() {
    let _guard = GATE.lock().await;
    let t = test_app().await;

    let (_a_id, a_access) = register_user(&t, "ulrich@example.com", "ulrich").await;
    let (_b_id, _b_access) = register_user(&t, "vera@example.com", "vera").await;
    let (_c_id, _c_access) = register_user(&t, "willhelm@example.com", "willhelm").await;
    // Conversation ids are BIGINT identity: created-first ⇒ smaller id.
    let conv_ab = create_conversation(&t, &a_access, "vera").await;
    let conv_ac = create_conversation(&t, &a_access, "willhelm").await;
    assert!(conv_ab < conv_ac);

    // Peers stay offline: everything reaches A later purely via sync.
    let mut ws_a = ws_connect(&t, &a_access).await;
    let (_ab1, ab_seq1) = send_and_ack(&mut ws_a, conv_ab, "ab-1").await;
    let (_ac1, ac_seq1) = send_and_ack(&mut ws_a, conv_ac, "ac-1").await;
    let (_ab2, ab_seq2) = send_and_ack(&mut ws_a, conv_ab, "ab-2").await;
    assert_eq!((ab_seq1, ac_seq1, ab_seq2), (1, 1, 2));

    // Full catch-up across both conversations in one request/response.
    send_sync_req(&mut ws_a, vec![(conv_ab, 0), (conv_ac, 0)]).await;
    let res = expect_sync_res(&mut ws_a).await;
    assert!(res.complete);
    let order: Vec<(i64, i64, String)> = res
        .messages
        .iter()
        .map(as_plain)
        .map(|m| (m.conversation_id, m.seq, m.body.clone()))
        .collect();
    assert_eq!(
        order,
        vec![
            (conv_ab, 1, "ab-1".to_owned()),
            (conv_ab, 2, "ab-2".to_owned()),
            (conv_ac, 1, "ac-1".to_owned()),
        ],
        "aggregated by (conversation_id, seq)"
    );
    // Bodies arrive decrypted to plaintext (at-rest ciphertext never leaks).
    assert_eq!(as_plain(&res.messages[0]).body, "ab-1");

    // Partial cursors: only the actual gaps are replayed.
    send_sync_req(&mut ws_a, vec![(conv_ab, 2), (conv_ac, 0)]).await;
    let res = expect_sync_res(&mut ws_a).await;
    let order: Vec<(i64, i64)> = res
        .messages
        .iter()
        .map(as_plain)
        .map(|m| (m.conversation_id, m.seq))
        .collect();
    assert_eq!(order, vec![(conv_ac, 1)]);
    assert!(res.complete);
}
