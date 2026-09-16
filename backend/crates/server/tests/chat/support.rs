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
    AuthSession, ClientEnvelope, ClientEvent, ConversationSummary, MessageAck, MessageRejected,
    NewMessage, SendMessage, ServerEnvelope, ServerEvent,
};
use jiuyue_realtime::RealtimeHub;
use jiuyue_server::{AppState, Config, Services, app};
use jiuyue_store::Store;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor as _, PgPool};
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
        let realtime = Arc::new(RealtimeHub::new(Arc::clone(&chat)));

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
