//! End-to-end journeys: register, log in, reach a protected path, log out.
//!
//! These are the acceptance criteria of the ticket, driven through the real HTTP
//! surface against a real database.

use axum::http::StatusCode;
use jiuyue_contract::{AuthSession, TokenPair, UserProfile, WhoAmI};
use serde_json::json;
use sqlx::Row;

use crate::support::{
    TestApp, error_code, get_with_token, post_json, post_with_token, register, register_body,
};

#[tokio::test]
async fn register_then_protected_then_logout_then_protected_fails() {
    let app = TestApp::start().await;

    // Register: 201, a profile, and a working token pair.
    let session = register(&app, "alice", "alice@example.com", "secret123").await;
    assert_eq!(session.user.username, "alice");
    assert_eq!(session.user.email, "alice@example.com");
    assert_eq!(
        session.user.display_name, "alice",
        "a blank name falls back to the handle"
    );
    assert!(
        !session.user.email_verified,
        "verification is a later ticket"
    );
    assert_eq!(session.tokens.token_type, "Bearer");
    assert!(session.tokens.expires_in > 0);
    assert!(!session.tokens.access_token.is_empty());
    assert!(!session.tokens.refresh_token.is_empty());

    // The protected endpoint accepts the access token.
    let (status, body) = get_with_token(&app, "/auth/whoami", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "whoami must succeed: {body}");
    let who: WhoAmI = serde_json::from_value(body).expect("whoami must match the contract");
    assert_eq!(who.user_id, session.user.id);
    assert_eq!(who.username, "alice");
    assert!(!who.session_id.is_empty());

    let (status, body) = get_with_token(&app, "/auth/me", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "me must succeed: {body}");
    let profile: UserProfile =
        serde_json::from_value(body).expect("profile must match the contract");
    assert_eq!(profile.id, session.user.id);
    assert_eq!(profile.email, "alice@example.com");

    // Log out...
    let (status, _) = post_with_token(&app, "/auth/logout", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // ...and the *same* access token is refused on the very next request.
    let (status, body) = get_with_token(&app, "/auth/whoami", &session.tokens.access_token).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "logout must end access immediately: {body}"
    );
    assert_eq!(error_code(&body), "UNAUTHENTICATED");

    let (status, _) = get_with_token(&app, "/auth/me", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    app.cleanup().await;
}

#[tokio::test]
async fn after_registering_you_can_log_in_with_the_same_credentials() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    // The email is normalised, so a differently-cased login still matches.
    let (status, body) = post_json(
        &app,
        "/auth/login",
        &json!({ "email": "  ALICE@Example.com ", "password": "secret123" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "login must succeed: {body}");
    let session: AuthSession = serde_json::from_value(body).expect("login must match the contract");
    assert_eq!(session.user.username, "alice");

    let (status, _) = get_with_token(&app, "/auth/whoami", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "the login token must be usable");

    app.cleanup().await;
}

#[tokio::test]
async fn refresh_issues_a_working_access_token_and_stops_after_logout() {
    let app = TestApp::start().await;
    let session = register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, body) = post_json(
        &app,
        "/auth/refresh",
        &json!({ "refresh_token": session.tokens.refresh_token }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "refresh must succeed: {body}");
    let tokens: TokenPair = serde_json::from_value(body).expect("refresh must match the contract");

    let (status, _) = get_with_token(&app, "/auth/whoami", &tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "the refreshed token must work");

    post_with_token(&app, "/auth/logout", &session.tokens.access_token).await;

    let (status, body) = post_json(
        &app,
        "/auth/refresh",
        &json!({ "refresh_token": session.tokens.refresh_token }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a revoked session must not refresh: {body}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn two_logins_are_two_independent_devices() {
    let app = TestApp::start().await;
    let first = register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, body) = post_json(
        &app,
        "/auth/login",
        &json!({ "email": "alice@example.com", "password": "secret123" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second: AuthSession = serde_json::from_value(body).expect("session");

    // Distinct sessions are the point of the session model.
    let (_, body) = get_with_token(&app, "/auth/whoami", &first.tokens.access_token).await;
    let first_who: WhoAmI = serde_json::from_value(body).expect("whoami");
    let (_, body) = get_with_token(&app, "/auth/whoami", &second.tokens.access_token).await;
    let second_who: WhoAmI = serde_json::from_value(body).expect("whoami");
    assert_ne!(
        first_who.session_id, second_who.session_id,
        "each login opens its own session"
    );

    // Logging one device out must not touch the other.
    post_with_token(&app, "/auth/logout", &first.tokens.access_token).await;

    let (status, _) = get_with_token(&app, "/auth/whoami", &first.tokens.access_token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = get_with_token(&app, "/auth/whoami", &second.tokens.access_token).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "logging out one device must not end the other"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn the_stored_password_hash_is_argon2id_and_never_the_plaintext() {
    let app = TestApp::start().await;
    const PASSWORD: &str = "correct-horse-1";

    register(&app, "hash_user", "hash@example.com", PASSWORD).await;

    let row = sqlx::query("SELECT password_hash FROM users WHERE email = $1")
        .bind("hash@example.com")
        .fetch_one(app.pool())
        .await
        .expect("the account must be selectable");
    let stored: String = row.get("password_hash");

    assert!(
        stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "expected an Argon2id PHC string at the OWASP parameters, got: {stored}"
    );
    assert!(
        !stored.contains(PASSWORD),
        "the plaintext password must never be stored"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn the_stored_refresh_token_is_a_digest_not_the_token() {
    let app = TestApp::start().await;
    let session = register(&app, "digest_user", "digest@example.com", "secret123").await;

    let row = sqlx::query("SELECT refresh_token_hash FROM sessions WHERE user_id = $1")
        .bind(&session.user.id)
        .fetch_one(app.pool())
        .await
        .expect("the session must be selectable");
    let stored: String = row.get("refresh_token_hash");

    assert_eq!(stored.len(), 64, "a SHA-256 digest is 64 hex characters");
    assert!(stored.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(
        stored, session.tokens.refresh_token,
        "the raw refresh token must not be recoverable from storage"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn duplicate_registration_with_a_different_username_is_still_rejected_by_email() {
    let app = TestApp::start().await;
    register(&app, "alice", "alice@example.com", "secret123").await;

    // Same email, different case and different username: the unique index on the
    // normalised column must still refuse it.
    let (status, body) = post_json(
        &app,
        "/auth/register",
        &register_body("alice_two", "Alice@Example.com", "secret123"),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "got: {body}");
    assert_eq!(error_code(&body), "EMAIL_TAKEN");

    app.cleanup().await;
}
