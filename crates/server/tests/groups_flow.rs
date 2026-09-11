//! M11a group-chat integration tests: creation + validation, consent-based
//! invites (accept/decline/duplicates), the owner/admin/member role matrix
//! (kick, role, transfer, leave), owner moderation recall, WS wire
//! notifications (`group.invited` / `group.updated`) and the group shape of
//! the conversation listing.
//!
//! Harness mirrors `media_flow.rs`: a global mutex + one-time schema reset +
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
use jiuyue_protocol::{
    Frame, GroupInvited, GroupUpdated, MsgAck, MsgNew, MsgRecall, MsgRecalled, MsgSend,
    PROTOCOL_VERSION, Payload, SyncCursor, SyncMessage, SyncReq,
};
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
        pool,
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

/// Creates a group, asserting 201, and returns the response body.
async fn create_group(t: &TestApp, access: &str, name: &str, invite: &[&str]) -> Value {
    let (status, body) = create_group_raw(t, access, name, invite).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

async fn create_group_raw(
    t: &TestApp,
    access: &str,
    name: &str,
    invite: &[&str],
) -> (StatusCode, Value) {
    let body = json!({ "name": name, "invite_usernames": invite });
    send_http(&t.app, "POST", "/api/groups", Some(access), Some(body)).await
}

async fn invite(
    t: &TestApp,
    access: &str,
    conversation_id: i64,
    username: &str,
) -> (StatusCode, Value) {
    send_http(
        &t.app,
        "POST",
        &format!("/api/groups/{conversation_id}/invites"),
        Some(access),
        Some(json!({ "username": username })),
    )
    .await
}

async fn accept_invite(t: &TestApp, access: &str, invite_id: &str) -> (StatusCode, Value) {
    send_http(
        &t.app,
        "POST",
        &format!("/api/groups/invites/{invite_id}/accept"),
        Some(access),
        None,
    )
    .await
}

async fn decline_invite(t: &TestApp, access: &str, invite_id: &str) -> (StatusCode, Value) {
    send_http(
        &t.app,
        "POST",
        &format!("/api/groups/invites/{invite_id}/decline"),
        Some(access),
        None,
    )
    .await
}

async fn get_group(t: &TestApp, access: &str, conversation_id: i64) -> (StatusCode, Value) {
    send_http(
        &t.app,
        "GET",
        &format!("/api/groups/{conversation_id}"),
        Some(access),
        None,
    )
    .await
}

async fn list_invites(t: &TestApp, access: &str) -> Vec<Value> {
    let (status, body) =
        send_http(&t.app, "GET", "/api/groups/invites", Some(access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array().cloned().unwrap_or_default()
}

async fn list_conversations(t: &TestApp, access: &str) -> Vec<Value> {
    let (status, body) =
        send_http(&t.app, "GET", "/api/conversations", Some(access), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array().cloned().unwrap_or_default()
}

async fn set_role(
    t: &TestApp,
    access: &str,
    conversation_id: i64,
    user_id: Uuid,
    role: &str,
) -> (StatusCode, Value) {
    send_http(
        &t.app,
        "POST",
        &format!("/api/groups/{conversation_id}/members/{user_id}/role"),
        Some(access),
        Some(json!({ "role": role })),
    )
    .await
}

async fn kick(
    t: &TestApp,
    access: &str,
    conversation_id: i64,
    user_id: Uuid,
) -> (StatusCode, Value) {
    send_http(
        &t.app,
        "POST",
        &format!("/api/groups/{conversation_id}/members/{user_id}/kick"),
        Some(access),
        None,
    )
    .await
}

async fn transfer(
    t: &TestApp,
    access: &str,
    conversation_id: i64,
    user_id: Uuid,
) -> (StatusCode, Value) {
    send_http(
        &t.app,
        "POST",
        &format!("/api/groups/{conversation_id}/transfer"),
        Some(access),
        Some(json!({ "user_id": user_id })),
    )
    .await
}

async fn leave(t: &TestApp, access: &str, conversation_id: i64) -> (StatusCode, Value) {
    send_http(
        &t.app,
        "POST",
        &format!("/api/groups/{conversation_id}/leave"),
        Some(access),
        None,
    )
    .await
}

/// Invites `username` and returns the created `invite_id` string.
async fn invite_ok(t: &TestApp, access: &str, conversation_id: i64, username: &str) -> String {
    let (status, body) = invite(t, access, conversation_id, username).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["invite_id"].as_str().expect("invite_id").to_owned()
}

/// Creates an empty group, then invites + accepts each `(username, access)`
/// member (one pending invite at a time; no duplicate-invite conflicts).
async fn group_with_members(
    t: &TestApp,
    owner_access: &str,
    owner_username: &str,
    members: &[(&str, &str)], // (username, access)
) -> i64 {
    let _ = owner_username;
    let created = create_group(t, owner_access, "测试群", &[]).await;
    let conversation_id = created["conversation_id"].as_i64().expect("conversation_id");
    for (name, access) in members {
        let invite_id = invite_ok(t, owner_access, conversation_id, name).await;
        let (status, body) = accept_invite(t, access, &invite_id).await;
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

/// Sends one `msg.send` and returns the resulting `msg.ack` message_id.
async fn ws_send_text_message(ws: &mut WsClient, conversation_id: i64, body: &str) -> (Uuid, Uuid) {
    let client_msg_id = Uuid::now_v7();
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id,
            body: body.to_owned(),
            reply_to: None,
            media: None,
            forward_of_message_id: None,
        }),
    };
    ws_send_text(ws, &serde_json::to_string(&frame).unwrap()).await;
    match ws_next_frame(ws).await.payload {
        Payload::MsgAck(MsgAck {
            client_msg_id: got,
            message_id,
            ..
        }) => {
            assert_eq!(got, client_msg_id);
            (client_msg_id, message_id)
        }
        other => panic!("expected msg.ack, got {other:?}"),
    }
}

fn expect_msg_new(frame: Frame) -> MsgNew {
    match frame.payload {
        Payload::MsgNew(msg) => msg,
        other => panic!("expected msg.new, got {other:?}"),
    }
}

fn expect_recalled(frame: Frame, conversation_id: i64, message_id: Uuid) {
    match frame.payload {
        Payload::MsgRecalled(MsgRecalled {
            conversation_id: got_conv,
            message_id: got_msg,
        }) => {
            assert_eq!(got_conv, conversation_id);
            assert_eq!(got_msg, message_id);
        }
        other => panic!("expected msg.recalled, got {other:?}"),
    }
}

fn expect_group_updated(frame: Frame, conversation_id: i64) {
    match frame.payload {
        Payload::GroupUpdated(GroupUpdated { conversation_id: got }) => {
            assert_eq!(got, conversation_id);
        }
        other => panic!("expected group.updated, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests: creation + validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_group_sets_owner_role_and_pending_invites() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "gc-a@example.com", "gca").await;
    let (invitee_id, _invitee_access) = register_user(&t, "gc-b@example.com", "gcb").await;

    let created = create_group(&t, &owner_access, "  项目组  ", &["gcb", "gcb"]).await;
    assert_eq!(created["name"], "项目组", "name is trimmed");
    assert_eq!(created["member_count"], 1);
    assert_eq!(
        created["invited"],
        json!(["gcb"]),
        "duplicate usernames collapse"
    );
    let conversation_id = created["conversation_id"].as_i64().expect("conversation_id");

    let (kind, name): (String, Option<String>) =
        sqlx::query_as("SELECT kind, name FROM conversations WHERE id = $1")
            .bind(conversation_id)
            .fetch_one(&t.pool)
            .await
            .expect("conversation row");
    assert_eq!(kind, "group");
    assert_eq!(name.as_deref(), Some("项目组"));

    let role: String = sqlx::query_scalar(
        "SELECT role FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(owner_id)
    .fetch_one(&t.pool)
    .await
    .expect("owner membership");
    assert_eq!(role, "owner");

    let (to_user, status): (Uuid, String) =
        sqlx::query_as("SELECT to_user, status FROM group_invites WHERE conversation_id = $1")
            .bind(conversation_id)
            .fetch_one(&t.pool)
            .await
            .expect("invite row");
    assert_eq!(to_user, invitee_id);
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn create_group_validates_name_and_unknown_invitee_rejects_all() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "gc-c@example.com", "gcc").await;

    // Empty and over-long names are 422.
    let (status, body) = create_group_raw(&t, &access, "   ", &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let too_long = "x".repeat(33);
    let (status, body) = create_group_raw(&t, &access, &too_long, &[]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Exactly 32 characters is accepted.
    let exact = "y".repeat(32);
    let created = create_group(&t, &access, &exact, &[]).await;
    assert_eq!(created["name"], json!(exact));

    // Unknown invitee: whole request 404 and NOTHING new is created.
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM conversations")
        .fetch_one(&t.pool)
        .await
        .expect("count before");
    let (status, body) = create_group_raw(&t, &access, "should-not-exist", &["ghost-user"]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], json!("peer_not_found"));
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM conversations")
        .fetch_one(&t.pool)
        .await
        .expect("count after");
    assert_eq!(before, after, "failed create must persist nothing");
    let invites: i64 = sqlx::query_scalar("SELECT count(*) FROM group_invites")
        .fetch_one(&t.pool)
        .await
        .expect("invite count");
    assert_eq!(invites, 0);
}

// ---------------------------------------------------------------------------
// Tests: invite accept / decline / duplicates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invite_accept_joins_group_and_shows_in_listing() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "gi-a@example.com", "gia").await;
    let (invitee_id, invitee_access) = register_user(&t, "gi-b@example.com", "gib").await;

    let created = create_group(&t, &owner_access, "设计组", &[]).await;
    let conversation_id = created["conversation_id"].as_i64().unwrap();
    let invite_id = invite_ok(&t, &owner_access, conversation_id, "gib").await;

    // Invitee sees the pending invite with group name + inviter.
    let invites = list_invites(&t, &invitee_access).await;
    assert_eq!(invites.len(), 1, "{invites:?}");
    assert_eq!(invites[0]["invite_id"], json!(invite_id));
    assert_eq!(invites[0]["conversation_id"], json!(conversation_id));
    assert_eq!(invites[0]["group_name"], "设计组");
    assert_eq!(invites[0]["from"]["username"], "gia");

    let (status, body) = accept_invite(&t, &invitee_access, &invite_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["conversation_id"], json!(conversation_id));

    // Pending invite is consumed.
    assert!(list_invites(&t, &invitee_access).await.is_empty());

    // Group now appears in the invitee's conversation listing.
    let items = list_conversations(&t, &invitee_access).await;
    let item = items
        .iter()
        .find(|item| item["conversation_id"] == json!(conversation_id))
        .expect("group in listing");
    assert_eq!(item["kind"], "group");
    assert_eq!(item["name"], "设计组");
    assert!(item["peer"].is_null(), "groups have no single peer");

    // Owner sees the same title in its listing; peer stays null.
    let owner_items = list_conversations(&t, &owner_access).await;
    assert_eq!(owner_items[0]["kind"], "group");
    assert_eq!(owner_items[0]["name"], "设计组");
    assert!(owner_items[0]["peer"].is_null());

    // Roster: owner first, invitee second as member.
    let (status, body) = get_group(&t, &invitee_access, conversation_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["my_role"], "member");
    assert_eq!(body["member_count"], 2);
    let members = body["members"].as_array().expect("members");
    assert_eq!(members[0]["role"], "owner");
    assert_eq!(members[0]["user_id"], json!(owner_id));
    assert_eq!(members[1]["role"], "member");
    assert_eq!(members[1]["user_id"], json!(invitee_id));
    assert!(members[0]["joined_at"].is_string());

    // Re-inviting an existing member is a conflict; unknown user is 404.
    let (status, body) = invite(&t, &owner_access, conversation_id, "gib").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], json!("already_member"));
    let (status, body) = invite(&t, &owner_access, conversation_id, "nobody-here").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], json!("peer_not_found"));
}

#[tokio::test]
async fn duplicate_pending_invite_is_conflict() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_o, owner_access) = register_user(&t, "gd-a@example.com", "gda").await;
    let (_b, _b_access) = register_user(&t, "gd-b@example.com", "gdb").await;

    let created = create_group(&t, &owner_access, "重复", &[]).await;
    let conversation_id = created["conversation_id"].as_i64().unwrap();
    invite_ok(&t, &owner_access, conversation_id, "gdb").await;

    let (status, body) = invite(&t, &owner_access, conversation_id, "gdb").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], json!("invite_already_pending"));

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM group_invites")
        .fetch_one(&t.pool)
        .await
        .expect("invite count");
    assert_eq!(rows, 1, "only one pending invite row");
}

#[tokio::test]
async fn invite_decline_keeps_user_out() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_o, owner_access) = register_user(&t, "ge-a@example.com", "gea").await;
    let (_b, invitee_access) = register_user(&t, "ge-b@example.com", "geb").await;

    let created = create_group(&t, &owner_access, "拒绝", &[]).await;
    let conversation_id = created["conversation_id"].as_i64().unwrap();
    let invite_id = invite_ok(&t, &owner_access, conversation_id, "geb").await;

    let (status, _) = decline_invite(&t, &invitee_access, &invite_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(list_invites(&t, &invitee_access).await.is_empty());

    // Declined invite can no longer be accepted.
    let (status, _) = accept_invite(&t, &invitee_access, &invite_id).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Not a member: not in listing and group read is 404 (no oracle).
    assert!(list_conversations(&t, &invitee_access).await.is_empty());
    let (status, _) = get_group(&t, &invitee_access, conversation_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Tests: kick / role / transfer / leave
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kick_role_matrix_enforced() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "gk-a@example.com", "gka").await;
    let (admin_id, admin_access) = register_user(&t, "gk-b@example.com", "gkb").await;
    let (member_id, member_access) = register_user(&t, "gk-c@example.com", "gkc").await;
    let (admin2_id, admin2_access) = register_user(&t, "gk-d@example.com", "gkd").await;
    let (_outsider_id, outsider_access) = register_user(&t, "gk-e@example.com", "gke").await;

    let conversation_id = group_with_members(
        &t,
        &owner_access,
        "gka",
        &[
            ("gkb", admin_access.as_str()),
            ("gkc", member_access.as_str()),
            ("gkd", admin2_access.as_str()),
        ],
    )
    .await;

    // Promote gkb and gkd to admin.
    let (status, body) = set_role(&t, &owner_access, conversation_id, admin_id, "admin").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = set_role(&t, &owner_access, conversation_id, admin2_id, "admin").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Admin cannot kick another admin.
    let (status, body) = kick(&t, &admin_access, conversation_id, admin2_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Plain member cannot kick anyone.
    let (status, body) = kick(&t, &member_access, conversation_id, admin2_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Nobody can kick the owner.
    let (status, body) = kick(&t, &admin_access, conversation_id, owner_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Non-member cannot kick (404, no oracle).
    let (status, _) = kick(&t, &outsider_access, conversation_id, member_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Admin CAN kick a plain member.
    let (status, _) = kick(&t, &admin_access, conversation_id, member_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Owner CAN kick an admin.
    let (status, _) = kick(&t, &owner_access, conversation_id, admin2_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Kicked members lose access: listing excludes the group, read is 404.
    assert!(
        list_conversations(&t, &member_access)
            .await
            .iter()
            .all(|item| item["conversation_id"] != json!(conversation_id)),
        "kicked member must not see the group"
    );
    let (status, _) = get_group(&t, &member_access, conversation_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        list_conversations(&t, &owner_access)
            .await
            .iter()
            .filter(|item| item["kind"] == "group")
            .count(),
        1
    );
}

#[tokio::test]
async fn role_change_requires_owner() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "gr-a@example.com", "gra").await;
    let (target_id, target_access) = register_user(&t, "gr-b@example.com", "grb").await;

    let conversation_id = group_with_members(
        &t,
        &owner_access,
        "gra",
        &[("grb", target_access.as_str())],
    )
    .await;

    let (status, body) = set_role(&t, &owner_access, conversation_id, target_id, "admin").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = get_group(&t, &target_access, conversation_id).await;
    assert_eq!(body["my_role"], "admin");

    let (status, body) = set_role(&t, &owner_access, conversation_id, target_id, "member").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = get_group(&t, &target_access, conversation_id).await;
    assert_eq!(body["my_role"], "member");

    // Non-owner (even admin) cannot change roles.
    let (status, body) = set_role(&t, &owner_access, conversation_id, target_id, "admin").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = set_role(&t, &target_access, conversation_id, owner_id, "member").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Owner's own role cannot be changed; invalid role strings are 422.
    let (status, _) = set_role(&t, &owner_access, conversation_id, owner_id, "member").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = set_role(&t, &owner_access, conversation_id, target_id, "superadmin").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn ownership_transfer_moves_owner_and_demotes_old_owner() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "gt-a@example.com", "gta").await;
    let (target_id, target_access) = register_user(&t, "gt-b@example.com", "gtb").await;

    let conversation_id = group_with_members(
        &t,
        &owner_access,
        "gta",
        &[("gtb", target_access.as_str())],
    )
    .await;

    // Non-owner cannot transfer.
    let (status, body) = transfer(&t, &target_access, conversation_id, owner_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Owner transfers to the member.
    let (status, body) = transfer(&t, &owner_access, conversation_id, target_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let roles: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT user_id, role FROM conversation_members WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_all(&t.pool)
    .await
    .expect("roles");
    let role_of = |id: Uuid| roles.iter().find(|(u, _)| *u == id).map(|(_, r)| r.clone());
    assert_eq!(role_of(target_id).as_deref(), Some("owner"));
    assert_eq!(role_of(owner_id).as_deref(), Some("member"));

    let (_, body) = get_group(&t, &target_access, conversation_id).await;
    assert_eq!(body["my_role"], "owner");
    let (_, body) = get_group(&t, &owner_access, conversation_id).await;
    assert_eq!(body["my_role"], "member");

    // Old owner (now member) can no longer transfer.
    let (status, body) = transfer(&t, &owner_access, conversation_id, target_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn leave_removes_member_but_owner_must_transfer_first() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_owner_id, owner_access) = register_user(&t, "gl-a@example.com", "gla").await;
    let (_member_id, member_access) = register_user(&t, "gl-b@example.com", "glb").await;

    let conversation_id = group_with_members(
        &t,
        &owner_access,
        "gla",
        &[("glb", member_access.as_str())],
    )
    .await;

    // Owner cannot leave outright.
    let (status, body) = leave(&t, &owner_access, conversation_id).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Member leaves → access gone.
    let (status, _) = leave(&t, &member_access, conversation_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(list_conversations(&t, &member_access).await.is_empty());
    let (status, _) = get_group(&t, &member_access, conversation_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Tests: owner moderation recall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn group_owner_can_recall_members_message_and_sync_replays_tombstone() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "gm-a@example.com", "gma").await;
    let (_member_id, member_access) = register_user(&t, "gm-b@example.com", "gmb").await;

    let conversation_id = group_with_members(
        &t,
        &owner_access,
        "gma",
        &[("gmb", member_access.as_str())],
    )
    .await;

    let mut ws_owner = ws_connect(&t, &owner_access).await;
    let mut ws_member = ws_connect(&t, &member_access).await;

    // Member sends; owner (not the author) receives msg.new.
    let (_, message_id) = ws_send_text_message(&mut ws_member, conversation_id, "请撤回我").await;
    let delivered = expect_msg_new(ws_next_frame(&mut ws_owner).await);
    assert_eq!(delivered.message_id, message_id);
    assert_ne!(delivered.sender_id, owner_id);

    // Owner moderation-recall: no window, another user's message.
    let recall = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgRecall(MsgRecall {
            conversation_id,
            message_id,
        }),
    };
    ws_send_text(&mut ws_owner, &serde_json::to_string(&recall).unwrap()).await;

    expect_recalled(ws_next_frame(&mut ws_owner).await, conversation_id, message_id);
    expect_recalled(
        ws_next_frame(&mut ws_member).await,
        conversation_id,
        message_id,
    );

    // Sync replays the tombstone: recalled, empty body.
    let sync = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::SyncReq(SyncReq {
            cursors: vec![SyncCursor {
                conversation_id,
                last_delivered_seq: 0,
            }],
        }),
    };
    ws_send_text(&mut ws_member, &serde_json::to_string(&sync).unwrap()).await;
    match ws_next_frame(&mut ws_member).await.payload {
        Payload::SyncRes(res) => {
            let entry = res
                .messages
                .iter()
                .find_map(|message| match message {
                    SyncMessage::Plain(msg) if msg.message_id == message_id => Some(msg),
                    _ => None,
                })
                .expect("recalled message present in sync");
            assert!(entry.recalled, "sync entry must be a tombstone");
            assert_eq!(entry.body, "", "tombstone body is empty");
        }
        other => panic!("expected sync.res, got {other:?}"),
    }
}

#[tokio::test]
async fn group_admin_cannot_recall_someone_elses_message() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_owner_id, owner_access) = register_user(&t, "gn-a@example.com", "gna").await;
    let (admin_id, admin_access) = register_user(&t, "gn-b@example.com", "gnb").await;
    let (_member_id, member_access) = register_user(&t, "gn-c@example.com", "gnc").await;

    let conversation_id = group_with_members(
        &t,
        &owner_access,
        "gna",
        &[
            ("gnb", admin_access.as_str()),
            ("gnc", member_access.as_str()),
        ],
    )
    .await;
    let (status, body) = set_role(&t, &owner_access, conversation_id, admin_id, "admin").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mut ws_owner = ws_connect(&t, &owner_access).await;
    let mut ws_admin = ws_connect(&t, &admin_access).await;
    let mut ws_member = ws_connect(&t, &member_access).await;

    // Member sends; owner + admin receive msg.new.
    let (_, message_id) = ws_send_text_message(&mut ws_member, conversation_id, "成员消息").await;
    let _ = expect_msg_new(ws_next_frame(&mut ws_owner).await);
    let _ = expect_msg_new(ws_next_frame(&mut ws_admin).await);

    // Admin tries to moderation-recall: refused (not the sender).
    let recall = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgRecall(MsgRecall {
            conversation_id,
            message_id,
        }),
    };
    ws_send_text(&mut ws_admin, &serde_json::to_string(&recall).unwrap()).await;
    match ws_next_frame(&mut ws_admin).await.payload {
        Payload::Error(payload) => {
            assert_eq!(payload.code, jiuyue_protocol::ErrorCode::Unauthorized);
        }
        other => panic!("expected error frame, got {other:?}"),
    }

    let recalled: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT recalled_at FROM messages WHERE id = $1")
            .bind(message_id)
            .fetch_one(&t.pool)
            .await
            .expect("message row");
    assert!(recalled.is_none(), "admin recall must not tombstone");
}

// ---------------------------------------------------------------------------
// Tests: wire notifications
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invitee_receives_group_invited_and_members_receive_group_updated() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "gw-a@example.com", "gwa").await;
    let (_b_id, invitee_access) = register_user(&t, "gw-b@example.com", "gwb").await;

    let mut ws_invitee = ws_connect(&t, &invitee_access).await;

    let created = create_group(&t, &owner_access, "通知群", &[]).await;
    let conversation_id = created["conversation_id"].as_i64().unwrap();
    let invite_id = invite_ok(&t, &owner_access, conversation_id, "gwb").await;

    // invitee got group.invited with the group title and inviter identity.
    match ws_next_frame(&mut ws_invitee).await.payload {
        Payload::GroupInvited(GroupInvited {
            invite_id: got_id,
            conversation_id: got_conv,
            group_name,
            from,
        }) => {
            assert_eq!(got_id.to_string(), invite_id);
            assert_eq!(got_conv, conversation_id);
            assert_eq!(group_name, "通知群");
            assert_eq!(from.user_id, owner_id);
            assert_eq!(from.username, "gwa");
        }
        other => panic!("expected group.invited, got {other:?}"),
    }

    // Owner connects, then the invitee accepts → BOTH members get group.updated.
    let mut ws_owner = ws_connect(&t, &owner_access).await;
    let (status, _) = accept_invite(&t, &invitee_access, &invite_id).await;
    assert_eq!(status, StatusCode::OK);
    expect_group_updated(ws_next_frame(&mut ws_owner).await, conversation_id);
    expect_group_updated(ws_next_frame(&mut ws_invitee).await, conversation_id);
}

#[tokio::test]
async fn role_transfer_and_kick_broadcast_group_updated() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_owner_id, owner_access) = register_user(&t, "gx-a@example.com", "gxa").await;
    let (b_id, b_access) = register_user(&t, "gx-b@example.com", "gxb").await;
    let (c_id, c_access) = register_user(&t, "gx-c@example.com", "gxc").await;

    let conversation_id = group_with_members(
        &t,
        &owner_access,
        "gxa",
        &[("gxb", b_access.as_str()), ("gxc", c_access.as_str())],
    )
    .await;

    let mut ws_owner = ws_connect(&t, &owner_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;
    let mut ws_c = ws_connect(&t, &c_access).await;

    // Promote b → all three members get group.updated.
    let (status, _) = set_role(&t, &owner_access, conversation_id, b_id, "admin").await;
    assert_eq!(status, StatusCode::OK);
    expect_group_updated(ws_next_frame(&mut ws_owner).await, conversation_id);
    expect_group_updated(ws_next_frame(&mut ws_b).await, conversation_id);
    expect_group_updated(ws_next_frame(&mut ws_c).await, conversation_id);

    // Transfer ownership to b → all three get group.updated.
    let (status, _) = transfer(&t, &owner_access, conversation_id, b_id).await;
    assert_eq!(status, StatusCode::OK);
    expect_group_updated(ws_next_frame(&mut ws_owner).await, conversation_id);
    expect_group_updated(ws_next_frame(&mut ws_b).await, conversation_id);
    expect_group_updated(ws_next_frame(&mut ws_c).await, conversation_id);

    // New owner b kicks c → b + the (still member) old owner get
    // group.updated; kicked c does NOT.
    let (status, _) = kick(&t, &b_access, conversation_id, c_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    expect_group_updated(ws_next_frame(&mut ws_b).await, conversation_id);
    // owner_id connection is also still open; it gets the update.
    expect_group_updated(ws_next_frame(&mut ws_owner).await, conversation_id);
    // c was kicked, so it should receive nothing more (drain any leftovers first).
    assert!(
        ws_try_next_frame(&mut ws_c, Duration::from_millis(300))
            .await
            .is_none(),
        "kicked member must not receive group.updated"
    );

    assert!(list_conversations(&t, &c_access).await.is_empty());
}
