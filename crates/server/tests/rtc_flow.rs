//! M14a RTC call-signaling integration tests: the `rtc.signal` relay, the
//! in-memory call registry (`call_busy` / roster / `ended`), membership +
//! secret-conversation gates, SDP/ICE verbatim relay, and `GET /api/rtc/config`.
//!
//! Harness mirrors `groups_flow.rs`: a global mutex + one-time schema reset +
//! per-test TRUNCATE, plus the shared cross-binary PG advisory lock so the
//! integration binaries never reset each other's schema. Every received wire
//! frame is decoded into `jiuyue_protocol::Frame` so assertions pin the real
//! protocol shapes.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use jiuyue_protocol::{ErrorCode, Frame, PROTOCOL_VERSION, Payload, RtcSignalIn, RtcSignalOut};
use jiuyue_server::state::AppState;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower::ServiceExt;
use uuid::Uuid;

static GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::const_new(()));
static SCHEMA_READY: AtomicBool = AtomicBool::new(false);

/// Same lock key as every other integration binary: the schema reset and
/// per-test TRUNCATEs are serialized across binaries.
const TEST_ADVISORY_LOCK_KEY: i64 = 0x6A_75_59_55_00_01;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const QUIET_WAIT: Duration = Duration::from_millis(300);

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestApp {
    app: Router,
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
        .max_connections(5)
        .connect(&env_var("TEST_DATABASE_URL"))
        .await
        .expect("connect TEST_DATABASE_URL");
    sqlx::query(
        "TRUNCATE users, auth_identities, devices, refresh_tokens, \
         conversations, conversation_members, messages, group_invites \
         RESTART IDENTITY CASCADE",
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

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
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

/// Creates (or gets) a direct or secret conversation; returns
/// `(conversation_id, kind)`.
async fn create_conversation(
    t: &TestApp,
    access: &str,
    peer_username: &str,
    kind: Option<&str>,
) -> (i64, String) {
    let body = match kind {
        Some(kind) => json!({ "peer_username": peer_username, "kind": kind }),
        None => json!({ "peer_username": peer_username }),
    };
    let (status, body) = send_http(&t.app, "POST", "/api/conversations", Some(access), Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (
        body["conversation_id"].as_i64().expect("conversation_id"),
        body["kind"].as_str().expect("kind").to_owned(),
    )
}

/// Creates an empty group, then invites + accepts each `(username, access)`
/// member; returns the group conversation id.
async fn group_with_members(
    t: &TestApp,
    owner_access: &str,
    members: &[(&str, &str)], // (username, access)
) -> i64 {
    let (status, created) = send_http(
        &t.app,
        "POST",
        "/api/groups",
        Some(owner_access),
        Some(json!({ "name": "CallGroup", "invite_usernames": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let conversation_id = created["conversation_id"].as_i64().expect("conversation_id");
    for (name, access) in members {
        let (status, body) = send_http(
            &t.app,
            "POST",
            &format!("/api/groups/{conversation_id}/invites"),
            Some(owner_access),
            Some(json!({ "username": name })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "invite {name}: {body}");
        let invite_id = body["invite_id"].as_str().expect("invite_id");
        let (status, body) = send_http(
            &t.app,
            "POST",
            &format!("/api/groups/invites/{invite_id}/accept"),
            Some(access),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "accept {name}: {body}");
    }
    conversation_id
}

// ---------------------------------------------------------------------------
// WebSocket helpers
// ---------------------------------------------------------------------------

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
    let msg = match tokio::time::timeout(READ_TIMEOUT, ws.next()).await {
        Ok(Some(Ok(msg))) => msg,
        Ok(other) => panic!("expected a websocket frame, got {other:?}"),
        Err(_) => panic!("timed out waiting for websocket frame"),
    };
    let text = msg.to_text().expect("text frame").to_owned();
    serde_json::from_str::<Frame>(&text)
        .expect("wire frame must decode into jiuyue_protocol::Frame")
}

/// Non-panicking read: returns `None` when nothing arrives within `wait`.
async fn ws_try_next_frame(ws: &mut WsClient, wait: Duration) -> Option<Frame> {
    match tokio::time::timeout(wait, ws.next()).await {
        Ok(Some(Ok(msg))) => {
            let text = msg.to_text().ok()?.to_owned();
            serde_json::from_str::<Frame>(&text).ok()
        }
        Ok(_) => None,
        Err(_) => None,
    }
}

/// Builds an empty C2S `rtc.signal` payload of `kind`.
fn rtc_in(conversation_id: i64, call_id: Uuid, kind: &str) -> RtcSignalIn {
    RtcSignalIn {
        conversation_id,
        call_id,
        kind: kind.to_owned(),
        to_user_id: None,
        media: None,
        reason: None,
        sdp_type: None,
        sdp: None,
        candidate: None,
        sdp_mid: None,
        sdp_mline_index: None,
    }
}

/// Sends one `rtc.signal` frame from a client.
async fn rtc_send(ws: &mut WsClient, signal: RtcSignalIn) {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::RtcSignal(RtcSignalOut {
            signal,
            from: None,
            participants: None,
        }),
    };
    ws_send_text(ws, &serde_json::to_string(&frame).unwrap()).await;
}

/// Reads the next frame and asserts it is an `rtc.signal` of `kind`.
async fn expect_rtc(ws: &mut WsClient, kind: &str) -> RtcSignalOut {
    match ws_next_frame(ws).await.payload {
        Payload::RtcSignal(out) => {
            assert_eq!(out.signal.kind, kind, "unexpected rtc.signal kind");
            out
        }
        other => panic!("expected rtc.signal {kind}, got {other:?}"),
    }
}

/// Reads the next frame and asserts it is an `error` with `code` + message.
async fn expect_rtc_error(ws: &mut WsClient, code: ErrorCode, contains: &str) {
    match ws_next_frame(ws).await.payload {
        Payload::Error(payload) => {
            assert_eq!(payload.code, code, "error code mismatch: {payload:?}");
            assert!(
                payload.message.contains(contains),
                "error message {:?} must contain {contains:?}",
                payload.message
            );
        }
        other => panic!("expected error frame, got {other:?}"),
    }
}

/// Collects participant user ids from an outbound roster frame.
fn participant_ids(out: &RtcSignalOut) -> Vec<Uuid> {
    out.participants
        .as_ref()
        .expect("roster-bearing frame must carry participants")
        .iter()
        .map(|user| user.user_id)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests: invite / busy / gates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dm_invite_relays_to_callee_and_second_invite_is_busy() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (a_id, a_access) = register_user(&t, "rt-a@example.com", "rta").await;
    let (_b_id, b_access) = register_user(&t, "rt-b@example.com", "rtb").await;
    let (conversation_id, kind) = create_conversation(&t, &a_access, "rtb", None).await;
    assert_eq!(kind, "direct");

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let call_id = Uuid::now_v7();
    let mut invite = rtc_in(conversation_id, call_id, "invite");
    invite.media = Some("audio".to_owned());
    rtc_send(&mut ws_a, invite).await;

    // Callee hears the ring with the caller's identity stamped.
    let ring = expect_rtc(&mut ws_b, "invite").await;
    assert_eq!(ring.signal.conversation_id, conversation_id);
    assert_eq!(ring.signal.call_id, call_id);
    assert_eq!(ring.signal.media.as_deref(), Some("audio"));
    let from = ring.from.expect("invite carries from");
    assert_eq!(from.user_id, a_id);
    assert_eq!(from.username, "rta");
    assert_eq!(from.display_name, "rta");

    // The initiator is excluded from its own ring.
    assert!(
        ws_try_next_frame(&mut ws_a, QUIET_WAIT).await.is_none(),
        "inviter must not receive its own invite"
    );

    // A second invite into the same conversation is busy.
    rtc_send(&mut ws_a, rtc_in(conversation_id, Uuid::now_v7(), "invite")).await;
    expect_rtc_error(&mut ws_a, ErrorCode::Conflict, "call_busy").await;
    assert!(
        ws_try_next_frame(&mut ws_b, QUIET_WAIT).await.is_none(),
        "a busy invite must not ring the callee"
    );
}

#[tokio::test]
async fn invite_rejected_for_non_member_and_secret_conversation() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "rts-a@example.com", "rtsa").await;
    let (_b_id, b_access) = register_user(&t, "rts-b@example.com", "rtsb").await;
    let (_c_id, c_access) = register_user(&t, "rts-c@example.com", "rtsc").await;

    let (direct_id, _) = create_conversation(&t, &a_access, "rtsb", None).await;
    let (secret_id, secret_kind) = create_conversation(&t, &a_access, "rtsb", Some("secret")).await;
    assert_eq!(secret_kind, "secret");

    let mut ws_b = ws_connect(&t, &b_access).await;
    let mut ws_c = ws_connect(&t, &c_access).await;
    let mut ws_a = ws_connect(&t, &a_access).await;

    // Outsider into a direct conversation: no membership/conversation oracle.
    rtc_send(&mut ws_c, rtc_in(direct_id, Uuid::now_v7(), "invite")).await;
    expect_rtc_error(&mut ws_c, ErrorCode::BadRequest, "not_a_member").await;

    // A member of a secret conversation: calls are unsupported there.
    rtc_send(&mut ws_a, rtc_in(secret_id, Uuid::now_v7(), "invite")).await;
    expect_rtc_error(
        &mut ws_a,
        ErrorCode::BadRequest,
        "calls_unsupported_in_secret",
    )
    .await;

    // Neither gate rang the callee.
    assert!(ws_try_next_frame(&mut ws_b, QUIET_WAIT).await.is_none());
}

// ---------------------------------------------------------------------------
// Tests: accept / roster / sdp / ice
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dm_accept_broadcasts_roster_to_both_participants() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (a_id, a_access) = register_user(&t, "ra-a@example.com", "raa").await;
    let (b_id, b_access) = register_user(&t, "ra-b@example.com", "rab").await;
    let (conversation_id, _) = create_conversation(&t, &a_access, "rab", None).await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let call_id = Uuid::now_v7();
    rtc_send(&mut ws_a, rtc_in(conversation_id, call_id, "invite")).await;
    let _ = expect_rtc(&mut ws_b, "invite").await;

    rtc_send(&mut ws_b, rtc_in(conversation_id, call_id, "accept")).await;

    for ws in [&mut ws_a, &mut ws_b] {
        let roster = expect_rtc(ws, "roster").await;
        assert_eq!(roster.signal.call_id, call_id);
        assert_eq!(roster.from.as_ref().map(|f| f.user_id), Some(b_id));
        let ids = participant_ids(&roster);
        assert_eq!(ids, vec![a_id, b_id], "roster lists both participants");
    }
}

#[tokio::test]
async fn sdp_and_ice_relay_verbatim_and_unknown_target_is_rejected() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (a_id, a_access) = register_user(&t, "rs-a@example.com", "rsa").await;
    let (b_id, b_access) = register_user(&t, "rs-b@example.com", "rsb").await;
    let (_c_id, _c_access) = register_user(&t, "rs-c@example.com", "rsc").await;
    let (conversation_id, _) = create_conversation(&t, &a_access, "rsb", None).await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let call_id = Uuid::now_v7();
    rtc_send(&mut ws_a, rtc_in(conversation_id, call_id, "invite")).await;
    let _ = expect_rtc(&mut ws_b, "invite").await;
    rtc_send(&mut ws_b, rtc_in(conversation_id, call_id, "accept")).await;
    let _ = expect_rtc(&mut ws_a, "roster").await;
    let _ = expect_rtc(&mut ws_b, "roster").await;

    // A to B SDP offer, relayed verbatim with from = A.
    let mut offer = rtc_in(conversation_id, call_id, "sdp");
    offer.to_user_id = Some(b_id);
    offer.sdp_type = Some("offer".to_owned());
    offer.sdp = Some("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n".to_owned());
    rtc_send(&mut ws_a, offer).await;
    let relayed = expect_rtc(&mut ws_b, "sdp").await;
    assert_eq!(relayed.signal.to_user_id, Some(b_id));
    assert_eq!(relayed.signal.sdp_type.as_deref(), Some("offer"));
    assert_eq!(
        relayed.signal.sdp.as_deref(),
        Some("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n"),
        "SDP body is byte-for-byte"
    );
    assert_eq!(relayed.from.as_ref().map(|f| f.user_id), Some(a_id));
    assert!(
        ws_try_next_frame(&mut ws_a, QUIET_WAIT).await.is_none(),
        "direct signal is not echoed to the sender"
    );

    // B to A ICE candidate, relayed verbatim with from = B.
    let mut ice = rtc_in(conversation_id, call_id, "ice");
    ice.to_user_id = Some(a_id);
    ice.candidate = Some(r#"{"candidate":"candidate:1 1 udp 1 127.0.0.1 9 typ host"}"#.to_owned());
    ice.sdp_mid = Some("0".to_owned());
    ice.sdp_mline_index = Some(0);
    rtc_send(&mut ws_b, ice).await;
    let relayed = expect_rtc(&mut ws_a, "ice").await;
    assert_eq!(
        relayed.signal.candidate.as_deref(),
        Some(r#"{"candidate":"candidate:1 1 udp 1 127.0.0.1 9 typ host"}"#)
    );
    assert_eq!(relayed.signal.sdp_mid.as_deref(), Some("0"));
    assert_eq!(relayed.signal.sdp_mline_index, Some(0));
    assert_eq!(relayed.from.as_ref().map(|f| f.user_id), Some(b_id));

    // Target outside the call -> call_not_found, no oracle.
    let mut stray = rtc_in(conversation_id, call_id, "sdp");
    stray.to_user_id = Some(Uuid::now_v7());
    stray.sdp_type = Some("offer".to_owned());
    stray.sdp = Some("v=0".to_owned());
    rtc_send(&mut ws_b, stray).await;
    expect_rtc_error(&mut ws_b, ErrorCode::NotFound, "call_not_found").await;
}

// ---------------------------------------------------------------------------
// Tests: hangup cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dm_hangup_ends_call_and_registry_is_reusable() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "rh-a@example.com", "rha").await;
    let (_b_id, b_access) = register_user(&t, "rh-b@example.com", "rhb").await;
    let (conversation_id, _) = create_conversation(&t, &a_access, "rhb", None).await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let call_id = Uuid::now_v7();
    rtc_send(&mut ws_a, rtc_in(conversation_id, call_id, "invite")).await;
    let _ = expect_rtc(&mut ws_b, "invite").await;
    rtc_send(&mut ws_b, rtc_in(conversation_id, call_id, "accept")).await;
    let _ = expect_rtc(&mut ws_a, "roster").await;
    let _ = expect_rtc(&mut ws_b, "roster").await;

    // B hangs up the 2-person DM -> both ends hear `ended` (empty participants).
    rtc_send(&mut ws_b, rtc_in(conversation_id, call_id, "hangup")).await;
    for ws in [&mut ws_a, &mut ws_b] {
        let ended = expect_rtc(ws, "ended").await;
        assert_eq!(ended.signal.call_id, call_id);
        assert_eq!(participant_ids(&ended), Vec::<Uuid>::new());
    }

    // Registry cleaned: a brand-new invite into the same conversation works --
    // no `call_busy`, and the callee rings again.
    let fresh_call = Uuid::now_v7();
    rtc_send(&mut ws_a, rtc_in(conversation_id, fresh_call, "invite")).await;
    assert!(
        ws_try_next_frame(&mut ws_a, QUIET_WAIT).await.is_none(),
        "cleanup means the new invite is not rejected as busy"
    );
    let ring = expect_rtc(&mut ws_b, "invite").await;
    assert_eq!(ring.signal.call_id, fresh_call);
}

// ---------------------------------------------------------------------------
// Tests: group mesh
// ---------------------------------------------------------------------------

#[tokio::test]
async fn group_call_roster_join_leave_and_last_leaver_ends() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (a_id, a_access) = register_user(&t, "rg-a@example.com", "rga").await;
    let (b_id, b_access) = register_user(&t, "rg-b@example.com", "rgb").await;
    let (c_id, c_access) = register_user(&t, "rg-c@example.com", "rgc").await;

    let conversation_id = group_with_members(
        &t,
        &a_access,
        &[("rgb", b_access.as_str()), ("rgc", c_access.as_str())],
    )
    .await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;
    let mut ws_c = ws_connect(&t, &c_access).await;

    // A invites: the call rings EVERY other member; A is excluded.
    let call_id = Uuid::now_v7();
    rtc_send(&mut ws_a, rtc_in(conversation_id, call_id, "invite")).await;
    assert_eq!(
        expect_rtc(&mut ws_b, "invite").await.from.unwrap().user_id,
        a_id
    );
    assert_eq!(
        expect_rtc(&mut ws_c, "invite").await.from.unwrap().user_id,
        a_id
    );
    assert!(ws_try_next_frame(&mut ws_a, QUIET_WAIT).await.is_none());

    // B joins -> roster(2) to A+B, join relayed to A.
    rtc_send(&mut ws_b, rtc_in(conversation_id, call_id, "join")).await;
    let roster_a = expect_rtc(&mut ws_a, "roster").await;
    assert_eq!(participant_ids(&roster_a), vec![a_id, b_id]);
    assert_eq!(
        expect_rtc(&mut ws_a, "join").await.from.unwrap().user_id,
        b_id
    );
    let roster_b = expect_rtc(&mut ws_b, "roster").await;
    assert_eq!(participant_ids(&roster_b), vec![a_id, b_id]);

    // C joins -> roster(3) everywhere, join relayed to A+B.
    rtc_send(&mut ws_c, rtc_in(conversation_id, call_id, "join")).await;
    for (ws, expected) in [
        (&mut ws_a, vec![a_id, b_id, c_id]),
        (&mut ws_b, vec![a_id, b_id, c_id]),
    ] {
        let roster = expect_rtc(ws, "roster").await;
        assert_eq!(participant_ids(&roster), expected);
        assert_eq!(expect_rtc(ws, "join").await.from.unwrap().user_id, c_id);
    }
    let roster_c = expect_rtc(&mut ws_c, "roster").await;
    assert_eq!(participant_ids(&roster_c), vec![a_id, b_id, c_id]);

    // B leaves -> shrunk roster(2: A,C) plus the leave relayed to A+C.
    rtc_send(&mut ws_b, rtc_in(conversation_id, call_id, "leave")).await;
    for (ws, expected) in [
        (&mut ws_a, vec![a_id, c_id]),
        (&mut ws_c, vec![a_id, c_id]),
    ] {
        let roster = expect_rtc(ws, "roster").await;
        assert_eq!(participant_ids(&roster), expected);
        assert_eq!(expect_rtc(ws, "leave").await.from.unwrap().user_id, b_id);
    }
    assert!(
        ws_try_next_frame(&mut ws_b, QUIET_WAIT).await.is_none(),
        "a leaver is no longer a participant and hears no roster"
    );

    // C leaves -> roster(1: A) plus leave.
    rtc_send(&mut ws_c, rtc_in(conversation_id, call_id, "leave")).await;
    let roster_a = expect_rtc(&mut ws_a, "roster").await;
    assert_eq!(participant_ids(&roster_a), vec![a_id]);
    assert_eq!(
        expect_rtc(&mut ws_a, "leave").await.from.unwrap().user_id,
        c_id
    );

    // A (the last participant) leaves -> the call ends.
    rtc_send(&mut ws_a, rtc_in(conversation_id, call_id, "hangup")).await;
    let ended = expect_rtc(&mut ws_a, "ended").await;
    assert_eq!(participant_ids(&ended), Vec::<Uuid>::new());

    // And the group conversation is free for a fresh call.
    let fresh = Uuid::now_v7();
    rtc_send(&mut ws_a, rtc_in(conversation_id, fresh, "invite")).await;
    assert_eq!(expect_rtc(&mut ws_b, "invite").await.signal.call_id, fresh);
    assert_eq!(expect_rtc(&mut ws_c, "invite").await.signal.call_id, fresh);
}

// ---------------------------------------------------------------------------
// Tests: config endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rtc_config_returns_stun_list_and_requires_auth() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "rc-a@example.com", "rca").await;

    let (status, body) = send_http(&t.app, "GET", "/api/rtc/config", Some(&access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let servers = body["ice_servers"].as_array().expect("ice_servers array");
    assert!(!servers.is_empty(), "at least one ICE server");
    let urls = servers[0]["urls"].as_array().expect("urls array");
    assert!(
        urls.iter()
            .any(|url| url.as_str().is_some_and(|u| u.starts_with("stun:"))),
        "default config advertises a STUN server: {body}"
    );

    let (status, _) = send_http(&t.app, "GET", "/api/rtc/config", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "Bearer auth is required");
}
