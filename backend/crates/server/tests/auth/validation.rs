//! Rejection shapes: the machine-readable errors the frontend maps to fields.

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::support::{TestApp, error_code, post_json, register, register_body};

#[tokio::test]
async fn a_duplicate_email_is_rejected_distinctly_from_a_generic_failure() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, body) = post_json(
        &app,
        "/auth/register",
        &register_body("alice_two", "alice@example.com", "secret123"),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "got: {body}");
    assert_eq!(error_code(&body), "EMAIL_TAKEN");
    assert_eq!(body["error"]["fields"][0]["field"], "email");
    assert_eq!(body["error"]["fields"][0]["code"], "TAKEN");

    app.cleanup().await;
}

#[tokio::test]
async fn a_duplicate_username_is_rejected_distinctly() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, body) = post_json(
        &app,
        "/auth/register",
        &register_body("alice", "alice_two@example.com", "secret123"),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "got: {body}");
    assert_eq!(error_code(&body), "USERNAME_TAKEN");
    assert_eq!(body["error"]["fields"][0]["field"], "username");

    app.cleanup().await;
}

#[tokio::test]
async fn a_malformed_email_is_rejected_with_field_detail() {
    let app = TestApp::start().await;

    let (status, body) = post_json(
        &app,
        "/auth/register",
        &register_body("alice", "not-an-email", "secret123"),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "got: {body}");
    assert_eq!(error_code(&body), "VALIDATION_FAILED");
    assert!(
        has_field(&body, "email", "INVALID_FORMAT"),
        "expected an email/INVALID_FORMAT entry, got: {body}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_weak_password_is_rejected_with_field_detail() {
    let app = TestApp::start().await;

    for (password, expected) in [("short1", "TOO_SHORT"), ("allletters", "WEAK")] {
        let (status, body) = post_json(
            &app,
            "/auth/register",
            &register_body("alice", "alice@example.com", password),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "got: {body}");
        assert_eq!(error_code(&body), "VALIDATION_FAILED");
        assert!(
            has_field(&body, "password", expected),
            "`{password}` should report password/{expected}, got: {body}"
        );
    }

    app.cleanup().await;
}

#[tokio::test]
async fn every_broken_field_is_reported_in_one_response() {
    let app = TestApp::start().await;

    let (status, body) =
        post_json(&app, "/auth/register", &register_body("A", "nope", "short")).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "VALIDATION_FAILED");

    let fields = body["error"]["fields"]
        .as_array()
        .expect("fields must be an array");
    assert_eq!(
        fields.len(),
        3,
        "username, email and password must all be reported: {body}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_account_look_the_same() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    let (wrong_status, wrong_body) = post_json(
        &app,
        "/auth/login",
        &json!({ "email": "alice@example.com", "password": "secret124" }),
    )
    .await;

    let (unknown_status, unknown_body) = post_json(
        &app,
        "/auth/login",
        &json!({ "email": "nobody@example.com", "password": "secret123" }),
    )
    .await;

    assert_eq!(wrong_status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown_status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&wrong_body), "INVALID_CREDENTIALS");
    assert_eq!(error_code(&unknown_body), "INVALID_CREDENTIALS");
    assert_eq!(
        wrong_body["error"]["message"], unknown_body["error"]["message"],
        "login must not reveal whether an account exists"
    );

    app.cleanup().await;
}

/// Whether `error.fields` contains an entry for `field` with `code`.
fn has_field(body: &Value, field: &str, code: &str) -> bool {
    body["error"]["fields"].as_array().is_some_and(|fields| {
        fields.iter().any(|entry| {
            entry["field"].as_str() == Some(field) && entry["code"].as_str() == Some(code)
        })
    })
}
