//! Shared integration-test harness: a real router over a throwaway PostgreSQL schema.
//!
//! Each test gets its own schema (selected through the connection's `search_path`)
//! migrated to latest, so tests run in parallel and can be re-run with no manual
//! cleanup. Without `TEST_DATABASE_URL` (or `DATABASE_URL`) the harness panics
//! with instructions — a silently skipped test would hide a broken contract,
//! which is worse than no test at all.

use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use jiuyue_auth::{AuthConfig, AuthService};
use jiuyue_contract::AuthSession;
use jiuyue_server::{AppState, Config, app};
use jiuyue_store::Store;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor as _, PgPool};
use tower::ServiceExt;

/// Signing secret used by the test router. Long enough for the 32-byte minimum.
pub const TEST_SECRET: &str = "test-signing-secret-0123456789abcdef";

/// A running server under test: the router plus the schema it writes into.
pub struct TestApp {
    router: Router,
    store: Store,
    admin_url: String,
    schema: String,
}

impl TestApp {
    /// Create a uniquely named schema, migrate it, and build the real router.
    pub async fn start() -> Self {
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

        let auth = AuthService::new(store.pool().clone(), AuthConfig::new(TEST_SECRET))
            .await
            .expect("the identity service must build with a valid secret");

        let router = app(AppState::with_auth(Config::default(), auth));

        Self {
            router,
            store,
            admin_url,
            schema,
        }
    }

    /// A clone of the router, ready for one request.
    pub fn router(&self) -> Router {
        self.router.clone()
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
