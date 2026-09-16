//! The two email journeys: verify an address, reset a password.
//!
//! Everything here runs against the real router, the real `account_tokens` table
//! and a capturing [`jiuyue_auth::InMemoryMailer`] — no database is mocked and no
//! mail provider is contacted. The captured link is "followed" exactly as a human
//! would follow it, by POSTing its token back to the API.

use std::time::Duration;

use axum::http::StatusCode;
use jiuyue_contract::UserProfile;
use serde_json::{Value, json};

use crate::support::{
    MailKind, TestApp, accepted_body, error_code, insert_expired_token, login_body, post_json,
    post_json_from, post_json_from_full, register, verify_address, wait_for_token,
};

/// The source every test acts from unless it says otherwise.
const SOURCE: &str = "203.0.113.7";

/// A second source, for the "unknown address" half of the oracle test.
const OTHER_SOURCE: &str = "198.51.100.9";

/// A syntactically valid ULID for a hand-inserted token row.
const EXPIRED_ID: &str = "01J8ZQ7K2M4N6P8R0T2V4X6Y8Z";

/// The reset-link path fragment, for counting reset mail.
const RESET_LINK: &str = "/reset-password?token=";

fn verify_body(token: &str) -> Value {
    json!({ "token": token })
}

fn reset_body(token: &str, password: &str) -> Value {
    json!({ "token": token, "password": password })
}

/// How many reset links have been captured for `email`.
fn reset_links_sent(app: &TestApp, email: &str) -> usize {
    app.mailer()
        .messages_to(email)
        .iter()
        .filter(|message| message.text.contains(RESET_LINK))
        .count()
}

#[tokio::test]
async fn registering_sends_a_verification_link_and_following_it_verifies_the_account() {
    let app = TestApp::start().await;

    let session = register(&app, "alice", "alice@example.com", "secret123").await;
    assert!(
        !session.user.email_verified,
        "a fresh account starts unverified"
    );

    // Follow the link registration queued, exactly as the mail client would.
    let profile = verify_address(&app, "alice@example.com").await;
    assert!(
        profile.email_verified,
        "the profile must report verification"
    );
    assert_eq!(profile.id, session.user.id);

    // The stored row agrees, and `/auth/me` reflects it on the next read.
    let stored: Option<sqlx::types::time::OffsetDateTime> =
        sqlx::query_scalar("SELECT email_verified_at FROM users WHERE email = $1")
            .bind("alice@example.com")
            .fetch_one(app.pool())
            .await
            .expect("the account must be selectable");
    assert!(
        stored.is_some(),
        "email_verified_at must be set in the database"
    );

    let (status, body) =
        crate::support::get_with_token(&app, "/auth/me", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "me must succeed: {body}");
    let me: UserProfile = serde_json::from_value(body).expect("me must match UserProfile");
    assert!(me.email_verified, "/auth/me must report the verified flag");

    app.cleanup().await;
}

#[tokio::test]
async fn a_verification_token_can_only_be_used_once() {
    let app = TestApp::start().await;

    register(&app, "alice", "alice@example.com", "secret123").await;
    let token = wait_for_token(&app, "alice@example.com", MailKind::Verification, 1).await;

    let (status, body) = post_json(&app, "/auth/verify-email", &verify_body(&token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the first redemption must succeed: {body}"
    );

    let (status, body) = post_json(&app, "/auth/verify-email", &verify_body(&token)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a spent link must be refused: {body}"
    );
    assert_eq!(error_code(&body), "TOKEN_INVALID");

    // And the account is still verified: the failed replay changed nothing.
    let stored: Option<sqlx::types::time::OffsetDateTime> =
        sqlx::query_scalar("SELECT email_verified_at FROM users WHERE email = $1")
            .bind("alice@example.com")
            .fetch_one(app.pool())
            .await
            .expect("the account must be selectable");
    assert!(stored.is_some());

    app.cleanup().await;
}

#[tokio::test]
async fn an_expired_verification_token_is_refused_distinctly() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    let token =
        insert_expired_token(&app, "alice@example.com", EXPIRED_ID, "email_verification").await;
    let (status, body) = post_json(&app, "/auth/verify-email", &verify_body(&token)).await;

    assert_eq!(status, StatusCode::GONE, "an expired link is gone: {body}");
    assert_eq!(
        error_code(&body),
        "TOKEN_EXPIRED",
        "expiry must be distinguishable from a bad token"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn an_unknown_verification_token_is_refused() {
    let app = TestApp::start().await;

    let (status, body) = post_json(
        &app,
        "/auth/verify-email",
        &verify_body("this-token-was-never-minted"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "got: {body}");
    assert_eq!(error_code(&body), "TOKEN_INVALID");

    app.cleanup().await;
}

#[tokio::test]
async fn a_new_verification_link_retires_the_previous_one() {
    let app = TestApp::start().await;

    register(&app, "alice", "alice@example.com", "secret123").await;
    let first = wait_for_token(&app, "alice@example.com", MailKind::Verification, 1).await;

    let (status, _) = post_json(
        &app,
        "/auth/resend-verification",
        &json!({ "email": "alice@example.com" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let second = wait_for_token(&app, "alice@example.com", MailKind::Verification, 2).await;

    assert_ne!(first, second, "a re-send must mint a new token");

    let (status, body) = post_json(&app, "/auth/verify-email", &verify_body(&first)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the superseded link must stop working: {body}"
    );

    let (status, body) = post_json(&app, "/auth/verify-email", &verify_body(&second)).await;
    assert_eq!(status, StatusCode::OK, "the newest link must work: {body}");

    app.cleanup().await;
}

#[tokio::test]
async fn forgot_password_for_an_unknown_address_is_indistinguishable_from_a_known_one() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    let (known_status, known_headers, known_body) = post_json_from_full(
        &app,
        "/auth/forgot-password",
        &json!({ "email": "alice@example.com" }),
        Some(SOURCE),
    )
    .await;
    let (unknown_status, unknown_headers, unknown_body) = post_json_from_full(
        &app,
        "/auth/forgot-password",
        &json!({ "email": "nobody@example.com" }),
        Some(OTHER_SOURCE),
    )
    .await;

    assert_eq!(known_status, StatusCode::ACCEPTED);
    assert_eq!(unknown_status, StatusCode::ACCEPTED);

    // The response is identical in every visible respect. A different body, code
    // or content type would each leak whether the address has an account.
    assert_eq!(
        serde_json::to_string(&known_body).expect("serialisable"),
        serde_json::to_string(&unknown_body).expect("serialisable"),
        "the two responses must be byte-identical"
    );
    assert_eq!(
        known_headers.get("content-type"),
        unknown_headers.get("content-type")
    );
    assert!(accepted_body(&known_body).accepted);

    // Only the real account produced mail — which is the intended behaviour, and
    // the reason the *response* must not encode it.
    wait_for_token(&app, "alice@example.com", MailKind::Reset, 1).await;
    assert!(
        app.mailer().messages_to("nobody@example.com").is_empty(),
        "an unknown address receives nothing"
    );
    assert_eq!(
        reset_links_sent(&app, "alice@example.com"),
        1,
        "exactly one reset link was sent to the real account"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_reset_link_changes_the_password_and_is_spent_by_doing_so() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, _) = post_json_from(
        &app,
        "/auth/forgot-password",
        &json!({ "email": "alice@example.com" }),
        Some(SOURCE),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let token = wait_for_token(&app, "alice@example.com", MailKind::Reset, 1).await;
    let (status, body) = post_json(
        &app,
        "/auth/reset-password",
        &reset_body(&token, "brand-new-1"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the reset must succeed: {body}"
    );

    // The old password is dead and the new one works.
    let (status, body) = post_json(
        &app,
        "/auth/login",
        &login_body("alice@example.com", "secret123"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the old password must stop working: {body}"
    );

    let (status, body) = post_json(
        &app,
        "/auth/login",
        &login_body("alice@example.com", "brand-new-1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the new password must work: {body}");

    // The link is single-use: spending it once was the point.
    let (status, body) = post_json(
        &app,
        "/auth/reset-password",
        &reset_body(&token, "another-one-2"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a spent reset link must be refused: {body}"
    );
    assert_eq!(error_code(&body), "TOKEN_INVALID");

    app.cleanup().await;
}

#[tokio::test]
async fn an_expired_reset_token_is_refused_and_changes_nothing() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    let token = insert_expired_token(&app, "alice@example.com", EXPIRED_ID, "password_reset").await;
    let (status, body) = post_json(
        &app,
        "/auth/reset-password",
        &reset_body(&token, "brand-new-1"),
    )
    .await;

    assert_eq!(status, StatusCode::GONE, "got: {body}");
    assert_eq!(error_code(&body), "TOKEN_EXPIRED");

    // The password was not touched.
    let (status, _) = post_json(
        &app,
        "/auth/login",
        &login_body("alice@example.com", "secret123"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the original password must still work"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_successful_reset_revokes_the_sessions_opened_before_it() {
    let app = TestApp::start().await;
    let session = register(&app, "alice", "alice@example.com", "secret123").await;

    post_json_from(
        &app,
        "/auth/forgot-password",
        &json!({ "email": "alice@example.com" }),
        Some(SOURCE),
    )
    .await;
    let token = wait_for_token(&app, "alice@example.com", MailKind::Reset, 1).await;

    let (status, _) = post_json(
        &app,
        "/auth/reset-password",
        &reset_body(&token, "brand-new-1"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // A password change must end every session opened with the old password.
    let (status, body) =
        crate::support::get_with_token(&app, "/auth/me", &session.tokens.access_token).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the pre-reset session must be revoked: {body}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn an_unverified_account_can_still_log_in_and_read_its_profile() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    // Verification is additive: an account that never opened its inbox is not
    // locked out of logging in or reading — only of reaching out to others.
    let (status, body) = post_json(
        &app,
        "/auth/login",
        &login_body("alice@example.com", "secret123"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login must work unverified: {body}");

    let session: jiuyue_contract::AuthSession =
        serde_json::from_value(body).expect("login must match AuthSession");
    assert!(!session.user.email_verified);

    let (status, body) =
        crate::support::get_with_token(&app, "/auth/me", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "me must work unverified: {body}");

    app.cleanup().await;
}

#[tokio::test]
async fn an_over_eager_reset_requester_is_limited() {
    let (app, _login, _email) = TestApp::start_with_policies(
        crate::support::login_policy().build(),
        crate::support::login_policy()
            .max_failures(2)
            .window(Duration::from_secs(600))
            .throttle(Duration::from_secs(30))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    // Two accepted requests, then the third is refused before any mail is queued.
    for attempt in 1..=2 {
        let (status, body) = post_json_from(
            &app,
            "/auth/forgot-password",
            &json!({ "email": "alice@example.com" }),
            Some(SOURCE),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "attempt {attempt}: {body}");
    }

    let (status, headers, body) = post_json_from_full(
        &app,
        "/auth/forgot-password",
        &json!({ "email": "alice@example.com" }),
        Some(SOURCE),
    )
    .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "got: {body}");
    assert_eq!(error_code(&body), "TOO_MANY_ATTEMPTS");
    assert_eq!(
        crate::support::error_retry_after(&body),
        Some(30),
        "the wait must travel as data: {body}"
    );
    assert_eq!(
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("30"),
        "the standard header must agree with the body"
    );

    // Two accepted requests sent two links; the refused one sent nothing.
    wait_for_token(&app, "alice@example.com", MailKind::Reset, 2).await;
    assert_eq!(reset_links_sent(&app, "alice@example.com"), 2);

    app.cleanup().await;
}

#[tokio::test]
async fn resending_verification_for_an_unknown_address_is_indistinguishable() {
    let app = TestApp::start().await;

    let (known_status, known_body) = post_json_from(
        &app,
        "/auth/resend-verification",
        &json!({ "email": "nobody@example.com" }),
        Some(SOURCE),
    )
    .await;
    let (unknown_status, unknown_body) = post_json_from(
        &app,
        "/auth/resend-verification",
        &json!({ "email": "still-nobody@example.com" }),
        Some(OTHER_SOURCE),
    )
    .await;

    assert_eq!(known_status, StatusCode::ACCEPTED);
    assert_eq!(unknown_status, StatusCode::ACCEPTED);
    assert_eq!(
        serde_json::to_string(&known_body).expect("serialisable"),
        serde_json::to_string(&unknown_body).expect("serialisable")
    );
    assert_eq!(app.mailer().count(), 0, "no address exists, so no mail");

    app.cleanup().await;
}

#[tokio::test]
async fn a_verified_account_is_not_emailed_again_by_a_resend() {
    let app = TestApp::start().await;

    register(&app, "alice", "alice@example.com", "secret123").await;
    verify_address(&app, "alice@example.com").await;

    let before = app.mailer().count();
    let (status, _) = post_json(
        &app,
        "/auth/resend-verification",
        &json!({ "email": "alice@example.com" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Give a queued delivery (there should be none) more than enough time to land.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        app.mailer().count(),
        before,
        "an already-verified account must not receive another link"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_malformed_address_is_a_field_error_not_an_account_lookup() {
    let app = TestApp::start().await;

    let (status, body) = post_json(
        &app,
        "/auth/forgot-password",
        &json!({ "email": "not-an-email" }),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "got: {body}");
    assert_eq!(error_code(&body), "VALIDATION_FAILED");
    assert_eq!(body["error"]["fields"][0]["field"], "email");
    assert_eq!(app.mailer().count(), 0, "a rejected request sends nothing");

    app.cleanup().await;
}
