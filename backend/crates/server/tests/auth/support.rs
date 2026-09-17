//! Shared integration-test harness: a real router over a throwaway PostgreSQL schema.
//!
//! Each test gets its own schema (selected through the connection's `search_path`)
//! migrated to latest, so tests run in parallel and can be re-run with no manual
//! cleanup. Without `TEST_DATABASE_URL` (or `DATABASE_URL`) the harness panics
//! with instructions — a silently skipped test would hide a broken contract,
//! which is worse than no test at all.

use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use jiuyue_auth::oauth::{OAuthClient, OAuthConfig};
use jiuyue_auth::{
    AuthConfig, AuthService, InMemoryMailer, InProcessLoginAttemptStore, LoginAttemptPolicy,
    LoginAttemptPolicyBuilder, LoginAttemptStore, Mailer, OutgoingMessage,
};
use jiuyue_contract::auth::RequestAccepted;
use jiuyue_contract::{AuthSession, UserProfile};
use jiuyue_server::{AppState, Config, app};
use jiuyue_store::Store;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor as _, PgPool};
use tower::ServiceExt;

use super::oauth_support::{FakeOAuthClient, credentialed_providers};

/// Signing secret used by the test router. Long enough for the 32-byte minimum.
pub const TEST_SECRET: &str = "test-signing-secret-0123456789abcdef";

/// A running server under test: the router plus the schema it writes into.
pub struct TestApp {
    router: Router,
    store: Store,
    admin_url: String,
    schema: String,
    mailer: Arc<InMemoryMailer>,
    /// The fake provider transport. Every test app has one; the OAuth tests are
    /// the only ones that script it.
    oauth: Arc<FakeOAuthClient>,
}

impl TestApp {
    /// Create a uniquely named schema, migrate it, and build the real router.
    pub async fn start() -> Self {
        Self::start_with_login_policy(LoginAttemptPolicy::default())
            .await
            .0
    }

    /// Build the real router with a chosen login-limiting policy, and hand back
    /// the store the limiter actually uses.
    ///
    /// Tests override thresholds through [`login_policy`] instead of weakening
    /// [`LoginAttemptPolicy::default`], and they get the store so the memory
    /// ceiling can be asserted rather than assumed.
    pub async fn start_with_login_policy(
        policy: LoginAttemptPolicy,
    ) -> (Self, Arc<InProcessLoginAttemptStore>) {
        let (app, login, _email) =
            Self::start_with_policies(policy, LoginAttemptPolicy::default()).await;

        (app, login)
    }

    /// Build the real router with chosen login- and email-limiting policies.
    ///
    /// The email limiter guards the endpoints that trigger mail
    /// (`forgot-password`, `resend-verification`); it is a separate bucket from
    /// login, so tests configure it independently.
    pub async fn start_with_policies(
        login_policy: LoginAttemptPolicy,
        email_policy: LoginAttemptPolicy,
    ) -> (
        Self,
        Arc<InProcessLoginAttemptStore>,
        Arc<InProcessLoginAttemptStore>,
    ) {
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

        let login_store = Arc::new(InProcessLoginAttemptStore::new(login_policy));
        let email_store = Arc::new(InProcessLoginAttemptStore::new(email_policy));
        let mailer = Arc::new(InMemoryMailer::new());
        let oauth = Arc::new(FakeOAuthClient::new());

        let auth = AuthService::builder(store.pool().clone(), AuthConfig::new(TEST_SECRET))
            .login_store(Arc::clone(&login_store) as Arc<dyn LoginAttemptStore>)
            .email_store(Arc::clone(&email_store) as Arc<dyn LoginAttemptStore>)
            .mailer(Arc::clone(&mailer) as Arc<dyn Mailer>)
            .oauth(
                Arc::clone(&oauth) as Arc<dyn OAuthClient>,
                OAuthConfig::new(credentialed_providers()),
            )
            .build()
            .await
            .expect("the identity service must build with a valid secret");

        let router = app(AppState::with_auth(Config::default(), auth));

        (
            Self {
                router,
                store,
                admin_url,
                schema,
                mailer,
                oauth,
            },
            login_store,
            email_store,
        )
    }

    /// A clone of the router, ready for one request.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// The captured-mail transport, for asserting on what was "sent".
    pub fn mailer(&self) -> &InMemoryMailer {
        &self.mailer
    }

    /// The fake provider transport, for scripting what a provider answers.
    pub fn oauth(&self) -> &FakeOAuthClient {
        &self.oauth
    }

    /// The pool writing into this test's schema, for direct assertions.
    pub fn pool(&self) -> &PgPool {
        self.store.pool()
    }

    /// Drop this test's schema. Best effort, like the store harness.
    pub async fn cleanup(self) {
        self.store.pool().close().await;
        let statement = format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema);
        let _ = admin_execute(&self.admin_url, &statement).await;
    }
}

/// A registration body for the common fields.
pub fn register_body(username: &str, email: &str, password: &str) -> Value {
    json!({ "username": username, "email": email, "password": password })
}

/// A login body for the common fields.
pub fn login_body(email: &str, password: &str) -> Value {
    json!({ "email": email, "password": password })
}

/// A builder for test-sized login thresholds.
///
/// Production numbers stay in [`LoginAttemptPolicy::default`]; a test composes
/// only the knobs it needs to let a window elapse inside the test's lifetime.
pub fn login_policy() -> LoginAttemptPolicyBuilder {
    LoginAttemptPolicy::builder()
}

/// Register successfully and return the parsed session.
///
/// Panics on a non-201, because every caller depends on the account existing.
pub async fn register(app: &TestApp, username: &str, email: &str, password: &str) -> AuthSession {
    let (status, body) = post_json(
        app,
        "/auth/register",
        &register_body(username, email, password),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "registration must succeed, got {status}: {body}"
    );

    serde_json::from_value(body).expect("the registration response must match AuthSession")
}

/// `POST` a JSON body and return the status and parsed body.
pub async fn post_json(app: &TestApp, path: &str, body: &Value) -> (StatusCode, Value) {
    post_json_from(app, path, body, None).await
}

/// `POST` a JSON body from a named source.
///
/// The limiter counts attempts per source, and outside a test the source is the
/// client address the proxy appended to `X-Forwarded-For`. Naming it per request
/// is how one test acts as several clients through a single router.
pub async fn post_json_from(
    app: &TestApp,
    path: &str,
    body: &Value,
    source: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");

    if let Some(source) = source {
        builder = builder.header("x-forwarded-for", source);
    }

    let request = builder
        .body(Body::from(
            serde_json::to_vec(body).expect("a JSON value must serialise"),
        ))
        .expect("the request must build");

    send(app, request).await
}

/// Log in from a named source.
pub async fn login_from(
    app: &TestApp,
    email: &str,
    password: &str,
    source: &str,
) -> (StatusCode, Value) {
    post_json_from(
        app,
        "/auth/login",
        &login_body(email, password),
        Some(source),
    )
    .await
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

/// `POST` with a bearer token and no body.
pub async fn post_with_token(app: &TestApp, path: &str, token: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
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
    let (status, _, body) = send_full(app, request).await;

    (status, body)
}

/// Drive one request through the real router, keeping the response headers.
///
/// `Retry-After` is part of the throttling contract, so a test has to be able to
/// see it — the body alone would leave half the response unchecked.
pub async fn send_full(app: &TestApp, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = app
        .router()
        .oneshot(request)
        .await
        .expect("the router must answer");

    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the response body must be readable");

    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("every response body must be JSON")
    };

    (status, headers, body)
}

/// Drive a hand-built request and return its status and parsed body.
///
/// The OAuth journeys send bearer credentials on endpoints whose bodies are not
/// plain JSON, so they build the request themselves; this is the shared way to
/// send one and read the answer under test.
pub async fn send_full_body(
    app: &TestApp,
    request: Request<Body>,
) -> (StatusCode, HeaderMap, Value) {
    send_full(app, request).await
}

/// `POST` a JSON body from a source, keeping the response headers.
pub async fn post_json_from_full(
    app: &TestApp,
    path: &str,
    body: &Value,
    source: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json");

    if let Some(source) = source {
        builder = builder.header("x-forwarded-for", source);
    }

    let request = builder
        .body(Body::from(
            serde_json::to_vec(body).expect("a JSON value must serialise"),
        ))
        .expect("the request must build");

    send_full(app, request).await
}

/// The `error.code` string from an [`jiuyue_contract::ErrorBody`]-shaped response.
pub fn error_code(body: &Value) -> &str {
    body["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("response is not an ErrorBody: {body}"))
}

/// The `error.retry_after_seconds` hint, when the response carried one.
pub fn error_retry_after(body: &Value) -> Option<u64> {
    body["error"]["retry_after_seconds"].as_u64()
}

/// How long a test waits for a queued email to be captured.
pub const MAIL_TIMEOUT: Duration = Duration::from_secs(5);

/// Which of the two emails a test is waiting for.
///
/// The two are told apart by the path on their link, not by arrival order: a
/// registration and a reset can both be in flight for the same address, so "the
/// next message" is not a reliable answer and a link is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailKind {
    /// The address-verification link (`/verify-email?token=…`).
    Verification,
    /// The password-reset link (`/reset-password?token=…`).
    Reset,
}

impl MailKind {
    /// The route fragment that identifies this kind of link.
    fn link_path(self) -> &'static str {
        match self {
            Self::Verification => "/verify-email?token=",
            Self::Reset => "/reset-password?token=",
        }
    }
}

/// The token carried by the first link in a captured message.
pub fn token_from_message(message: &OutgoingMessage) -> String {
    let link = InMemoryMailer::first_link(message)
        .unwrap_or_else(|| panic!("the message carried no link: {message:?}"));

    link.split("token=")
        .nth(1)
        .unwrap_or_else(|| panic!("the link carried no token: {link}"))
        .to_owned()
}

/// Wait until `email` has received `count` messages of `kind`, then return the
/// token from the newest one.
///
/// Delivery happens on a spawned task, so this polls (bounded by [`MAIL_TIMEOUT`])
/// rather than sleeping a guessed interval. Counting per address and per kind is
/// what keeps it correct when several messages are in flight at once.
pub async fn wait_for_token(app: &TestApp, email: &str, kind: MailKind, count: usize) -> String {
    let deadline = tokio::time::Instant::now() + MAIL_TIMEOUT;

    loop {
        let matching: Vec<OutgoingMessage> = app
            .mailer()
            .messages_to(email)
            .into_iter()
            .filter(|message| message.text.contains(kind.link_path()))
            .collect();

        if matching.len() >= count {
            return token_from_message(matching.last().expect("the list is non-empty"));
        }

        if tokio::time::Instant::now() >= deadline {
            panic!(
                "{email} never received {count} {kind:?} email(s) within {MAIL_TIMEOUT:?}; \
                 captured: {:?}",
                app.mailer().messages_to(email)
            );
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Follow the newest verification link sent to `email` and return the profile.
pub async fn verify_address(app: &TestApp, email: &str) -> UserProfile {
    let token = wait_for_token(app, email, MailKind::Verification, 1).await;
    let (status, body) = post_json(app, "/auth/verify-email", &json!({ "token": token })).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "following the verification link must succeed: {body}"
    );

    serde_json::from_value(body).expect("the verification response must match UserProfile")
}

/// Insert an already-expired one-shot token for `email` and return the raw token.
///
/// Backdating in SQL is the only honest way to test expiry: the redemption check
/// is evaluated by the database's clock, so a test that shortened a TTL in the
/// application would not exercise it.
pub async fn insert_expired_token(app: &TestApp, email: &str, id: &str, purpose: &str) -> String {
    let raw = format!("expired-{purpose}-{id}");
    let hash = jiuyue_auth::TokenIssuer::hash_link_token(&raw);

    sqlx::query(
        "INSERT INTO account_tokens (id, user_id, purpose, token_hash, expires_at) \
         SELECT $1, u.id, $2, $3, now() - interval '1 hour' \
         FROM users AS u WHERE u.email = $4",
    )
    .bind(id)
    .bind(purpose)
    .bind(&hash)
    .bind(email)
    .execute(app.pool())
    .await
    .expect("inserting an expired token must succeed");

    raw
}

/// An `ErrorBody`-shaped acceptance body, asserted against the fixed contract.
pub fn accepted_body(body: &Value) -> RequestAccepted {
    serde_json::from_value(body.clone())
        .unwrap_or_else(|error| panic!("expected a RequestAccepted body, got {body}: {error}"))
}

/// Panics with an actionable message when no database is configured.
fn database_url() -> String {
    env::var("TEST_DATABASE_URL")
        .ok()
        .or_else(|| env::var("DATABASE_URL").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            panic!(
                "jiuyue-server integration tests need a real PostgreSQL database.\n\
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

    format!("auth_test_{}_{sequence}_{nanos}", std::process::id())
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
