//! Third-party sign-in, end to end through the real HTTP surface.
//!
//! Real router, real service, real database, real `state` and PKCE handling. The
//! provider is the only thing faked, because there is no network and no provider
//! sandbox — the seam it is faked at is the one that owns the wire.
//!
//! The tests that matter most are the two refusals: a matching email must not
//! hand over an account, and a replayed `state` must not complete a second sign-in.

use axum::http::StatusCode;
use jiuyue_contract::{AuthSession, OAuthCallbackResponse, OAuthProvider, UserProfile, WhoAmI};
use serde_json::{Value, json};

use crate::support::{TestApp, error_code, get_with_token, post_json, post_with_token, register};

/// Which provider the journeys use: the one the test instance is configured for.
const GITHUB: OAuthProvider = OAuthProvider::GitHub;

/// The provider that is deliberately left without credentials.
const UNCONFIGURED: OAuthProvider = crate::oauth_support::UNCONFIGURED;

/// The callback the instance advertises, asserted by the fake transport.
const REDIRECT_URI: &str = "http://localhost:5173/auth/oauth/callback";

/// Start a round trip and return the parsed authorize URL's query parameters.
///
/// The URL is read the way a browser would: parse the query string, do not
/// scrape it. That is what makes this assert the real encoding.
async fn start(app: &TestApp, provider: OAuthProvider) -> (StatusCode, Value) {
    post_json(app, "/auth/oauth/start", &json!({ "provider": provider })).await
}

/// Start a round trip and return its `state`.
async fn start_state(app: &TestApp, provider: OAuthProvider) -> String {
    let (status, body) = start(app, provider).await;
    assert_eq!(status, StatusCode::OK, "start must succeed: {body}");

    let url = authorize_url(&body);
    let query = query_of(&url);

    query
        .get("state")
        .and_then(Value::as_str)
        .expect("the authorization URL must carry a state")
        .to_owned()
}

/// The `authorize_url` from a start response.
fn authorize_url(body: &Value) -> String {
    body["authorize_url"]
        .as_str()
        .unwrap_or_else(|| panic!("no authorize_url in {body}"))
        .to_owned()
}

/// The query parameters of a URL, as a JSON object.
fn query_of(url: &str) -> Value {
    let query = url
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_else(|| panic!("{url} has no query"));

    let mut object = serde_json::Map::new();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        object.insert(key.into_owned(), Value::String(value.into_owned()));
    }

    Value::Object(object)
}

/// Drive a callback and return the status and body.
async fn callback(
    app: &TestApp,
    provider: OAuthProvider,
    code: &str,
    state: &str,
) -> (StatusCode, Value) {
    post_json(
        app,
        "/auth/oauth/callback",
        &json!({ "provider": provider, "code": code, "state": state }),
    )
    .await
}

/// A callback that must succeed, as a parsed response.
async fn callback_ok(
    app: &TestApp,
    provider: OAuthProvider,
    code: &str,
    state: &str,
) -> OAuthCallbackResponse {
    let (status, body) = callback(app, provider, code, state).await;
    assert_eq!(status, StatusCode::OK, "callback must succeed: {body}");

    serde_json::from_value(body).expect("the callback response must match the contract")
}

/// Drive a hand-built request and drop the headers, which these journeys never
/// assert on.
async fn await_body(
    app: &TestApp,
    request: axum::http::Request<axum::body::Body>,
) -> (StatusCode, Value) {
    let (status, _, body) = crate::support::send_full_body(app, request).await;

    (status, body)
}

/// Complete the username step with a limited token.
async fn complete_with(app: &TestApp, limited_token: &str, username: &str) -> (StatusCode, Value) {
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/auth/oauth/complete")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {limited_token}"))
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({ "username": username })).expect("body"),
        ))
        .expect("the request must build");

    await_body(app, request).await
}

/// Begin a first-time sign-in and return the limited token and suggested handle.
///
/// The `state` is taken from the same start response the callback then redeems:
/// the challenge sent to the provider and the verifier the callback uses are a
/// pair, so taking a second state here would test a different round trip's.
async fn first_time_sign_in(
    app: &TestApp,
    subject: &str,
    login: &str,
    email: &str,
) -> (String, String) {
    app.oauth().script_github(subject, login, Some(email), true);
    let (status, body) = start(app, GITHUB).await;
    assert_eq!(status, StatusCode::OK, "start must succeed: {body}");
    let state = query_of(&authorize_url(&body))["state"]
        .as_str()
        .expect("a state")
        .to_owned();

    let response = callback_ok(app, GITHUB, "github-code", &state).await;

    assert!(
        response.session.is_none(),
        "a first-time provider user must not get a full session"
    );

    let onboarding = response
        .onboarding
        .expect("a first-time provider user must be asked for a username");

    (onboarding.limited_token, onboarding.suggested_username)
}

#[tokio::test]
async fn a_first_time_provider_user_creates_an_account_and_stops_at_the_username_step() {
    let app = TestApp::start().await;

    app.oauth()
        .script_github("583231", "octocat", Some("octocat@example.com"), true);

    // The authorize URL is the documented endpoint, carrying the state, the PKCE
    // challenge and the fixed redirect — none of which the client chose. The
    // callback below redeems *this* URL's state, so the pair under test is the
    // pair that was actually handed out.
    let (status, body) = start(&app, GITHUB).await;
    assert_eq!(status, StatusCode::OK);
    let url = authorize_url(&body);
    assert!(
        url.starts_with("https://github.com/login/oauth/authorize?"),
        "{url}"
    );
    let query = query_of(&url);
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["redirect_uri"], REDIRECT_URI);
    assert_eq!(query["scope"], "read:user user:email");
    let state = query["state"].as_str().expect("a state").to_owned();

    let response = callback_ok(&app, GITHUB, "github-code", &state).await;
    let onboarding = response
        .onboarding
        .clone()
        .expect("a first-time provider user must be asked for a username");
    assert!(
        response.session.is_none(),
        "the account must not be usable before a username is chosen"
    );
    assert_eq!(
        onboarding.email.as_deref(),
        Some("octocat@example.com"),
        "the reported address is shown so the user can recognise the account"
    );

    // The account exists, with the address and display name the provider asserted.
    let row = sqlx::query(
        "SELECT u.email, u.password_hash, u.email_verified_at IS NOT NULL AS verified \
         FROM oauth_identities AS oi JOIN users AS u ON u.id = oi.user_id \
         WHERE oi.provider = 'github' AND oi.subject = '583231'",
    )
    .fetch_one(app.pool())
    .await
    .expect("the identity must be stored");
    use sqlx::Row as _;
    assert_eq!(row.get::<String, _>("email"), "octocat@example.com");
    assert!(
        row.get::<Option<String>, _>("password_hash").is_none(),
        "an OAuth account has no password to log in with"
    );
    assert!(
        row.get::<bool, _>("verified"),
        "a provider that asserts a verified address is believed"
    );

    // Finish the step and land on a real session.
    let (status, body) = complete_with(&app, &onboarding.limited_token, "octo_cat").await;
    assert_eq!(status, StatusCode::OK, "completing must succeed: {body}");
    let finished: OAuthCallbackResponse =
        serde_json::from_value(body).expect("the completion response must match the contract");
    let session = finished.session.expect("completing must produce a session");
    assert_eq!(session.user.username, "octo_cat");
    assert_eq!(session.user.email, "octocat@example.com");

    let (status, who) = get_with_token(&app, "/auth/whoami", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "the new session must work: {who}");
    let who: WhoAmI = serde_json::from_value(who).expect("whoami");
    assert_eq!(who.username, "octo_cat");

    app.cleanup().await;
}

#[tokio::test]
async fn the_limited_session_is_refused_by_every_protected_endpoint() {
    let app = TestApp::start().await;
    let (limited_token, _suggested) =
        first_time_sign_in(&app, "9001", "limited_user", "limited@example.com").await;

    // It is not an access token, so every endpoint that requires one refuses it.
    for path in ["/auth/whoami", "/auth/me"] {
        let (status, body) = get_with_token(&app, path, &limited_token).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} must refuse a limited session: {body}"
        );
        assert_eq!(error_code(&body), "UNAUTHENTICATED");
    }

    let (status, body) = post_with_token(&app, "/auth/logout", &limited_token).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a limited session must not even be able to log out: {body}"
    );

    // The chat surface refuses it too, which is the point of it being opaque
    // rather than a JWT with a flag.
    let (status, body) = get_with_token(&app, "/conversations", &limited_token).await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::SERVICE_UNAVAILABLE,
        "the chat surface must refuse it too, got {status}: {body}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn signing_in_with_the_same_provider_identity_lands_on_the_same_account() {
    let app = TestApp::start().await;
    let (limited_token, _) =
        first_time_sign_in(&app, "4242", "returning_user", "returning@example.com").await;

    let (status, body) = complete_with(&app, &limited_token, "returning_user").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let stored = sqlx::query(
        "SELECT u.id::text AS id, u.username, u.password_hash, \
                (SELECT count(*) FROM oauth_identities AS oi WHERE oi.user_id = u.id) AS links \
         FROM users AS u WHERE u.email = $1",
    )
    .bind("returning@example.com")
    .fetch_one(app.pool())
    .await
    .expect("the account must exist after completing");
    use sqlx::Row as _;
    assert_eq!(stored.get::<String, _>("username"), "returning_user");

    // Sign in again with the *same* provider identity. This time there is a
    // finished account, so the answer is a session — the same account, not a
    // second one.
    app.oauth().script_github(
        "4242",
        "returning_user",
        Some("returning@example.com"),
        true,
    );
    let state = start_state(&app, GITHUB).await;
    let response = callback_ok(&app, GITHUB, "github-code-2", &state).await;

    assert!(
        response.onboarding.is_none(),
        "the account is finished, so there is nothing to onboard: {response:?}"
    );
    let session = response
        .session
        .expect("the second sign-in must produce a session directly");
    assert_eq!(
        session.user.id,
        stored.get::<String, _>("id"),
        "the same identity must sign into the same account"
    );
    assert_eq!(session.user.username, "returning_user");
    assert!(response.onboarding.is_none());

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = $1")
        .bind("returning@example.com")
        .fetch_one(app.pool())
        .await
        .expect("counting must work");
    assert_eq!(
        count, 1,
        "the same identity must not create a second account"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_matching_email_never_hands_over_the_account() {
    let app = TestApp::start().await;

    // An account with an email and a password, created the ordinary way.
    let existing = register(&app, "alice", "alice@example.com", "secret123").await;

    // A provider identity that is NOT linked to it, asserting the same address.
    app.oauth()
        .script_github("7777", "alice-on-github", Some("alice@example.com"), true);
    let state = start_state(&app, GITHUB).await;

    let (status, body) = callback(&app, GITHUB, "github-code", &state).await;

    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a matching address must be refused, not linked and not signed in: {body}"
    );
    assert_eq!(error_code(&body), "OAUTH_ACCOUNT_EXISTS");

    // Nothing was linked, and no second account was created.
    let links: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM oauth_identities WHERE provider = 'github' AND subject = '7777'",
    )
    .fetch_one(app.pool())
    .await
    .expect("counting must work");
    assert_eq!(links, 0, "the identity must not have been attached");

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = $1")
        .bind("alice@example.com")
        .fetch_one(app.pool())
        .await
        .expect("counting must work");
    assert_eq!(accounts, 1, "no duplicate account may be created");

    // And the original account still works exactly as before.
    let (status, body) = post_json(
        &app,
        "/auth/login",
        &json!({ "email": "alice@example.com", "password": "secret123" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the real account is untouched: {body}"
    );
    let session: AuthSession = serde_json::from_value(body).expect("a session");
    assert_eq!(session.user.id, existing.user.id);

    app.cleanup().await;
}

#[tokio::test]
async fn a_matching_email_is_refused_even_when_the_provider_did_not_verify_it() {
    let app = TestApp::start().await;
    register(&app, "bob", "bob@example.com", "secret123").await;

    // An unverified assertion is still not a way to claim the account: the check
    // is the address, not the provider's confidence in it.
    app.oauth()
        .script_github("8888", "bob-elsewhere", Some("bob@example.com"), false);
    let state = start_state(&app, GITHUB).await;

    let (status, body) = callback(&app, GITHUB, "github-code", &state).await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "OAUTH_ACCOUNT_EXISTS");

    app.cleanup().await;
}

#[tokio::test]
async fn a_provider_identity_cannot_be_claimed_by_a_second_account() {
    let app = TestApp::start().await;

    // An account owns a provider identity.
    let (limited_token, _) = first_time_sign_in(&app, "5555", "owner", "owner@example.com").await;
    let (status, body) = complete_with(&app, &limited_token, "owner").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A second account signs in, and tries to link the same provider identity.
    let second = register(&app, "intruder", "intruder@example.com", "secret123").await;

    app.oauth()
        .script_github("5555", "owner", Some("owner@example.com"), true);
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/auth/oauth/link")
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", second.tokens.access_token),
        )
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({ "provider": GITHUB })).expect("body"),
        ))
        .expect("the request must build");
    let (status, _, body) = crate::support::send_full_body(&app, request).await;
    assert_eq!(status, StatusCode::OK, "starting a link must work: {body}");
    let state = query_of(&authorize_url(&body))
        .get("state")
        .and_then(Value::as_str)
        .expect("a state")
        .to_owned();

    let (status, body) = callback(&app, GITHUB, "github-code", &state).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "one provider identity belongs to one account: {body}"
    );
    assert_eq!(error_code(&body), "OAUTH_ACCOUNT_EXISTS");

    app.cleanup().await;
}

#[tokio::test]
async fn a_signed_in_user_can_link_their_own_provider_identity() {
    let app = TestApp::start().await;
    let account = register(&app, "carol", "carol@example.com", "secret123").await;

    app.oauth()
        .script_github("6666", "carol", Some("carol@example.com"), true);

    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri("/auth/oauth/link")
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", account.tokens.access_token),
        )
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({ "provider": GITHUB })).expect("body"),
        ))
        .expect("the request must build");
    let (status, _, body) = crate::support::send_full_body(&app, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let state = query_of(&authorize_url(&body))
        .get("state")
        .and_then(Value::as_str)
        .expect("a state")
        .to_owned();

    // The account id came from the bearer token, not from the request body, so no
    // body field could have pointed the link at somebody else.
    let response = callback_ok(&app, GITHUB, "github-code", &state).await;
    let session = response
        .session
        .expect("linking signs the existing account in");
    assert_eq!(
        session.user.id, account.user.id,
        "the linked account is the one that was signed in"
    );

    let linked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM oauth_identities WHERE user_id = $1 AND provider = 'github'",
    )
    .bind(&account.user.id)
    .fetch_one(app.pool())
    .await
    .expect("counting must work");
    assert_eq!(linked, 1);

    app.cleanup().await;
}

#[tokio::test]
async fn linking_the_same_identity_twice_is_a_no_op_not_a_conflict() {
    let app = TestApp::start().await;
    let account = register(&app, "dave", "dave@example.com", "secret123").await;

    for _ in 0..2 {
        app.oauth()
            .script_github("4321", "dave", Some("dave@example.com"), true);

        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri("/auth/oauth/link")
            .header("content-type", "application/json")
            .header(
                "authorization",
                format!("Bearer {}", account.tokens.access_token),
            )
            .body(axum::body::Body::from(
                serde_json::to_vec(&json!({ "provider": GITHUB })).expect("body"),
            ))
            .expect("the request must build");
        let (_, _, body) = crate::support::send_full_body(&app, request).await;
        let state = query_of(&authorize_url(&body))
            .get("state")
            .and_then(Value::as_str)
            .expect("a state")
            .to_owned();

        let (status, body) = callback(&app, GITHUB, "github-code", &state).await;
        assert_eq!(status, StatusCode::OK, "linking again must succeed: {body}");
    }

    app.cleanup().await;
}

#[tokio::test]
async fn a_replayed_state_is_refused() {
    let app = TestApp::start().await;
    app.oauth()
        .script_github("1234", "replay", Some("replay@example.com"), true);
    let state = start_state(&app, GITHUB).await;

    let (status, body) = callback(&app, GITHUB, "github-code", &state).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the first callback must succeed: {body}"
    );

    // The same state again. One provider call was scripted, so a second exchange
    // would fail on the empty queue — but the state is spent before the provider
    // is even called, so the refusal happens first and no call is made.
    let (status, body) = callback(&app, GITHUB, "github-code", &state).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a replayed state must be refused: {body}"
    );
    assert_eq!(error_code(&body), "OAUTH_STATE_INVALID");
    assert_eq!(
        app.oauth().queued(),
        0,
        "the replay must be refused without spending a provider round trip"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_state_minted_for_one_provider_cannot_be_redeemed_against_another() {
    let app = TestApp::start().await;
    let state = start_state(&app, GITHUB).await;

    // No provider call is scripted: the mismatch must be caught before the
    // exchange, so an empty queue is itself the assertion that nothing was tried.
    let (status, body) = callback(&app, UNCONFIGURED, "some-code", &state).await;

    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::SERVICE_UNAVAILABLE,
        "a cross-provider state must not complete: {status} {body}"
    );
    assert_eq!(app.oauth().queued(), 0, "no provider call may be made");

    app.cleanup().await;
}

#[tokio::test]
async fn a_provider_that_is_not_configured_is_not_offered() {
    let app = TestApp::start().await;

    let (status, body) = crate::support::get_without_token(&app, "/auth/oauth/providers").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let providers = body["providers"]
        .as_array()
        .expect("the providers endpoint must answer an array");
    let named: Vec<&str> = providers
        .iter()
        .filter_map(|entry| entry["provider"].as_str())
        .collect();

    assert_eq!(
        named,
        ["github"],
        "only the provider with credentials may be offered: {body}"
    );

    // And asking for the unconfigured one directly is refused, rather than
    // trusting the client to only request what it was shown.
    let (status, body) = start(&app, UNCONFIGURED).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "an unconfigured provider must be refused: {body}"
    );
    assert_eq!(error_code(&body), "OAUTH_NOT_CONFIGURED");

    app.cleanup().await;
}

#[tokio::test]
async fn a_provider_refusal_does_not_leak_the_provider_error() {
    let app = TestApp::start().await;
    let state = start_state(&app, GITHUB).await;

    // The provider's own vocabulary, including a value that must never reach the
    // client.
    app.oauth().push_refusal(
        400,
        r#"{"error":"bad_verification_code","secret_hint":"gho_internal"}"#,
    );

    let (status, body) = callback(&app, GITHUB, "stale-code", &state).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(error_code(&body), "OAUTH_PROVIDER_ERROR");
    let rendered = body.to_string();
    assert!(
        !rendered.contains("bad_verification_code"),
        "the provider's error vocabulary must not be echoed: {rendered}"
    );
    assert!(
        !rendered.contains("secret_hint") && !rendered.contains("gho_internal"),
        "the provider's error body must not be echoed: {rendered}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn an_abandoned_username_step_can_be_resumed_with_the_same_identity() {
    let app = TestApp::start().await;
    let (first_token, _) =
        first_time_sign_in(&app, "31415", "abandoner", "abandoner@example.com").await;

    // The user never finishes. Signing in again must not leave them sealed behind
    // a token they lost — the provider identity still names the account, so a
    // fresh limited session is minted for it.
    app.oauth()
        .script_github("31415", "abandoner", Some("abandoner@example.com"), true);
    let state = start_state(&app, GITHUB).await;
    let resumed = callback_ok(&app, GITHUB, "github-code-2", &state).await;

    let onboarding = resumed
        .onboarding
        .expect("an unfinished account must return to the username step");
    assert_ne!(
        onboarding.limited_token, first_token,
        "a fresh limited session must be minted"
    );

    let (status, body) = complete_with(&app, &onboarding.limited_token, "abandoner").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = $1")
        .bind("abandoner@example.com")
        .fetch_one(app.pool())
        .await
        .expect("counting must work");
    assert_eq!(accounts, 1, "resuming must not create a second account");

    app.cleanup().await;
}

#[tokio::test]
async fn a_username_that_is_taken_is_retryable_with_the_same_limited_token() {
    let app = TestApp::start().await;
    register(&app, "taken_handle", "occupant@example.com", "secret123").await;

    let (limited_token, _) =
        first_time_sign_in(&app, "2718", "chooser", "chooser@example.com").await;

    let (status, body) = complete_with(&app, &limited_token, "taken_handle").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "USERNAME_TAKEN");

    // The failed attempt wrote nothing, so the same token still works.
    let (status, body) = complete_with(&app, &limited_token, "free_handle").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a lost username race must not burn the limited session: {body}"
    );
    let finished: OAuthCallbackResponse = serde_json::from_value(body).expect("contract");
    let session = finished.session.expect("a session");
    assert_eq!(session.user.username, "free_handle");

    app.cleanup().await;
}

#[tokio::test]
async fn a_username_that_breaks_the_rules_is_refused_with_a_field_problem() {
    let app = TestApp::start().await;
    let (limited_token, _) = first_time_sign_in(&app, "1618", "picky", "picky@example.com").await;

    for (username, expected_code) in [("ab", "TOO_SHORT"), ("Has Space", "INVALID_FORMAT")] {
        let (status, body) = complete_with(&app, &limited_token, username).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{username}` must be refused: {body}"
        );
        assert_eq!(error_code(&body), "VALIDATION_FAILED");
        assert_eq!(
            body["error"]["fields"][0]["code"].as_str(),
            Some(expected_code),
            "the field problem must name the rule that failed: {body}"
        );
        assert_eq!(
            body["error"]["fields"][0]["field"].as_str(),
            Some("username")
        );
    }

    // Still usable afterwards: a validation failure changes nothing.
    let (status, body) = complete_with(&app, &limited_token, "picky_user").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    app.cleanup().await;
}

#[tokio::test]
async fn a_limited_session_is_single_use() {
    let app = TestApp::start().await;
    let (limited_token, _) = first_time_sign_in(&app, "1414", "once", "once@example.com").await;

    let (status, body) = complete_with(&app, &limited_token, "once_user").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = complete_with(&app, &limited_token, "once_user_again").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the limited session must not be usable twice: {body}"
    );
    assert_eq!(error_code(&body), "OAUTH_STATE_INVALID");

    app.cleanup().await;
}

#[tokio::test]
async fn the_convertible_verifier_is_sent_to_the_token_endpoint() {
    let app = TestApp::start().await;
    app.oauth()
        .script_github("2020", "pkce", Some("pkce@example.com"), true);

    // One start, one callback: the challenge that went out and the verifier that
    // came back must belong to the same round trip.
    let (status, body) = start(&app, GITHUB).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let query = query_of(&authorize_url(&body));
    let challenge = query["code_challenge"]
        .as_str()
        .expect("a challenge")
        .to_owned();
    let state = query["state"].as_str().expect("a state").to_owned();

    let _ = callback(&app, GITHUB, "github-code", &state).await;

    // Exactly one provider call was made (the token exchange, plus the two
    // GitHub profile calls), and the token request carried a verifier whose
    // SHA-256 the challenge commits to. The verifier itself must not equal the
    // challenge — that would be `plain`, which is refused.
    let requests = app.oauth().requests();
    let token = requests
        .iter()
        .find(|request| request.endpoint == jiuyue_auth::oauth::OAuthEndpoint::Token)
        .expect("a token exchange must have been made");
    let form = crate::oauth_support::form_of(token);

    let verifier = form
        .get("code_verifier")
        .expect("the token request must carry the PKCE verifier");
    assert_ne!(
        verifier, &challenge,
        "the verifier must not be the challenge"
    );
    assert_eq!(
        jiuyue_auth::oauth::PkcePair::from_state_token(&state).challenge,
        challenge,
        "the challenge must be the S256 of the verifier that was sent"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_return_path_must_be_relative() {
    let app = TestApp::start().await;

    for hostile in [
        "https://evil.example/steal",
        "//evil.example",
        "http://localhost:5173@evil.example",
    ] {
        let (status, body) = post_json(
            &app,
            "/auth/oauth/start",
            &json!({ "provider": GITHUB, "return_to": hostile }),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{hostile}` must be refused: {body}"
        );
    }

    // A relative path is accepted and carried to the callback.
    app.oauth()
        .script_github("3030", "deep", Some("deep@example.com"), true);
    let (status, body) = post_json(
        &app,
        "/auth/oauth/start",
        &json!({ "provider": GITHUB, "return_to": "/chat/42" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let state = query_of(&authorize_url(&body))
        .get("state")
        .and_then(Value::as_str)
        .expect("a state")
        .to_owned();

    let response = callback_ok(&app, GITHUB, "github-code", &state).await;
    let onboarding = response.onboarding.expect("a first-time user");
    assert_eq!(
        onboarding.redirect_path.as_deref(),
        Some("/chat/42"),
        "the intent must survive the username step"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_google_identity_is_read_from_the_openid_profile() {
    let app = TestApp::start().await;

    // Google is not configured on the test instance, so this asserts the
    // refusal rather than the journey — the journey itself is unit-tested at the
    // adapter, which is as far as it can go without a live provider.
    let (status, body) = start(&app, UNCONFIGURED).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    let (status, body) = crate::support::get_without_token(&app, "/auth/oauth/providers").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.to_string().contains("google"),
        "an unconfigured Google must be absent from the login page: {body}"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn an_oauth_account_reports_a_profile_and_a_working_session() {
    let app = TestApp::start().await;
    let (limited_token, _) =
        first_time_sign_in(&app, "5150", "profiled", "profiled@example.com").await;
    let (_, body) = complete_with(&app, &limited_token, "profiled").await;
    let response: OAuthCallbackResponse = serde_json::from_value(body).expect("contract");
    let session = response.session.expect("a session");

    let (status, body) = get_with_token(&app, "/auth/me", &session.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let profile: UserProfile = serde_json::from_value(body).expect("a profile");
    assert_eq!(profile.username, "profiled");
    assert_eq!(profile.email, "profiled@example.com");
    assert!(
        profile.email_verified,
        "the provider asserted the address, so the account carries it"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn the_stored_state_and_verifier_are_digests_not_the_values() {
    let app = TestApp::start().await;
    let state = start_state(&app, GITHUB).await;

    let row = sqlx::query(
        "SELECT state_hash, code_verifier_hash, consumed_at FROM oauth_states WHERE state_hash = $1",
    )
    .bind(jiuyue_auth::hash_token(&state))
    .fetch_one(app.pool())
    .await
    .expect("the state must be stored");
    use sqlx::Row as _;

    let stored: String = row.get("state_hash");
    assert_eq!(stored.len(), 64);
    assert!(stored.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(
        !stored.contains(&state),
        "the raw state must not be recoverable from storage"
    );
    assert!(
        row.get::<Option<sqlx::types::time::OffsetDateTime>, _>("consumed_at")
            .is_none(),
        "a fresh state is unspent"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn a_link_attempt_without_a_session_is_refused() {
    let app = TestApp::start().await;

    let (status, body) = post_json(&app, "/auth/oauth/link", &json!({ "provider": GITHUB })).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "linking requires an authenticated account: {body}"
    );

    app.cleanup().await;
}
