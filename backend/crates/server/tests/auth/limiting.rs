//! Login rate limiting: throttle, decay, lockout, and the memory bound.
//!
//! Driven through the real HTTP surface against a real database. The source is
//! named per request because that — not the email — is what the limiter keys on,
//! so a throttled answer can never reveal whether an account exists.

use std::time::Duration;

use axum::http::StatusCode;

use crate::support::{
    TestApp, error_code, error_retry_after, login_body, login_from, login_policy, post_json_from,
    post_json_from_full, register,
};

/// The source every test acts from unless it says otherwise.
const SOURCE: &str = "203.0.113.7";

/// A password that is well-formed but wrong, so a failure is a credential
/// failure rather than a validation rejection.
const WRONG: &str = "wrong-password1";

#[tokio::test]
async fn repeated_failures_throttle_the_next_attempt_with_a_retry_hint() {
    let (app, _store) = TestApp::start_with_login_policy(
        login_policy()
            .max_failures(2)
            .window(Duration::from_secs(600))
            .throttle(Duration::from_secs(30))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    for attempt in 1..=2 {
        let (status, body) = login_from(&app, "alice@example.com", WRONG, SOURCE).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt}: {body}"
        );
        assert_eq!(error_code(&body), "INVALID_CREDENTIALS");
        assert_eq!(
            error_retry_after(&body),
            None,
            "a plain credential failure carries no wait"
        );
    }

    // The next attempt is refused before the password is looked at, so even the
    // *correct* password is throttled — which is the whole point.
    let (status, headers, body) = post_json_from_full(
        &app,
        "/auth/login",
        &login_body("alice@example.com", "secret123"),
        Some(SOURCE),
    )
    .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "got: {body}");
    assert_eq!(error_code(&body), "TOO_MANY_ATTEMPTS");
    assert_eq!(
        error_retry_after(&body),
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

    app.cleanup().await;
}

#[tokio::test]
async fn an_elapsed_throttle_is_evaluated_normally_again() {
    let (app, _store) = TestApp::start_with_login_policy(
        login_policy()
            .max_failures(2)
            .window(Duration::from_secs(600))
            .throttle(Duration::from_millis(300))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    login_from(&app, "alice@example.com", WRONG, SOURCE).await;
    login_from(&app, "alice@example.com", WRONG, SOURCE).await;

    let (status, _) = login_from(&app, "alice@example.com", "secret123", SOURCE).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Decay is the half of the behaviour that is usually forgotten: a throttle
    // that never lifts is a permanent denial of service against a real user.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let (status, body) = login_from(&app, "alice@example.com", "secret123", SOURCE).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the throttle must have decayed on its own: {body}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_success_clears_the_failure_counter() {
    let (app, _store) = TestApp::start_with_login_policy(
        login_policy()
            .max_failures(3)
            .window(Duration::from_secs(600))
            .throttle(Duration::from_secs(30))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    login_from(&app, "alice@example.com", WRONG, SOURCE).await;
    login_from(&app, "alice@example.com", WRONG, SOURCE).await;

    let (status, body) = login_from(&app, "alice@example.com", "secret123", SOURCE).await;
    assert_eq!(status, StatusCode::OK, "signing in must succeed: {body}");

    // Three more failures must all be plain 401s. Had the two pre-success
    // failures survived, the third of these would already be a 429.
    for attempt in 1..=3 {
        let (status, body) = login_from(&app, "alice@example.com", WRONG, SOURCE).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "failure {attempt} after a success must start the count over: {body}"
        );
    }

    app.cleanup().await;
}

#[tokio::test]
async fn repeated_throttling_escalates_to_a_lockout() {
    let (app, _store) = TestApp::start_with_login_policy(
        login_policy()
            .max_failures(1)
            .window(Duration::from_secs(600))
            .throttle(Duration::from_millis(150))
            .max_throttles(2)
            .lockout(Duration::from_secs(5))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    // First failure earns a throttle.
    let (status, _) = login_from(&app, "alice@example.com", WRONG, SOURCE).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = login_from(&app, "alice@example.com", WRONG, SOURCE).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "got: {body}");

    // Wait the throttle out, then fail again: this is the source reoffending, and
    // it must escalate rather than earn another short backoff.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, _) = login_from(&app, "alice@example.com", WRONG, SOURCE).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = login_from(&app, "alice@example.com", "secret123", SOURCE).await;
    assert_eq!(status, StatusCode::LOCKED, "got: {body}");
    assert_eq!(error_code(&body), "LOCKED_OUT");
    assert_eq!(error_retry_after(&body), Some(5));

    app.cleanup().await;
}

#[tokio::test]
async fn two_sources_do_not_affect_each_other() {
    let (app, _store) = TestApp::start_with_login_policy(
        login_policy()
            .max_failures(2)
            .window(Duration::from_secs(600))
            .throttle(Duration::from_secs(30))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    login_from(&app, "alice@example.com", WRONG, "203.0.113.7").await;
    login_from(&app, "alice@example.com", WRONG, "203.0.113.7").await;

    let (status, body) = login_from(&app, "alice@example.com", "secret123", "198.51.100.9").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "one source exhausting its budget must not touch another: {body}"
    );

    let (status, _) = login_from(&app, "alice@example.com", "secret123", "203.0.113.7").await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the throttled source must still be throttled"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_user_who_mistypes_twice_can_still_log_in_afterwards() {
    let (app, _store) = TestApp::start_with_login_policy(
        login_policy()
            .window(Duration::from_secs(600))
            .throttle(Duration::from_secs(30))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    for attempt in 1..=2 {
        let (status, body) = login_from(&app, "alice@example.com", WRONG, SOURCE).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {attempt}: {body}"
        );
    }

    let (status, body) = login_from(&app, "alice@example.com", "secret123", SOURCE).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "two typos must not cost a real user their sign-in: {body}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_throttled_response_is_identical_for_a_registered_and_an_unregistered_address() {
    let (app, _store) = TestApp::start_with_login_policy(
        login_policy()
            .max_failures(2)
            .window(Duration::from_secs(600))
            .throttle(Duration::from_secs(30))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    for _ in 0..2 {
        login_from(&app, "alice@example.com", WRONG, "203.0.113.7").await;
        login_from(&app, "nobody@example.com", WRONG, "198.51.100.9").await;
    }

    let (known_status, _, known_body) = post_json_from_full(
        &app,
        "/auth/login",
        &login_body("alice@example.com", WRONG),
        Some("203.0.113.7"),
    )
    .await;
    let (unknown_status, _, unknown_body) = post_json_from_full(
        &app,
        "/auth/login",
        &login_body("nobody@example.com", WRONG),
        Some("198.51.100.9"),
    )
    .await;

    assert_eq!(known_status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(unknown_status, StatusCode::TOO_MANY_REQUESTS);

    // Byte-for-byte, not merely "the same code": if the hint counted down, the
    // two responses would still differ for anyone able to observe timing.
    assert_eq!(
        serde_json::to_string(&known_body).expect("serialisable"),
        serde_json::to_string(&unknown_body).expect("serialisable"),
        "throttling must not be an account-enumeration oracle"
    );
    assert!(
        error_retry_after(&known_body).is_some(),
        "the comparison is only meaningful while the hint is present"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_spoofed_leading_forwarded_address_cannot_rotate_the_key() {
    let (app, _store) = TestApp::start_with_login_policy(
        login_policy()
            .max_failures(2)
            .window(Duration::from_secs(600))
            .throttle(Duration::from_secs(30))
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    // Caddy appends the address it observed, so the trusted entry is the last one.
    // A client that varies the leading entry must not get a fresh budget.
    for prefix in ["198.51.100.1", "198.51.100.2"] {
        let spoofed = format!("{prefix}, {SOURCE}");
        let (status, body) = post_json_from(
            &app,
            "/auth/login",
            &login_body("alice@example.com", WRONG),
            Some(&spoofed),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "got: {body}");
    }

    let (status, body) = login_from(&app, "alice@example.com", "secret123", SOURCE).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the spoofed prefixes must not have created separate buckets: {body}"
    );
    assert_eq!(error_code(&body), "TOO_MANY_ATTEMPTS");

    app.cleanup().await;
}

#[tokio::test]
async fn the_tracked_source_cap_holds_when_many_distinct_sources_arrive() {
    const CAP: usize = 4;

    let (app, store) = TestApp::start_with_login_policy(
        login_policy()
            .window(Duration::from_secs(600))
            .max_tracked_sources(CAP)
            .build(),
    )
    .await;

    register(&app, "alice", "alice@example.com", "secret123").await;

    for index in 0..12u16 {
        let source = format!("203.0.113.{index}");
        let (status, body) = login_from(&app, "alice@example.com", WRONG, &source).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "got: {body}");

        assert!(
            store.tracked_sources() <= CAP,
            "the store held {} sources, above the cap of {CAP}",
            store.tracked_sources()
        );
    }

    app.cleanup().await;
}
