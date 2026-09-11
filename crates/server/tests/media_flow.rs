//! M8 media integration tests: raw-body upload (sniffing, caps), public
//! Range-capable serving, and the media-carrying WS message pipeline.
//!
//! Harness mirrors `chat_flow.rs`: a global mutex + one-time schema reset +
//! per-test TRUNCATE, plus the shared cross-binary PG advisory lock so the
//! integration binaries never reset each other's schema. Uploads are pointed
//! at a per-run temp directory (no env mutation), and every received wire
//! frame is decoded into `jiuyue_protocol::Frame` so assertions pin the real
//! protocol shapes.

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use jiuyue_protocol::{
    ErrorCode, Frame, MediaRef, MsgAck, MsgNew, MsgRecall, MsgRecalled, MsgSend, PROTOCOL_VERSION,
    Payload, SyncCursor, SyncReq,
};
use jiuyue_server::crypto::BodyCipher;
use jiuyue_server::state::AppState;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection, PgPool};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use time::OffsetDateTime;
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

/// PNG magic header + filler — the server only needs the 8 magic bytes to
/// sniff, so a synthetic body is enough (no decode is attempted, per the
/// "store verbatim" contract).
const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

fn png_bytes(len: usize) -> Vec<u8> {
    let mut out = PNG_MAGIC.to_vec();
    out.resize(len.max(PNG_MAGIC.len()), 0xAB);
    out
}

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
         conversations, conversation_members, messages, media RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate tables");
    let redis_client = redis::Client::open(env_var("REDIS_URL").as_str()).expect("parse redis url");
    let redis = redis::aio::ConnectionManager::new(redis_client)
        .await
        .expect("connect redis");
    let mut state = AppState::new(pool.clone(), redis.clone(), env_var("JIUYUE_JWT_SECRET"));

    // Point uploads at a throwaway dir by overriding the (public) state field
    // instead of mutating process-global env.
    let media_dir = std::env::temp_dir().join(format!("jiuyue-media-test-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&media_dir).expect("create media temp dir");
    state.media_dir = Arc::new(media_dir.clone());

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

/// Raw `POST /api/media`.
async fn upload_media(
    app: &Router,
    access: Option<&str>,
    content_type: &str,
    file_name: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/media")
        .header("content-type", content_type);
    if let Some(token) = access {
        builder = builder.header("authorization", format!("Bearer {token}"));
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

async fn fetch_media(
    app: &Router,
    id: Uuid,
    range: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("/api/media/{id}"));
    if let Some(range) = range {
        builder = builder.header("range", range);
    }
    let request = builder.body(Body::empty()).expect("build get request");
    let response = app.clone().oneshot(request).await.expect("oneshot get");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read get body")
        .to_bytes();
    (status, headers, bytes.to_vec())
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

/// Uploads a valid PNG as `access` and returns `(media_id, byte_len)`.
async fn upload_png(t: &TestApp, access: &str, len: usize) -> (Uuid, usize) {
    let bytes = png_bytes(len);
    let (status, body) = upload_media(
        &t.app,
        Some(access),
        "image/png",
        Some("photo.png"),
        bytes.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let media_id: Uuid = body["media_id"]
        .as_str()
        .expect("media_id")
        .parse()
        .expect("uuid");
    assert_eq!(body["kind"], json!("image"));
    assert_eq!(body["mime"], json!("image/png"));
    assert_eq!(body["bytes"], json!(bytes.len()));
    assert_eq!(body["file_name"], json!("photo.png"));
    (media_id, bytes.len())
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

fn expect_error_code(frame: Frame, expected: ErrorCode) -> jiuyue_protocol::ErrorPayload {
    match frame.payload {
        Payload::Error(payload) => {
            assert_eq!(payload.code, expected, "error payload: {payload:?}");
            payload
        }
        other => panic!("expected error frame, got {other:?}"),
    }
}

/// File path the server should have used for `(created_at, mime)`.
async fn stored_path(t: &TestApp, media_id: Uuid) -> PathBuf {
    let (mime, created_at): (String, OffsetDateTime) =
        sqlx::query_as("SELECT mime, created_at FROM media WHERE id = $1")
            .bind(media_id)
            .fetch_one(&t.pool)
            .await
            .expect("media row");
    let ext = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        other => panic!("unexpected mime {other}"),
    };
    t.media_dir
        .join(format!("{:04}", created_at.year()))
        .join(format!("{:02}", u8::from(created_at.month())))
        .join(format!("{media_id}.{ext}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_png_stores_verbatim_file_and_returns_metadata() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "ump-a@example.com", "umpa").await;

    let original = png_bytes(1024);
    let (status, body) = upload_media(
        &t.app,
        Some(&access),
        "image/png",
        Some("dir/sub\\weird.png"),
        original.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let media_id: Uuid = body["media_id"].as_str().unwrap().parse().unwrap();
    // sanitizer strips path separators
    assert_eq!(body["file_name"], json!("dirsubweird.png"));

    let path = stored_path(&t, media_id).await;
    let stored = std::fs::read(&path).expect("stored file readable");
    assert_eq!(stored, original, "bytes must be stored verbatim");
}

#[tokio::test]
async fn upload_requires_bearer_auth() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (status, _) = upload_media(&t.app, None, "image/png", None, png_bytes(64)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn upload_with_lying_content_type_is_rejected_415() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "ump-b@example.com", "umpb").await;

    // PNG magic but declared as video.
    let (status, body) =
        upload_media(&t.app, Some(&access), "video/mp4", None, png_bytes(64)).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_eq!(body["error"], json!("unsupported_type"));

    // Unsupported declared type is also 415.
    let (status, body) = upload_media(
        &t.app,
        Some(&access),
        "application/pdf",
        None,
        b"%PDF-1.7".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_eq!(body["error"], json!("unsupported_type"));
}

#[tokio::test]
async fn oversize_image_is_rejected_413() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "ump-c@example.com", "umpc").await;

    // Default image cap is 15 MiB; one byte over must fail.
    let oversized = png_bytes(15 * 1024 * 1024 + 1);
    let (status, body) = upload_media(&t.app, Some(&access), "image/png", None, oversized).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], json!("too_large"));

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM media")
        .fetch_one(&t.pool)
        .await
        .expect("count media");
    assert_eq!(rows, 0, "rejected upload must leave no media row");
}

#[tokio::test]
async fn empty_upload_is_rejected_400() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "ump-d@example.com", "umpd").await;

    let (status, body) = upload_media(&t.app, Some(&access), "image/png", None, Vec::new()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], json!("bad_request"));
}

#[tokio::test]
async fn get_serves_bytes_with_db_content_type_and_cache_headers() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "ump-e@example.com", "umpe").await;
    let (media_id, _len) = upload_png(&t, &access, 512).await;

    let (status, headers, body) = fetch_media(&t.app, media_id, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    assert!(
        headers
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("inline; filename=")),
        "content-disposition must be inline"
    );
    assert_eq!(body, png_bytes(512), "served bytes are verbatim");
}

#[tokio::test]
async fn get_supports_single_byte_range() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "ump-f@example.com", "umpf").await;
    let (media_id, len) = upload_png(&t, &access, 300).await;

    let (status, headers, body) = fetch_media(&t.app, media_id, Some("bytes=0-3")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        headers.get("content-range").and_then(|v| v.to_str().ok()),
        Some(format!("bytes 0-3/{len}").as_str())
    );
    assert_eq!(
        headers.get("accept-ranges").and_then(|v| v.to_str().ok()),
        Some("bytes")
    );
    assert_eq!(body, png_bytes(300)[0..4].to_vec());
}

#[tokio::test]
async fn get_unknown_id_is_404() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (status, _headers, _body) = fetch_media(&t.app, Uuid::now_v7(), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn msg_send_with_owned_media_fans_out_msg_new_carrying_media() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (a_id, a_access) = register_user(&t, "med-a@example.com", "meda").await;
    let (_b_id, b_access) = register_user(&t, "med-b@example.com", "medb").await;
    let conversation_id = create_conversation(&t, &a_access, "medb").await;
    let (media_id, len) = upload_png(&t, &a_access, 777).await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;

    let client_msg_id = Uuid::now_v7();
    let media = MediaRef {
        media_id,
        kind: "image".to_owned(),
        mime: "image/png".to_owned(),
        bytes: len as i64,
        file_name: "photo.png".to_owned(),
        width: Some(64),
        height: Some(48),
        duration_ms: None,
    };
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id,
            body: String::new(),
            reply_to: None,
            media: Some(media.clone()),
            forward_of_message_id: None,
        }),
    };
    ws_send_text(&mut ws_a, &serde_json::to_string(&frame).unwrap()).await;

    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(MsgAck {
            client_msg_id: got, ..
        }) => assert_eq!(got, client_msg_id),
        other => panic!("expected msg.ack, got {other:?}"),
    }

    match ws_next_frame(&mut ws_b).await.payload {
        Payload::MsgNew(MsgNew {
            sender_id,
            body,
            media: got_media,
            ..
        }) => {
            assert_eq!(sender_id, a_id);
            assert_eq!(body, "", "media message body must be empty");
            let got = got_media.expect("msg.new must carry the media object");
            assert_eq!(got.media_id, media_id);
            assert_eq!(got.kind, "image");
            assert_eq!(got.mime, "image/png");
            assert_eq!(got.bytes, len as i64);
            assert_eq!(got.file_name, "photo.png");
            assert_eq!(got.width, Some(64), "client width hint is preserved");
            assert_eq!(got.height, Some(48));
        }
        other => panic!("expected msg.new, got {other:?}"),
    }

    // Stored row uses the media kind and the envelope at rest.
    let (kind, body_enc): (String, Vec<u8>) =
        sqlx::query_as("SELECT kind, body_enc FROM messages WHERE conversation_id = $1")
            .bind(conversation_id)
            .fetch_one(&t.pool)
            .await
            .expect("stored message");
    assert_eq!(kind, "image", "messages.kind mirrors media.kind");
    let cipher = BodyCipher::new(&env_var("JIUYUE_MASTER_KEY"));
    let envelope = cipher.decrypt(&body_enc).expect("decrypt media envelope");
    let stored: MediaRef = serde_json::from_str(&envelope).expect("envelope is a MediaRef");
    assert_eq!(stored.media_id, media_id);
}

#[tokio::test]
async fn msg_send_with_another_users_media_is_rejected() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "med-c@example.com", "medc").await;
    let (_c_id, c_access) = register_user(&t, "med-d@example.com", "medd").await;
    let (_d_id, _d_access) = register_user(&t, "med-e@example.com", "mede").await;

    // A owns the media; C tries to send it in C's own conversation with D.
    let (media_id, len) = upload_png(&t, &a_access, 256).await;
    let conversation_id = create_conversation(&t, &c_access, "mede").await;

    let mut ws_c = ws_connect(&t, &c_access).await;
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: String::new(),
            reply_to: None,
            media: Some(MediaRef {
                media_id,
                kind: "image".to_owned(),
                mime: "image/png".to_owned(),
                bytes: len as i64,
                file_name: "photo.png".to_owned(),
                width: None,
                height: None,
                duration_ms: None,
            }),
            forward_of_message_id: None,
        }),
    };
    ws_send_text(&mut ws_c, &serde_json::to_string(&frame).unwrap()).await;

    let err = expect_error_code(ws_next_frame(&mut ws_c).await, ErrorCode::BadRequest);
    assert!(
        err.message.contains("media"),
        "rejection should name the media field: {}",
        err.message
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id = $1")
        .bind(conversation_id)
        .fetch_one(&t.pool)
        .await
        .expect("count messages");
    assert_eq!(rows, 0, "rejected send must persist nothing");
}

#[tokio::test]
async fn sync_req_replays_media_message_like_live_fanout() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "med-f@example.com", "medf").await;
    let (_b_id, b_access) = register_user(&t, "med-g@example.com", "medg").await;
    let conversation_id = create_conversation(&t, &a_access, "medg").await;
    let (media_id, len) = upload_png(&t, &a_access, 999).await;

    // A sends the media message and acks.
    let mut ws_a = ws_connect(&t, &a_access).await;
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: String::new(),
            reply_to: None,
            media: Some(MediaRef {
                media_id,
                kind: "image".to_owned(),
                mime: "image/png".to_owned(),
                bytes: len as i64,
                file_name: "photo.png".to_owned(),
                width: Some(12),
                height: Some(34),
                duration_ms: None,
            }),
            forward_of_message_id: None,
        }),
    };
    ws_send_text(&mut ws_a, &serde_json::to_string(&frame).unwrap()).await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(_) => {}
        other => panic!("expected ack, got {other:?}"),
    }
    drop(ws_a);

    // B opens a fresh connection and catches up from seq 0.
    let mut ws_b = ws_connect(&t, &b_access).await;
    let sync = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::SyncReq(SyncReq {
            cursors: vec![SyncCursor {
                conversation_id,
                last_delivered_seq: 0,
            }],
        }),
    };
    ws_send_text(&mut ws_b, &serde_json::to_string(&sync).unwrap()).await;

    match ws_next_frame(&mut ws_b).await.payload {
        Payload::SyncRes(res) => {
            assert!(
                !res.messages.is_empty(),
                "sync must replay the media message"
            );
            let entry = res
                .messages
                .iter()
                .find_map(|message| match message {
                    jiuyue_protocol::SyncMessage::Plain(msg) => Some(msg),
                    jiuyue_protocol::SyncMessage::Encrypted(_) => None,
                })
                .expect("a plain msg.new entry");
            assert_eq!(entry.body, "", "replayed media body must be empty");
            let media = entry.media.as_ref().expect("sync entry carries media");
            assert_eq!(media.media_id, media_id);
            assert_eq!(media.kind, "image");
            assert_eq!(media.mime, "image/png");
            assert_eq!(media.bytes, len as i64);
            assert_eq!(media.width, Some(12));
            assert_eq!(media.height, Some(34));
        }
        other => panic!("expected sync.res, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// M9a: voice/audio + server-side forwarding
// ---------------------------------------------------------------------------

/// EBML (webm) magic + filler; the server only sniffs the container family.
fn audio_webm_bytes(len: usize) -> Vec<u8> {
    let mut out = vec![0x1A, 0x45, 0xDF, 0xA3];
    out.resize(len.max(4), 0xCD);
    out
}

/// Ogg-container magic (`OggS`) + filler.
fn ogg_bytes(len: usize) -> Vec<u8> {
    let mut out = b"OggS".to_vec();
    out.resize(len.max(4), 0xEF);
    out
}

/// ISO-BMFF `ftyp` box carrying an audio-ish brand.
fn m4a_bytes() -> Vec<u8> {
    let mut out = vec![0, 0, 0, 0x18];
    out.extend_from_slice(b"ftyp");
    out.extend_from_slice(b"M4A ");
    out.extend_from_slice(&[0, 0, 0, 0]);
    out
}

/// Uploads audio bytes as `access`; returns `(media_id, byte_len)`.
async fn upload_audio(
    t: &TestApp,
    access: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> (Uuid, usize) {
    let (status, body) = upload_media(
        &t.app,
        Some(access),
        content_type,
        Some("voice.webm"),
        bytes.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], json!("audio"));
    assert_eq!(body["mime"], json!(content_type));
    assert_eq!(body["bytes"], json!(bytes.len()));
    let media_id: Uuid = body["media_id"].as_str().unwrap().parse().unwrap();
    (media_id, bytes.len())
}

/// Sends a plain text `msg.send` and returns the resulting message id (from
/// the ack), consuming exactly one frame.
async fn send_text_and_ack(
    ws: &mut WsClient,
    conversation_id: i64,
    client_msg_id: Uuid,
    body: &str,
) -> Uuid {
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
        Payload::MsgAck(MsgAck { message_id, .. }) => message_id,
        other => panic!("expected msg.ack, got {other:?}"),
    }
}

#[tokio::test]
async fn forward_text_copies_body_and_attributes_original_author() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "fwd-t-a@example.com", "fwdta").await;
    let (b_id, b_access) = register_user(&t, "fwd-t-b@example.com", "fwdtdb").await;
    let (_c_id, c_access) = register_user(&t, "fwd-t-c@example.com", "fwdtdc").await;
    let conv1 = create_conversation(&t, &a_access, "fwdtdb").await;
    let conv2 = create_conversation(&t, &b_access, "fwdtdc").await;

    // A writes m1 in conv1 (A + B share it).
    let mut ws_a = ws_connect(&t, &a_access).await;
    let m1_id = send_text_and_ack(&mut ws_a, conv1, Uuid::now_v7(), "forward-me").await;
    drop(ws_a);

    // B forwards m1 into conv2 (B + C) with an empty body.
    let mut ws_b = ws_connect(&t, &b_access).await;
    let mut ws_c = ws_connect(&t, &c_access).await;
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id: conv2,
            client_msg_id: Uuid::now_v7(),
            body: String::new(),
            reply_to: None,
            media: None,
            forward_of_message_id: Some(m1_id),
        }),
    };
    ws_send_text(&mut ws_b, &serde_json::to_string(&frame).unwrap()).await;
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::MsgAck(_) => {}
        other => panic!("expected ack, got {other:?}"),
    }

    match ws_next_frame(&mut ws_c).await.payload {
        Payload::MsgNew(MsgNew {
            sender_id,
            body,
            forwarded_from_username,
            media,
            ..
        }) => {
            assert_eq!(sender_id, b_id, "the new message's sender is the forwarder");
            assert_eq!(body, "forward-me", "forwarded text body is copied verbatim");
            assert_eq!(
                forwarded_from_username.as_deref(),
                Some("fwdta"),
                "attribution names the ORIGINAL author, not the forwarder"
            );
            assert!(media.is_none());
        }
        other => panic!("expected msg.new, got {other:?}"),
    }

    let (kind, stored): (String, Option<String>) = sqlx::query_as(
        "SELECT kind, forwarded_from_username FROM messages WHERE conversation_id = $1",
    )
    .bind(conv2)
    .fetch_one(&t.pool)
    .await
    .expect("forwarded row");
    assert_eq!(kind, "text");
    assert_eq!(stored.as_deref(), Some("fwdta"));
}

#[tokio::test]
async fn forward_media_cross_user_copies_attachment_and_attribution() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "fwd-m-a@example.com", "fwdma").await;
    let (b_id, b_access) = register_user(&t, "fwd-m-b@example.com", "fwdmb").await;
    let (_c_id, c_access) = register_user(&t, "fwd-m-c@example.com", "fwdmc").await;
    let conv1 = create_conversation(&t, &a_access, "fwdmb").await;
    let conv2 = create_conversation(&t, &b_access, "fwdmc").await;
    let (media_id, len) = upload_png(&t, &a_access, 4242).await;

    // A sends the image into conv1.
    let mut ws_a = ws_connect(&t, &a_access).await;
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id: conv1,
            client_msg_id: Uuid::now_v7(),
            body: String::new(),
            reply_to: None,
            media: Some(MediaRef {
                media_id,
                kind: "image".to_owned(),
                mime: "image/png".to_owned(),
                bytes: len as i64,
                file_name: "photo.png".to_owned(),
                width: Some(5),
                height: Some(6),
                duration_ms: None,
            }),
            forward_of_message_id: None,
        }),
    };
    ws_send_text(&mut ws_a, &serde_json::to_string(&frame).unwrap()).await;
    let m1_id = match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(MsgAck { message_id, .. }) => message_id,
        other => panic!("expected ack, got {other:?}"),
    };
    drop(ws_a);

    // B forwards the media message WITHOUT a media field (server copies it).
    let mut ws_b = ws_connect(&t, &b_access).await;
    let mut ws_c = ws_connect(&t, &c_access).await;
    let forward = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id: conv2,
            client_msg_id: Uuid::now_v7(),
            body: String::new(),
            reply_to: None,
            media: None,
            forward_of_message_id: Some(m1_id),
        }),
    };
    ws_send_text(&mut ws_b, &serde_json::to_string(&forward).unwrap()).await;
    match ws_next_frame(&mut ws_b).await.payload {
        Payload::MsgAck(_) => {}
        other => panic!("expected ack, got {other:?}"),
    }

    match ws_next_frame(&mut ws_c).await.payload {
        Payload::MsgNew(MsgNew {
            sender_id,
            body,
            forwarded_from_username,
            media,
            ..
        }) => {
            assert_eq!(sender_id, b_id);
            assert_eq!(body, "", "forwarded media fanout body is empty");
            assert_eq!(forwarded_from_username.as_deref(), Some("fwdma"));
            let got = media.expect("forwarded media must carry the MediaRef");
            assert_eq!(got.media_id, media_id);
            assert_eq!(got.kind, "image");
            assert_eq!(got.mime, "image/png");
            assert_eq!(got.bytes, len as i64);
            assert_eq!(got.width, Some(5), "source width hint is copied");
            assert_eq!(got.height, Some(6));
        }
        other => panic!("expected msg.new, got {other:?}"),
    }

    let kind: String = sqlx::query_scalar("SELECT kind FROM messages WHERE conversation_id = $1")
        .bind(conv2)
        .fetch_one(&t.pool)
        .await
        .expect("forwarded media row");
    assert_eq!(kind, "image");
}

#[tokio::test]
async fn forward_recalled_source_is_rejected() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "fwd-r-a@example.com", "fwdra").await;
    let (_b_id, b_access) = register_user(&t, "fwd-r-b@example.com", "fwdrb").await;
    let (_c_id, _c_access) = register_user(&t, "fwd-r-c@example.com", "fwdrbc").await;
    let conv1 = create_conversation(&t, &a_access, "fwdrb").await;
    let conv2 = create_conversation(&t, &b_access, "fwdrbc").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let m1_id = send_text_and_ack(&mut ws_a, conv1, Uuid::now_v7(), "regret").await;
    let recall = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgRecall(MsgRecall {
            conversation_id: conv1,
            message_id: m1_id,
        }),
    };
    ws_send_text(&mut ws_a, &serde_json::to_string(&recall).unwrap()).await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgRecalled(MsgRecalled { message_id, .. }) => assert_eq!(message_id, m1_id),
        other => panic!("expected msg.recalled, got {other:?}"),
    }
    drop(ws_a);

    // B (a member of conv1) may not forward a recalled source.
    let mut ws_b = ws_connect(&t, &b_access).await;
    let forward = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id: conv2,
            client_msg_id: Uuid::now_v7(),
            body: String::new(),
            reply_to: None,
            media: None,
            forward_of_message_id: Some(m1_id),
        }),
    };
    ws_send_text(&mut ws_b, &serde_json::to_string(&forward).unwrap()).await;
    let err = expect_error_code(ws_next_frame(&mut ws_b).await, ErrorCode::BadRequest);
    assert!(
        err.message.contains("forward_source_invalid"),
        "recalled source must be refused: {}",
        err.message
    );
}

#[tokio::test]
async fn forward_source_outside_requester_membership_is_rejected() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_a_id, a_access) = register_user(&t, "fwd-x-a@example.com", "fwdxa").await;
    let (_b_id, _b_access) = register_user(&t, "fwd-x-b@example.com", "fwdxb").await;
    let (_c_id, c_access) = register_user(&t, "fwd-x-c@example.com", "fwdxc").await;
    let (_d_id, _d_access) = register_user(&t, "fwd-x-d@example.com", "fwdxd").await;
    // conv1: A + B (C is NOT a member).
    let conv1 = create_conversation(&t, &a_access, "fwdxb").await;
    // conv2: C + D (C's own conversation).
    let conv2 = create_conversation(&t, &c_access, "fwdxd").await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let m1_id = send_text_and_ack(&mut ws_a, conv1, Uuid::now_v7(), "private").await;
    drop(ws_a);

    // C, a stranger to conv1, tries to forward m1 into C's own conversation.
    let mut ws_c = ws_connect(&t, &c_access).await;
    let forward = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id: conv2,
            client_msg_id: Uuid::now_v7(),
            body: String::new(),
            reply_to: None,
            media: None,
            forward_of_message_id: Some(m1_id),
        }),
    };
    ws_send_text(&mut ws_c, &serde_json::to_string(&forward).unwrap()).await;
    let err = expect_error_code(ws_next_frame(&mut ws_c).await, ErrorCode::BadRequest);
    assert!(
        err.message.contains("forward_source_invalid"),
        "non-member source must be refused: {}",
        err.message
    );
}

#[tokio::test]
async fn audio_upload_accepts_supported_families_and_rejects_mismatches() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (_id, access) = register_user(&t, "aud-a@example.com", "auda").await;

    // EBML (webm) family → audio/webm.
    let (media_id, len) = upload_audio(&t, &access, "audio/webm", audio_webm_bytes(2048)).await;
    assert!(len >= 2048);
    assert!(media_id != Uuid::nil());

    // Ogg family → audio/ogg.
    upload_audio(&t, &access, "audio/ogg", ogg_bytes(777)).await;

    // ISO-BMFF family → audio/mp4.
    upload_audio(&t, &access, "audio/mp4", m4a_bytes()).await;

    // Wrong magic (PNG bytes declared audio/webm) → 415.
    let (status, body) =
        upload_media(&t.app, Some(&access), "audio/webm", None, png_bytes(64)).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_eq!(body["error"], json!("unsupported_type"));

    // Family disagreement (EBML bytes declared audio/ogg) → 415.
    let (status, body) = upload_media(
        &t.app,
        Some(&access),
        "audio/ogg",
        None,
        audio_webm_bytes(128),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
}

#[tokio::test]
async fn audio_message_fans_out_and_syncs_with_duration_ms() {
    let _guard = GATE.lock().await;
    let t = test_app().await;
    let (a_id, a_access) = register_user(&t, "aud-msg-a@example.com", "audmsga").await;
    let (_b_id, b_access) = register_user(&t, "aud-msg-b@example.com", "audmsgb").await;
    let conversation_id = create_conversation(&t, &a_access, "audmsgb").await;
    let (media_id, len) = upload_audio(&t, &a_access, "audio/webm", audio_webm_bytes(4096)).await;

    let mut ws_a = ws_connect(&t, &a_access).await;
    let mut ws_b = ws_connect(&t, &b_access).await;
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgSend(MsgSend {
            conversation_id,
            client_msg_id: Uuid::now_v7(),
            body: String::new(),
            reply_to: None,
            media: Some(MediaRef {
                media_id,
                kind: "audio".to_owned(),
                mime: "audio/webm".to_owned(),
                bytes: len as i64,
                file_name: "voice.webm".to_owned(),
                width: None,
                height: None,
                duration_ms: Some(4200),
            }),
            forward_of_message_id: None,
        }),
    };
    ws_send_text(&mut ws_a, &serde_json::to_string(&frame).unwrap()).await;
    match ws_next_frame(&mut ws_a).await.payload {
        Payload::MsgAck(_) => {}
        other => panic!("expected ack, got {other:?}"),
    }

    match ws_next_frame(&mut ws_b).await.payload {
        Payload::MsgNew(MsgNew {
            sender_id,
            body,
            media,
            ..
        }) => {
            assert_eq!(sender_id, a_id);
            assert_eq!(body, "", "audio message body must be empty");
            let got = media.expect("audio msg.new must carry the media object");
            assert_eq!(got.media_id, media_id);
            assert_eq!(got.kind, "audio");
            assert_eq!(got.mime, "audio/webm");
            assert_eq!(got.duration_ms, Some(4200), "duration hint is preserved");
        }
        other => panic!("expected msg.new, got {other:?}"),
    }
    drop(ws_a);
    drop(ws_b);

    // Fresh replay must produce the identical audio attachment.
    let mut ws_b2 = ws_connect(&t, &b_access).await;
    let sync = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::SyncReq(SyncReq {
            cursors: vec![SyncCursor {
                conversation_id,
                last_delivered_seq: 0,
            }],
        }),
    };
    ws_send_text(&mut ws_b2, &serde_json::to_string(&sync).unwrap()).await;
    match ws_next_frame(&mut ws_b2).await.payload {
        Payload::SyncRes(res) => {
            let entry = res
                .messages
                .iter()
                .find_map(|message| match message {
                    jiuyue_protocol::SyncMessage::Plain(msg) => Some(msg),
                    jiuyue_protocol::SyncMessage::Encrypted(_) => None,
                })
                .expect("a plain msg.new entry");
            assert_eq!(entry.body, "", "replayed audio body must be empty");
            let media = entry.media.as_ref().expect("sync entry carries media");
            assert_eq!(media.media_id, media_id);
            assert_eq!(media.kind, "audio");
            assert_eq!(media.mime, "audio/webm");
            assert_eq!(media.duration_ms, Some(4200));
        }
        other => panic!("expected sync.res, got {other:?}"),
    }
}
