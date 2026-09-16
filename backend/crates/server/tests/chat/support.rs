//! Shared harness for the chat integration tests: a real router over a throwaway
//! PostgreSQL schema, plus real WebSocket clients.
//!
//! The database is never mocked — Sequence Number allocation, the UNIQUE
//! constraints and the idempotent insert *are* the behaviour under test, and none
//! of them exist in a mock. Each test gets its own schema (selected through the
//! connection's `search_path`) migrated to latest, so tests run in parallel and
//! can be re-run with no manual cleanup.
//!
//! Without `TEST_DATABASE_URL` (or `DATABASE_URL`) the harness panics with
//! instructions — a silently skipped test would hide a broken delivery path,
//! which is worse than no test at all.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use jiuyue_auth::{AuthConfig, AuthService};
use jiuyue_chat::ChatService;
use jiuyue_contract::{
    AuthSession, ClientEnvelope, ClientEvent, ConversationSummary, MarkRead, MessageAck,
    MessageList, MessageRejected, MessageView, NewMessage, ReadMarker, ReadReceipt, Resume, Resync,
    SendMessage, ServerEnvelope, ServerEvent, SyncCursor, SyncState,
};
use jiuyue_realtime::{HEARTBEAT_INTERVAL, RealtimeHub};
use jiuyue_server::{AppState, Config, Services, app};
use jiuyue_store::Store;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor as _, PgPool, Row};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tower::ServiceExt;

/// Signing secret used by the test router. Long enough for the 32-byte minimum.
pub const TEST_SECRET: &str = "test-signing-secret-0123456789abcdef";

/// How long a test waits for one socket event before failing.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected test WebSocket.
pub type TestSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A running server under test: the router plus the schema it writes into.
pub struct TestApp {
    state: AppState,
    store: Store,
    admin_url: String,
    schema: String,
}

impl TestApp {
    /// Create a uniquely named schema, migrate it, and build the real router.
    pub async fn start() -> Self {
        Self::start_with_heartbeat(HEARTBEAT_INTERVAL).await
    }

    /// Same, with an explicit heartbeat period.
    ///
    /// The heartbeat tests cannot wait the production 30 seconds, so they build
    /// the same app with a millisecond period. Nothing else differs: the real
    /// registry, the real replay buffer and the real database are all still used.
    pub async fn start_with_heartbeat(heartbeat: Duration) -> Self {
        // Surface the server's own error logs when a test fails; the first
        // initialisation wins and a second is a no-op.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
            )
            .try_init();

        let admin_url = database_url();
        let schema = unique_schema_name();

        admin_execute(&admin_url, &format!("CREATE SCHEMA \"{schema}\""))
            .await
            .expect("the test database must allow creating a schema");

        let store = Store::connect(&scoped_url(&admin_url, &schema))
            .await
            .expect("Store::connect must reach the test database");
        store
            .migrate()
            .await
            .expect("migrations must apply to an empty schema");

        let pool = store.pool().clone();
        let auth = AuthService::new(pool.clone(), AuthConfig::new(TEST_SECRET))
            .await
            .expect("the identity service must build with a valid secret");
        let chat = Arc::new(ChatService::new(pool));
        let realtime = Arc::new(RealtimeHub::with_settings(
            Arc::clone(&chat),
            heartbeat,
            jiuyue_realtime::DEFAULT_REPLAY_CAPACITY,
        ));

        let state = AppState::with_services(
            Config::default(),
            Services {
                auth: Arc::new(auth),
                chat,
                realtime,
            },
        );

        Self {
            state,
            store,
            admin_url,
            schema,
        }
    }

    /// A clone of the router, ready for one request.
    pub fn router(&self) -> Router {
        app(self.state.clone())
    }

    /// The pool writing into this test's schema, for direct assertions.
    pub fn pool(&self) -> &PgPool {
        self.store.pool()
    }

    /// Bind the real router to an ephemeral port and return the WebSocket URL.
    pub async fn serve_websocket(&self) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the listener must bind");
        let address = listener
            .local_addr()
            .expect("the listener must report its address");

        let router = self.router();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("the server must run");
        });

        (format!("ws://{address}/ws"), server)
    }

    /// Drop this test's schema. Best effort, like the store harness.
    pub async fn cleanup(self) {
        self.store.pool().close().await;
        let statement = format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema);
        let _ = admin_execute(&self.admin_url, &statement).await;
    }
}

/// Register successfully and return the parsed session.
pub async fn register(app: &TestApp, username: &str, email: &str, password: &str) -> AuthSession {
    let (status, body) = post_json(
        app,
        "/auth/register",
        &json!({ "username": username, "email": email, "password": password }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "registration must succeed, got {status}: {body}"
    );

    serde_json::from_value(body).expect("the registration response must match AuthSession")
}

/// Log in an existing account, opening a second Device for it.
///
/// A second login is a second `sessions` row — the Device identity a Sync Cursor is
/// scoped to — so this is how a test gets two Devices of one account.
pub async fn login(app: &TestApp, email: &str, password: &str) -> AuthSession {
    let (status, body) = post_json(
        app,
        "/auth/login",
        &json!({ "email": email, "password": password }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "login must succeed, got {status}: {body}"
    );

    serde_json::from_value(body).expect("the login response must match AuthSession")
}

/// The `sessions` id behind an access token, as `GET /auth/whoami` reports it.
pub async fn session_id(app: &TestApp, token: &str) -> String {
    let (status, body) = get_with_token(app, "/auth/whoami", token).await;
    assert_eq!(status, StatusCode::OK, "whoami must succeed: {body}");

    body["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("whoami must carry session_id: {body}"))
        .to_owned()
}

/// Open (or reopen) a Direct Conversation and return the summary.
pub async fn create_direct(app: &TestApp, token: &str, peer_username: &str) -> ConversationSummary {
    let (status, body) = post_json_with_token(
        app,
        "/conversations/direct",
        &json!({ "peer_username": peer_username }),
        token,
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "opening a direct conversation must succeed: {body}"
    );

    serde_json::from_value(body).expect("the response must match ConversationSummary")
}

/// A `SendMessage` client event for the given conversation.
pub fn send_message(conversation_id: &str, client_msg_id: &str, body: &str) -> ClientEvent {
    ClientEvent::SendMessage(SendMessage {
        conversation_id: conversation_id.to_owned(),
        client_msg_id: client_msg_id.to_owned(),
        body: body.to_owned(),
    })
}

/// A `Resume` wake-up handshake naming the highest connection sequence consumed.
///
/// `0` means "this is a first connection, I consumed nothing"; `connection_id` is
/// the id from a heartbeat of the connection the position belongs to, or `None`
/// when the client cannot name it (a first connect, or right after a reconnect).
pub fn resume(last_seq: u64, connection_id: Option<u64>) -> ClientEvent {
    ClientEvent::Resume(Resume {
        last_seq,
        connection_id,
    })
}

/// A `SyncCursor` report: "this Device has consumed up to `last_seq` here".
///
/// This is the client half of the per-Device Sync Cursor: the server persists it
/// (coalesced and checkpointed) so the Device can be told, on its next connect,
/// exactly what it missed.
pub fn sync_cursor(conversation_id: &str, last_seq: i64) -> ClientEvent {
    ClientEvent::SyncCursor(SyncCursor {
        conversation_id: conversation_id.to_owned(),
        last_seq,
    })
}

/// A `MarkRead` client event: "this User has read up to `last_seq` here".
///
/// This is the one client action behind both read concepts: it advances the
/// User's private Read Marker (which clears the Unread Count and echoes to the
/// account's other Devices) and their public Read Receipt (which is broadcast to
/// the other Participants).
pub fn mark_read(conversation_id: &str, last_seq: i64) -> ClientEvent {
    ClientEvent::MarkRead(MarkRead {
        conversation_id: conversation_id.to_owned(),
        last_read_seq: last_seq,
    })
}

/// Read the REST repair loop: every Message strictly after `cursor`, oldest first.
///
/// This is what the client does after a reconnect: pull forward pages until the
/// server reports no more, concatenating them. It returns the Messages in order,
/// exactly as a repaired client would hold them.
pub async fn repair_after(
    app: &TestApp,
    conversation_id: &str,
    mut cursor: i64,
    token: &str,
) -> Vec<MessageView> {
    let mut repaired = Vec::new();

    loop {
        let path = format!("/conversations/{conversation_id}/messages?after={cursor}&limit=100");
        let (status, body) = get_with_token(app, &path, token).await;
        assert_eq!(status, StatusCode::OK, "the repair page must load: {body}");

        let page: MessageList =
            serde_json::from_value(body).expect("the response must be MessageList");
        let has_more = page.has_more;
        let next_after = page.next_after;
        repaired.extend(page.messages);

        // Continue only when the server proved there is more *and* the cursor
        // actually advanced; a non-advancing cursor would loop forever.
        match (has_more, next_after) {
            (true, Some(next)) if next > cursor => cursor = next,
            _ => break,
        }
    }

    repaired
}

/// Connect a real WebSocket as the given access token.
pub async fn connect_socket(ws_url: &str, token: &str) -> TestSocket {
    let url = format!("{ws_url}?token={token}");
    let (socket, _response) = connect_async(url).await.expect("the client must connect");
    socket
}

/// Read the next envelope, including heartbeats.
///
/// Reading the opening heartbeat is how a test knows the server has finished
/// registering the connection, so a subsequent REST call can fan out to it.
pub async fn next_envelope(socket: &mut TestSocket) -> ServerEnvelope {
    let frame = tokio::time::timeout(EVENT_TIMEOUT, socket.next())
        .await
        .expect("an envelope must arrive before the timeout")
        .expect("the socket must stay open")
        .expect("the frame must be valid");

    match frame {
        Message::Text(text) => {
            serde_json::from_str(text.as_str()).expect("the frame must decode as an envelope")
        }
        other => panic!("expected a text frame, got {other:?}"),
    }
}

/// Read the next non-heartbeat event.
pub async fn next_event(socket: &mut TestSocket) -> ServerEvent {
    loop {
        let envelope = next_envelope(socket).await;
        match envelope.event().clone() {
            ServerEvent::Ping(_) => continue,
            event => return event,
        }
    }
}

/// Send one client event over the socket.
pub async fn send_event(socket: &mut TestSocket, event: ClientEvent) {
    let text = serde_json::to_string(&ClientEnvelope::new(event))
        .expect("a client envelope must serialise");
    socket
        .send(Message::Text(text.into()))
        .await
        .expect("the client must be able to send");
}

/// Collect `count` acknowledgements, ignoring anything else on the wire.
pub async fn collect_acks(socket: &mut TestSocket, count: usize) -> Vec<MessageAck> {
    let mut acks = Vec::with_capacity(count);
    while acks.len() < count {
        if let ServerEvent::MessageAck(ack) = next_event(socket).await {
            acks.push(ack);
        }
    }
    acks
}

/// Read the next non-heartbeat event, or `None` when none arrives in time.
///
/// Used to assert an *absence*: that a replay produced no second fan-out.
pub async fn next_event_within(socket: &mut TestSocket, timeout: Duration) -> Option<ServerEvent> {
    tokio::time::timeout(timeout, next_event(socket)).await.ok()
}

/// Read until the next acknowledgement arrives.
pub async fn expect_ack(socket: &mut TestSocket) -> MessageAck {
    loop {
        if let ServerEvent::MessageAck(ack) = next_event(socket).await {
            return ack;
        }
    }
}

/// Read until the next new-message event arrives.
pub async fn expect_new_message(socket: &mut TestSocket) -> NewMessage {
    loop {
        if let ServerEvent::NewMessage(message) = next_event(socket).await {
            return message;
        }
    }
}

/// Read until the next rejection arrives.
pub async fn expect_rejection(socket: &mut TestSocket) -> MessageRejected {
    loop {
        if let ServerEvent::MessageRejected(rejection) = next_event(socket).await {
            return rejection;
        }
    }
}

/// Read until the next resync answer arrives, ignoring chat events and heartbeats.
pub async fn expect_resync(socket: &mut TestSocket) -> Resync {
    loop {
        if let ServerEvent::Resync(resync) = next_event(socket).await {
            return resync;
        }
    }
}

/// Read until the Device's stored Sync Cursors arrive, ignoring everything else.
///
/// The server pushes `SyncState` once per connection, right after the opening
/// heartbeat, and only when the Device has stored cursors — so a test that gets
/// here has already proven the persistence round-tripped.
pub async fn expect_sync_state(socket: &mut TestSocket) -> SyncState {
    loop {
        if let ServerEvent::SyncState(state) = next_event(socket).await {
            return state;
        }
    }
}

/// Read until the next private Read Marker arrives, ignoring everything else.
pub async fn expect_read_marker(socket: &mut TestSocket) -> ReadMarker {
    loop {
        if let ServerEvent::ReadMarker(marker) = next_event(socket).await {
            return marker;
        }
    }
}

/// Read until the next public Read Receipt arrives, ignoring everything else.
pub async fn expect_read_receipt(socket: &mut TestSocket) -> ReadReceipt {
    loop {
        if let ServerEvent::ReadReceipt(receipt) = next_event(socket).await {
            return receipt;
        }
    }
}

/// Every non-heartbeat event a socket receives within `timeout`.
///
/// Used to assert an **absence**: the privacy test drains a peer's socket for a
/// window and then asserts no `ReadMarker` was among the events.
pub async fn collect_events_within(socket: &mut TestSocket, timeout: Duration) -> Vec<ServerEvent> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut events = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return events;
        }

        match next_event_within(socket, remaining).await {
            Some(event) => events.push(event),
            None => return events,
        }
    }
}

/// `POST` a JSON body and return the status and parsed body.
pub async fn post_json(app: &TestApp, path: &str, body: &Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(body).expect("a JSON value must serialise"),
        ))
        .expect("the request must build");

    send(app, request).await
}

/// `POST` a JSON body with a bearer token.
pub async fn post_json_with_token(
    app: &TestApp,
    path: &str,
    body: &Value,
    token: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(body).expect("a JSON value must serialise"),
        ))
        .expect("the request must build");

    send(app, request).await
}

/// `GET` with a bearer token.
pub async fn get_with_token(app: &TestApp, path: &str, token: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("the request must build");

    send(app, request).await
}

/// `GET` with no credentials at all.
pub async fn get_without_token(app: &TestApp, path: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .expect("the request must build");

    send(app, request).await
}

/// Drive one request through the real router and parse the JSON response.
async fn send(app: &TestApp, request: Request<Body>) -> (StatusCode, Value) {
    let response = app
        .router()
        .oneshot(request)
        .await
        .expect("the router must answer");

    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the response body must be readable");

    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("every response body must be JSON")
    };

    (status, body)
}

/// The `error.code` string from an [`jiuyue_contract::ErrorBody`]-shaped response.
pub fn error_code(body: &Value) -> &str {
    body["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("response is not an ErrorBody: {body}"))
}

/// How many rows a table holds in this test's schema.
pub async fn count_rows(pool: &PgPool, table: &str) -> i64 {
    // `table` is a hard-coded literal at every call site, never user input.
    let query = format!("SELECT COUNT(*) FROM {table}");

    sqlx::query_scalar(&query)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("counting `{table}` must succeed: {error}"))
}

/// Every Device's stored cursor for one Conversation, ordered by Device id.
///
/// Read straight from the schema, so it proves the value lives in PostgreSQL rather
/// than in a hub's memory. Ordered by the Device's ULID, which is time-sortable, so
/// "the first Device" is stable across calls.
pub async fn device_cursors_for_user(
    pool: &PgPool,
    user_id: &str,
    conversation_id: &str,
) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT sc.last_seq FROM sync_cursors AS sc \
         JOIN sessions AS s ON s.id = sc.session_id \
         WHERE s.user_id = $1 AND sc.conversation_id = $2 \
         ORDER BY s.id",
    )
    .bind(user_id)
    .bind(conversation_id)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|error| panic!("reading sync_cursors must succeed: {error}"))
}

/// Apply a delivery stream idempotently (keyed on Message ID) and assert the
/// result is exactly `seq 1..=count`, once each.
///
/// This is the client contract written as an assertion: delivery is at-least-once,
/// so a repair may re-deliver a Message the client already holds, and application
/// keys on Message ID. It returns the surviving ids for callers that also want to
/// assert the raw delivery shape.
pub fn assert_exact_sequence_set(messages: Vec<MessageView>, count: i64) -> BTreeSet<String> {
    let mut by_id: BTreeMap<String, MessageView> = BTreeMap::new();
    for message in messages {
        by_id.insert(message.id.clone(), message);
    }

    let mut merged: Vec<MessageView> = by_id.into_values().collect();
    merged.sort_by_key(|message| message.seq);

    let ids: BTreeSet<String> = merged.iter().map(|message| message.id.clone()).collect();
    assert_eq!(
        merged.len(),
        count as usize,
        "the client must hold exactly the {count} sent Messages"
    );
    assert_eq!(
        ids.len(),
        count as usize,
        "no Message may appear twice after idempotent application"
    );
    assert_eq!(
        merged.iter().map(|message| message.seq).collect::<Vec<_>>(),
        (1..=count).collect::<Vec<_>>(),
        "the repaired set must cover seq 1..={count} with no hole"
    );

    ids
}

/// One User's stored read state, read straight from the schema.
///
/// Returns `(read_marker_seq, read_receipt_seq, unread_count)`. Asserting from
/// PostgreSQL and not from a hub's memory is the point: the persistence *is* the
/// behaviour under test.
pub async fn member_read_state(
    pool: &PgPool,
    conversation_id: &str,
    user_id: &str,
) -> (i64, i64, i64) {
    let row = sqlx::query(
        "SELECT read_marker_seq, read_receipt_seq, unread_count \
         FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|error| panic!("reading conversation_members must succeed: {error}"));

    (
        row.get("read_marker_seq"),
        row.get("read_receipt_seq"),
        row.get("unread_count"),
    )
}

/// Just the stored Unread Count for one User in one Conversation.
pub async fn member_unread_count(pool: &PgPool, conversation_id: &str, user_id: &str) -> i64 {
    member_read_state(pool, conversation_id, user_id).await.2
}

/// Wait until a User's stored Unread Count is `expected`.
///
/// The count is committed with the Message insert and with the read update, so
/// this usually answers on the first poll; it exists so a test never races the
/// last write of a concurrent burst.
pub async fn wait_for_unread(pool: &PgPool, conversation_id: &str, user_id: &str, expected: i64) {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;

    loop {
        if member_unread_count(pool, conversation_id, user_id).await == expected {
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            panic!(
                "unread for {user_id} never reached {expected} within {EVENT_TIMEOUT:?}; \
                 stored: {:?}",
                member_read_state(pool, conversation_id, user_id).await
            );
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until some Device of `user_id` has exactly `expected` as its cursor.
///
/// The teardown checkpoint is asynchronous with respect to the client dropping its
/// socket: the server flushes after its frame loop ends. Polling the database is
/// how a test waits for that without racing a fixed sleep.
pub async fn wait_for_device_cursor(
    pool: &PgPool,
    user_id: &str,
    conversation_id: &str,
    expected: i64,
) {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;

    loop {
        if device_cursors_for_user(pool, user_id, conversation_id)
            .await
            .contains(&expected)
        {
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            panic!(
                "no Device cursor reached {expected} within {EVENT_TIMEOUT:?}; \
                 stored cursors: {:?}",
                device_cursors_for_user(pool, user_id, conversation_id).await
            );
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Panics with an actionable message when no database is configured.
fn database_url() -> String {
    env::var("TEST_DATABASE_URL")
        .ok()
        .or_else(|| env::var("DATABASE_URL").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            panic!(
                "jiuyue-server chat tests need a real PostgreSQL database.\n\
                 Set TEST_DATABASE_URL (preferred) or DATABASE_URL, for example:\n\
                 \x20   postgres://jiuyue:jiuyue_dev_password@localhost:5432/jiuyue_dev\n\
                 Tests are never silently skipped."
            )
        })
}

/// A process- and run-unique schema name, safe to interpolate as an identifier.
fn unique_schema_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);

    format!("chat_test_{}_{sequence}_{nanos}", std::process::id())
}

/// Point a connection at one schema through the `search_path` startup option.
fn scoped_url(base: &str, schema: &str) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}options[search_path]={schema}")
}

async fn admin_execute(admin_url: &str, statement: &str) -> Result<(), sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(admin_url)
        .await?;
    let result = pool.execute(statement).await;
    pool.close().await;
    result.map(|_| ())
}
