//! The one action that requires a verified email: reaching out to others.
//!
//! Opening a Conversation — direct or group — is where an account first contacts
//! other people, so it is gated on verification. The gate is enforced by the
//! server (`POST /conversations/*` resolve the caller through
//! `AuthService::authenticate_verified`), not by hiding a button: a client that
//! calls the endpoint directly gets the same refusal.
//!
//! Everything else an unverified account could already do — signing in, reading
//! profiles and the conversation list — keeps working. That is the "additive, not
//! retroactive" half of the rule.

use axum::http::StatusCode;
use serde_json::json;

use crate::support::{
    TestApp, error_code, get_with_token, post_json_with_token, register, register_unverified,
    wait_for_verification_token,
};

#[tokio::test]
async fn an_unverified_account_cannot_open_a_conversation_until_it_verifies() {
    let app = TestApp::start().await;

    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let alice = register_unverified(&app, "alice", "alice@example.com", "secret123").await;

    // Positive control: the fixture's verified account *can* open a conversation.
    // (The session `register` returns was captured before verification, so its
    // own `email_verified` flag is stale — the server is the authority.)
    let (status, body) = post_json_with_token(
        &app,
        "/conversations/direct",
        &json!({ "peer_username": "alice" }),
        &bob.tokens.access_token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a verified account may open a conversation: {body}"
    );

    // Opening a Direct Conversation is refused with a code the client can act on.
    let (status, body) = post_json_with_token(
        &app,
        "/conversations/direct",
        &json!({ "peer_username": "bob" }),
        &alice.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "EMAIL_NOT_VERIFIED");

    // Creating a Group is the same outbound reach, so it is gated too.
    let (status, body) = post_json_with_token(
        &app,
        "/conversations/group",
        &json!({ "title": "验证闸门", "member_usernames": ["bob"] }),
        &alice.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "EMAIL_NOT_VERIFIED");

    // Reading is deliberately *not* gated: the account is usable, just silent.
    let (status, body) = get_with_token(&app, "/conversations", &alice.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "reads must still work: {body}");

    // Follow the verification link registration queued...
    let token = wait_for_verification_token(&app, "alice@example.com").await;
    let (status, body) =
        crate::support::post_json(&app, "/auth/verify-email", &json!({ "token": token })).await;
    assert_eq!(status, StatusCode::OK, "verification must succeed: {body}");

    // ...and the very same call now succeeds.
    let (status, body) = post_json_with_token(
        &app,
        "/conversations/direct",
        &json!({ "peer_username": "bob" }),
        &alice.tokens.access_token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a verified account may open a conversation: {body}"
    );

    app.cleanup().await;
}
