//! Access-token failure paths through the HTTP surface.
//!
//! The token layer has its own unit tests; these prove the *server* refuses the
//! same inputs at the edge, where a client would actually hit them.

use axum::http::StatusCode;
use jiuyue_contract::WhoAmI;
use jsonwebtoken::{EncodingKey, Header, encode};
use serde_json::json;

use crate::support::{
    TEST_SECRET, TestApp, error_code, get_with_token, get_without_token, register,
};

/// A second secret, used to prove a foreign signature is refused.
const OTHER_SECRET: &str = "another-signing-secret-9876543210zyxwvu";

#[tokio::test]
async fn a_tampered_access_token_is_rejected() {
    let app = TestApp::start().await;
    let session = register(&app, "alice", "alice@example.com", "secret123").await;

    let mut tampered = session.tokens.access_token.clone();
    let last = tampered.pop().expect("a token is not empty");
    tampered.push(if last == 'A' { 'B' } else { 'A' });

    let (status, body) = get_with_token(&app, "/auth/whoami", &tampered).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "got: {body}");
    assert_eq!(error_code(&body), "UNAUTHENTICATED");

    app.cleanup().await;
}

#[tokio::test]
async fn a_missing_access_token_is_rejected() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, body) = get_without_token(&app, "/auth/whoami").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "got: {body}");
    assert_eq!(error_code(&body), "UNAUTHENTICATED");

    app.cleanup().await;
}

#[tokio::test]
async fn an_expired_access_token_is_rejected() {
    let app = TestApp::start().await;
    let session = register(&app, "alice", "alice@example.com", "secret123").await;

    // Learn the real user/session pair, then forge a correctly signed but expired token.
    let (_, body) = get_with_token(&app, "/auth/whoami", &session.tokens.access_token).await;
    let who: WhoAmI = serde_json::from_value(body).expect("whoami must match the contract");

    let now = now_seconds();
    let expired = forge(
        &json!({
            "sub": who.user_id,
            "sid": who.session_id,
            "iat": now - 7_200,
            "exp": now - 3_600,
        }),
        TEST_SECRET,
    );

    let (status, body) = get_with_token(&app, "/auth/whoami", &expired).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an expired token must not be accepted: {body}"
    );
    assert_eq!(error_code(&body), "UNAUTHENTICATED");

    app.cleanup().await;
}

#[tokio::test]
async fn a_token_signed_with_another_secret_is_rejected() {
    let app = TestApp::start().await;
    let session = register(&app, "alice", "alice@example.com", "secret123").await;

    let (_, body) = get_with_token(&app, "/auth/whoami", &session.tokens.access_token).await;
    let who: WhoAmI = serde_json::from_value(body).expect("whoami must match the contract");

    let now = now_seconds();
    let foreign = forge(
        &json!({
            "sub": who.user_id,
            "sid": who.session_id,
            "iat": now,
            "exp": now + 900,
        }),
        OTHER_SECRET,
    );

    let (status, body) = get_with_token(&app, "/auth/whoami", &foreign).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a foreign signature must not be accepted: {body}"
    );
    assert_eq!(error_code(&body), "UNAUTHENTICATED");

    app.cleanup().await;
}

/// Sign arbitrary claims with a chosen secret.
fn forge(claims: &serde_json::Value, secret: &str) -> String {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("forging a token must succeed")
}

/// Current wall-clock time in whole seconds since the Unix epoch.
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock must be after the Unix epoch")
        .as_secs() as i64
}
