//! M13a group-file storage integration tests: member-only raw uploads (CJK
//! names, per-file streaming cap), the 1 GiB free-quota → 7-day temporary
//! pricing rule, usage/listings (expired exclusion, newest-first), member-only
//! downloads with attachment disposition, the uploader/owner/admin delete
//! matrix, and the `group.updated` nudges fired after upload/delete.
//!
//! Harness mirrors `groups_flow.rs` / `media_flow.rs`: a global mutex +
//! one-time schema reset + per-test TRUNCATE, plus the shared cross-binary PG
//! advisory lock. Uploads are pointed at a per-run temp directory and the
//! per-file cap is shrunk through the public `AppState` field (no env
//! mutation), exactly like `media_flow.rs` overrides `media_dir`.

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use jiuyue_protocol::{Frame, GroupUpdated, Payload};
use jiuyue_server::group_files::{FREE_GROUP_BYTES, TEMPORARY_TTL_DAYS};
use jiuyue_server::state::AppState;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

static GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::const_new(()));
static SCHEMA_READY: AtomicBool = AtomicBool::new(false);

/// Same lock key as every other integration binary: the schema reset and
/// per-test TRUNCATEs are serialized across binaries.
const TEST_ADVISORY_LOCK_KEY: i64 = 0x6A_75_59_55_00_01;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Shrunk per-file cap for the harness so the 413 test does not allocate
/// 200 MiB; the production default is verified by the `AppState` env parse.
const TEST_GROUP_FILE_CAP: u64 = 4096;

const TINY_PDF: &[u8] = b"%PDF-1.7 tiny";

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestApp {
    app: Router,
    pool: PgPool,
    port: u16,
    media_dir: PathBuf,
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
         conversations, conversation_members, messages, group_invites, group_files \
         RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate tables");
    let redis_client = redis::Client::open(env_var("REDIS_URL").as_str()).expect("parse redis url");
    let redis = redis::aio::ConnectionManager::new(redis_client)
        .await
        .expect("connect redis");
    let mut state = AppState::new(pool.clone(), redis.clone(), env_var("JIUYUE_JWT_SECRET"));

    // Per-run temp media dir + shrunk per-file cap (public test hooks).
    let media_dir = std::env::temp_dir().join(format!("jiuyue-group-files-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&media_dir).expect("create media temp dir");
    state.media_dir = Arc::new(media_dir.clone());
    state.group_file_max_bytes = TEST_GROUP_FILE_CAP;

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
        media_dir,
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
    let user_id: Uuid = reg["user_id"].as_str().expect("user_id").parse().unwrap();
    let access = reg["access_token"].as_str().expect("access").to_owned();
    (user_id, access)
}

async fn create_group(t: &TestApp, access: &str, name: &str) -> i64 {
    let body = json!({ "name": name, "invite_usernames": [] });
    let (status, created) = send_http(&t.app, "POST", "/api/groups", Some(access), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    created["conversation_id"].as_i64().expect("conversation_id")
}

async fn invite_ok(t: &TestApp, access: &str, conversation_id: i64, username: &str) -> String {
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/groups/{conversation_id}/invites"),
        Some(access),
        Some(json!({ "username": username })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["invite_id"].as_str().expect("invite_id").to_owned()
}

async fn accept_invite(t: &TestApp, access: &str, invite_id: &str) {
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/groups/invites/{invite_id}/accept"),
        Some(access),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn group_with_members(
    t: &TestApp,
    owner_access: &str,
    members: &[(&str, &str)], // (username, access)
) -> i64 {
    let conversation_id = create_group(t, owner_access, "文件群").await;
    for (name, access) in members {
        let invite_id = invite_ok(t, owner_access, conversation_id, name).await;
        accept_invite(t, access, &invite_id).await;
    }
    conversation_id
}

/// Raw `POST /api/groups/{id}/files`.
async fn upload_group_file(
    app: &Router,
    access: Option<&str>,
    conversation_id: i64,
    content_type: Option<&str>,
    file_name: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/groups/{conversation_id}/files"));
    if let Some(token) = access {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    if let Some(name) = file_name {
        builder = builder.header("x-file-name", name);
    }
    let request = builder
        .body(Body::from(body))
        .expect("build upload request");
    let response = app.clone().oneshot(request).await.expect("oneshot upload");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read upload body")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

async fn list_group_files(app: &Router, access: &str, conversation_id: i64) -> (StatusCode, Value) {
    send_http(
        app,
        "GET",
        &format!("/api/groups/{conversation_id}/files"),
        Some(access),
        None,
    )
    .await
}

async fn download_group_file(
    app: &Router,
    access: Option<&str>,
    file_id: Uuid,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("/api/groups/files/{file_id}"));
    if let Some(token) = access {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).expect("build download request");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("oneshot download");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read download body")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

async fn delete_group_file(app: &Router, access: &str, file_id: Uuid) -> (StatusCode, Value) {
    send_http(
        app,
        "POST",
        &format!("/api/groups/files/{file_id}/delete"),
        Some(access),
        None,
    )
    .await
}

/// Directly inserts a group-file row (for expired/over-quota simulation).
async fn insert_file_row(
    t: &TestApp,
    conversation_id: i64,
    uploader_id: Uuid,
    name: &str,
    bytes: i64,
    expires_at: Option<OffsetDateTime>,
) -> Uuid {
    let id = Uuid::now_v7();
    let storage_path = t.media_dir.join(format!("nonexistent-{id}.bin"));
    sqlx::query(
        "INSERT INTO group_files \
         (id, conversation_id, uploader_id, name, mime, bytes, storage_path, expires_at) \
         VALUES ($1, $2, $3, $4, 'application/octet-stream', $5, $6, $7)",
    )
    .bind(id)
    .bind(conversation_id)
    .bind(uploader_id)
    .bind(name)
    .bind(bytes)
    .bind(storage_path.to_string_lossy().into_owned())
    .bind(expires_at)
    .execute(&t.pool)
    .await
    .expect("insert group_files row");
    id
}

async fn stored_path(t: &TestApp, file_id: Uuid) -> String {
    sqlx::query_scalar("SELECT storage_path FROM group_files WHERE id = $1")
        .bind(file_id)
        .fetch_one(&t.pool)
        .await
        .expect("storage_path")
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

fn expect_group_updated(frame: Frame, conversation_id: i64) {
    match frame.payload {
        Payload::GroupUpdated(GroupUpdated { conversation_id: got }) => {
            assert_eq!(got, conversation_id);
        }
        other => panic!("expected group.updated, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests: upload
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_member_decodes_cjk_name_and_requires_membership() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_owner_id, owner_access) = register_user(&t, "guf-a@example.com", "gufa").await;
    let (member_id, member_access) = register_user(&t, "guf-b@example.com", "gufb").await;
    let (_outsider_id, outsider_access) = register_user(&t, "guf-c@example.com", "gufc").await;
    let conversation_id =
        group_with_members(&t, &owner_access, &[("gufb", member_access.as_str())]).await;

    // The CJK name arrives percent-encoded because header values are Latin-1.
    let original = TINY_PDF.to_vec();
    let (status, body) = upload_group_file(
        &t.app,
        Some(&member_access),
        conversation_id,
        Some("application/pdf"),
        Some("%E6%96%87%E4%BB%B6.pdf"),
        original.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let file_id: Uuid = body["file_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(body["name"], json!("文件.pdf"), "name is percent-decoded");
    assert_eq!(body["mime"], json!("application/pdf"));
    assert_eq!(body["bytes"], json!(original.len()));
    assert!(body["expires_at"].is_null(), "under quota is permanent");
    assert!(body["created_at"].is_string());

    // Row + verbatim disk bytes + extension derived from the name.
    let (uploader, bytes, path): (Uuid, i64, String) =
        sqlx::query_as("SELECT uploader_id, bytes, storage_path FROM group_files WHERE id = $1")
            .bind(file_id)
            .fetch_one(&t.pool)
            .await
            .expect("row");
    assert_eq!(uploader, member_id);
    assert_eq!(bytes, original.len() as i64);
    assert_eq!(std::fs::read(&path).expect("stored file"), original);
    assert!(
        path.ends_with(".pdf"),
        "extension must be derived from the name: {path}"
    );

    // A missing Content-Type defaults to application/octet-stream.
    let (status, body) = upload_group_file(
        &t.app,
        Some(&member_access),
        conversation_id,
        None,
        Some("blob"),
        b"raw".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["mime"], json!("application/octet-stream"));
    assert_eq!(body["name"], json!("blob"));
    assert!(body["expires_at"].is_null());

    // Non-member: plain 404 (no oracle). Unauthenticated: 401.
    let (status, body) = upload_group_file(
        &t.app,
        Some(&outsider_access),
        conversation_id,
        None,
        Some("x.bin"),
        b"nope".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = upload_group_file(
        &t.app,
        None,
        conversation_id,
        None,
        None,
        b"x".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn oversize_upload_is_413_and_leaves_no_row() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_owner_id, owner_access) = register_user(&t, "goc-a@example.com", "goca").await;
    let conversation_id = create_group(&t, &owner_access, "超限组").await;

    let oversized = vec![0u8; TEST_GROUP_FILE_CAP as usize + 1];
    let (status, body) = upload_group_file(
        &t.app,
        Some(&owner_access),
        conversation_id,
        None,
        Some("big.bin"),
        oversized,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], json!("too_large"));

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM group_files")
        .fetch_one(&t.pool)
        .await
        .expect("count");
    assert_eq!(rows, 0, "rejected upload must leave no row");

    // Nothing was written under the group's storage dir either.
    let dir = t.media_dir.join("group-files").join(conversation_id.to_string());
    let files = std::fs::read_dir(&dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(files, 0, "rejected upload must leave no disk file");
}

// ---------------------------------------------------------------------------
// Tests: listing + usage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_reports_usage_excludes_expired_and_is_newest_first() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "glf-a@example.com", "glfa").await;
    let (member_id, member_access) = register_user(&t, "glf-b@example.com", "glfb").await;
    let (_outsider_id, outsider_access) = register_user(&t, "glf-c@example.com", "glfc").await;
    let conversation_id =
        group_with_members(&t, &owner_access, &[("glfb", member_access.as_str())]).await;

    // member uploads first (100 bytes), owner second (200 bytes).
    let (status, first) = upload_group_file(
        &t.app,
        Some(&member_access),
        conversation_id,
        Some("text/plain"),
        Some("first.txt"),
        vec![b'a'; 100],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, second) = upload_group_file(
        &t.app,
        Some(&owner_access),
        conversation_id,
        Some("text/plain"),
        Some("second.txt"),
        vec![b'b'; 200],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");

    let (status, body) = list_group_files(&t.app, &owner_access, conversation_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["usage_bytes"], json!(300));
    assert_eq!(body["quota_bytes"], json!(FREE_GROUP_BYTES));
    let files = body["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["file_id"], second["file_id"], "newest first");
    assert_eq!(files[1]["file_id"], first["file_id"]);
    assert_eq!(files[0]["uploader"]["username"], "glfa");
    assert_eq!(files[0]["uploader"]["display_name"], "glfa");
    assert_eq!(files[0]["uploader"]["user_id"], json!(owner_id));
    assert_eq!(files[1]["uploader"]["username"], "glfb");
    assert_eq!(files[1]["uploader"]["user_id"], json!(member_id));
    assert_eq!(files[0]["bytes"], json!(200));
    assert!(files[0]["created_at"].is_string());
    assert!(files[0]["expires_at"].is_null());

    // A directly-inserted EXPIRED row is excluded from both list and usage.
    insert_file_row(
        &t,
        conversation_id,
        owner_id,
        "expired.txt",
        999_999,
        Some(OffsetDateTime::now_utc() - time::Duration::days(1)),
    )
    .await;
    let (status, body) = list_group_files(&t.app, &member_access, conversation_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["usage_bytes"],
        json!(300),
        "expired bytes must not count toward usage"
    );
    assert_eq!(
        body["files"].as_array().unwrap().len(),
        2,
        "expired rows are hidden"
    );

    // Non-members cannot list (404, no oracle).
    let (status, _) = list_group_files(&t.app, &outsider_access, conversation_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Tests: quota pricing rule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn over_quota_upload_becomes_temporary_for_seven_days() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_owner_id, owner_access) = register_user(&t, "gqt-a@example.com", "gqta").await;
    let conversation_id = create_group(&t, &owner_access, "配额组").await;

    // Under quota: permanent.
    let (status, first) = upload_group_file(
        &t.app,
        Some(&owner_access),
        conversation_id,
        None,
        Some("permanent.bin"),
        vec![0u8; 64],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert!(first["expires_at"].is_null());
    let permanent_id: Uuid = first["file_id"].as_str().unwrap().parse().unwrap();

    // Push current usage to exactly the 1 GiB quota by editing the row's size
    // directly (no 1 GiB body needed).
    sqlx::query("UPDATE group_files SET bytes = $1 WHERE id = $2")
        .bind(FREE_GROUP_BYTES)
        .bind(permanent_id)
        .execute(&t.pool)
        .await
        .expect("inflate row");

    let before = OffsetDateTime::now_utc();
    let (status, temp) = upload_group_file(
        &t.app,
        Some(&owner_access),
        conversation_id,
        None,
        Some("temporary.bin"),
        vec![0u8; 32],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{temp}");
    let expires_raw = temp["expires_at"].as_str().expect("temporary expires_at");
    let expires_at = OffsetDateTime::parse(expires_raw, &Rfc3339).expect("rfc3339");
    let delta = expires_at - before;
    assert!(
        delta >= time::Duration::days(6) + time::Duration::hours(23)
            && delta <= time::Duration::days(7) + time::Duration::hours(1),
        "expected ~{} days, got {delta}",
        TEMPORARY_TTL_DAYS
    );

    // Usage now reflects the inflated permanent row.
    let (_, body) = list_group_files(&t.app, &owner_access, conversation_id).await;
    assert_eq!(body["usage_bytes"], json!(FREE_GROUP_BYTES + 32));
}

// ---------------------------------------------------------------------------
// Tests: download
// ---------------------------------------------------------------------------

#[tokio::test]
async fn download_member_gets_bytes_and_attachment_headers() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (owner_id, owner_access) = register_user(&t, "gdl-a@example.com", "gdla").await;
    let (_member_id, member_access) = register_user(&t, "gdl-b@example.com", "gdlb").await;
    let (_outsider_id, outsider_access) = register_user(&t, "gdl-c@example.com", "gdlc").await;
    let conversation_id =
        group_with_members(&t, &owner_access, &[("gdlb", member_access.as_str())]).await;

    let payload = b"exact download bytes".to_vec();
    let (status, body) = upload_group_file(
        &t.app,
        Some(&member_access),
        conversation_id,
        Some("application/pdf"),
        Some("%E6%96%87%E4%BB%B6.pdf"),
        payload.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let file_id: Uuid = body["file_id"].as_str().unwrap().parse().unwrap();

    // Member downloads the exact bytes with DB content-type + attachment name.
    let (status, headers, bytes) =
        download_group_file(&t.app, Some(&member_access), file_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/pdf")
    );
    let disposition = headers
        .get("content-disposition")
        .expect("content-disposition");
    let disposition = String::from_utf8_lossy(disposition.as_bytes());
    assert!(
        disposition.starts_with("attachment; filename=\""),
        "disposition: {disposition}"
    );
    // Headers are latin1: the CJK name must ride RFC 5987-encoded, not raw.
    assert!(
        disposition.contains("filename*=UTF-8''%E6%96%87%E4%BB%B6.pdf"),
        "disposition must carry the UTF-8 encoded name: {disposition}"
    );
    assert_eq!(bytes, payload, "served bytes are verbatim");

    // The owner (also a member) may download; a non-member gets 404.
    let (status, _, _) = download_group_file(&t.app, Some(&owner_access), file_id).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = download_group_file(&t.app, Some(&outsider_access), file_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = download_group_file(&t.app, None, file_id).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Expired rows 404 even for a member; unknown ids 404.
    let expired_id = insert_file_row(
        &t,
        conversation_id,
        owner_id,
        "gone.bin",
        10,
        Some(OffsetDateTime::now_utc() - time::Duration::seconds(1)),
    )
    .await;
    let (status, _, _) = download_group_file(&t.app, Some(&member_access), expired_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expired files are gone");
    let (status, _, _) = download_group_file(&t.app, Some(&member_access), Uuid::now_v7()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Tests: delete matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_uploader_and_owner_allowed_other_members_403() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_owner_id, owner_access) = register_user(&t, "gdd-a@example.com", "gdda").await;
    let (_member_a_id, member_a_access) = register_user(&t, "gdd-b@example.com", "gddb").await;
    let (member_b_id, member_b_access) = register_user(&t, "gdd-c@example.com", "gddc").await;
    let (_outsider_id, outsider_access) = register_user(&t, "gdd-d@example.com", "gddd").await;
    let conversation_id = group_with_members(
        &t,
        &owner_access,
        &[
            ("gddb", member_a_access.as_str()),
            ("gddc", member_b_access.as_str()),
        ],
    )
    .await;

    // Member A uploads; member B may not delete it (403), outsider 404.
    let (status, body) = upload_group_file(
        &t.app,
        Some(&member_a_access),
        conversation_id,
        None,
        Some("a.bin"),
        b"a".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let file_id: Uuid = body["file_id"].as_str().unwrap().parse().unwrap();
    let path = stored_path(&t, file_id).await;
    assert!(std::path::Path::new(&path).exists(), "disk file exists");

    let (status, body) = delete_group_file(&t.app, &member_b_access, file_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = delete_group_file(&t.app, &outsider_access, file_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "non-member sees no oracle");

    // Uploader deletes → 204, row + disk file gone.
    let (status, _) = delete_group_file(&t.app, &member_a_access, file_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM group_files WHERE id = $1")
        .bind(file_id)
        .fetch_one(&t.pool)
        .await
        .expect("count");
    assert_eq!(rows, 0, "row deleted");
    assert!(!std::path::Path::new(&path).exists(), "disk file removed");

    // The OWNER may delete another member's file.
    let (status, body) = upload_group_file(
        &t.app,
        Some(&member_a_access),
        conversation_id,
        None,
        Some("b.bin"),
        b"b".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let file_id: Uuid = body["file_id"].as_str().unwrap().parse().unwrap();
    let path = stored_path(&t, file_id).await;
    let (status, _) = delete_group_file(&t.app, &owner_access, file_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!std::path::Path::new(&path).exists(), "owner deleted bytes");

    // An ADMIN may also delete another member's file.
    let (status, body) = upload_group_file(
        &t.app,
        Some(&member_a_access),
        conversation_id,
        None,
        Some("c.bin"),
        b"c".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let file_id: Uuid = body["file_id"].as_str().unwrap().parse().unwrap();
    let (status, body) = send_http(
        &t.app,
        "POST",
        &format!("/api/groups/{conversation_id}/members/{member_b_id}/role"),
        Some(&owner_access),
        Some(json!({ "role": "admin" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = delete_group_file(&t.app, &member_b_access, file_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "admin may delete");

    // Unknown id 404.
    let (status, _) = delete_group_file(&t.app, &owner_access, Uuid::now_v7()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Tests: live `group.updated`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_and_delete_broadcast_group_updated_to_members() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_owner_id, owner_access) = register_user(&t, "gwu-a@example.com", "gwua").await;
    let (_member_id, member_access) = register_user(&t, "gwu-b@example.com", "gwub").await;
    let conversation_id =
        group_with_members(&t, &owner_access, &[("gwub", member_access.as_str())]).await;

    let mut ws_owner = ws_connect(&t, &owner_access).await;
    let mut ws_member = ws_connect(&t, &member_access).await;

    // Member uploads → BOTH members receive group.updated.
    let (status, body) = upload_group_file(
        &t.app,
        Some(&member_access),
        conversation_id,
        None,
        Some("notify.bin"),
        b"n".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let file_id: Uuid = body["file_id"].as_str().unwrap().parse().unwrap();
    expect_group_updated(ws_next_frame(&mut ws_owner).await, conversation_id);
    expect_group_updated(ws_next_frame(&mut ws_member).await, conversation_id);

    // Delete → BOTH members receive group.updated again.
    let (status, _) = delete_group_file(&t.app, &member_access, file_id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    expect_group_updated(ws_next_frame(&mut ws_owner).await, conversation_id);
    expect_group_updated(ws_next_frame(&mut ws_member).await, conversation_id);
}
