//! In-process integration tests for the HTTP surface.
//!
//! The router is exercised through `tower::ServiceExt::oneshot`, so no socket is
//! bound and no port is required.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use jiuyue_server::{AppState, Config, app};
use serde_json::Value;
use tower::ServiceExt;

/// Builds the router under test with default configuration.
fn test_router() -> axum::Router {
    app(AppState::new(Config::default()))
}

#[tokio::test]
async fn health_reports_ok_with_expected_body() {
    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .expect("request must build");

    let response = test_router()
        .oneshot(request)
        .await
        .expect("router must answer");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
    );

    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body must be readable");
    let payload: Value = serde_json::from_slice(&body).expect("body must be JSON");

    assert_eq!(payload["status"], "ok");
    // Same source of truth the handler uses. The release pipeline can override it
    // at compile time (`JIUYUE_BUILD_VERSION`), so the test must not hard-code the
    // workspace crate version.
    assert_eq!(payload["version"], jiuyue_server::routes::VERSION);

    let uptime = payload["uptime_s"]
        .as_u64()
        .expect("uptime_s must be a non-negative integer");
    assert!(
        uptime < 60,
        "a freshly started process must report tiny uptime, got {uptime}"
    );
}

#[tokio::test]
async fn unknown_route_is_not_found() {
    let request = Request::builder()
        .uri("/does-not-exist")
        .body(Body::empty())
        .expect("request must build");

    let response = test_router()
        .oneshot(request)
        .await
        .expect("router must answer");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
