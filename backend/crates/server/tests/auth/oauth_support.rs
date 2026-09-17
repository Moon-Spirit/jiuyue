//! The fake provider transport, and the response builders the OAuth journeys use.
//!
//! There is no outbound network in the environment these tests run in, and there
//! is no sandbox for `github.com` or Google anyway. So the provider is faked at
//! the [`OAuthClient`] seam — the one place that touches the wire — and everything
//! above it is the real code: the real router, the real service, the real
//! database, the real redirect, `state` and PKCE handling.
//!
//! The fake is a *queue*, not a stub that returns whatever the test wants: each
//! call consumes the next scripted response, so a flow that makes one call too
//! many fails on an empty queue instead of silently succeeding. It also records
//! every request, which is how the PKCE verifier and the fixed `redirect_uri` are
//! asserted from the outside.
//!
//! What this cannot prove is that the real providers agree with the request
//! shapes — see the module docs on the adapters for that boundary.

use std::sync::Mutex;

use jiuyue_auth::oauth::{OAuthClient, OAuthFuture, OAuthProviders, OAuthRequest, OAuthResponse};
use jiuyue_contract::OAuthProvider;

/// The provider transport, answering from a script.
///
/// The only thing recorded is the request: a test asserts on what was *sent* (the
/// PKCE verifier, the fixed redirect, one call per endpoint), never on the script
/// it supplied, which would be asserting the fixture against itself. The count of
/// calls is asserted through [`FakeOAuthClient::queued`] — a flow that makes one
/// call more than it was scripted for fails on the empty queue instead of getting
/// a default answer.
#[derive(Debug, Default)]
pub struct FakeOAuthClient {
    queued: Mutex<Vec<OAuthResponse>>,
    requests: Mutex<Vec<OAuthRequest>>,
}

impl FakeOAuthClient {
    /// An empty script.
    pub fn new() -> Self {
        Self::default()
    }

    /// Script the next response the transport will hand back.
    pub fn push(&self, response: OAuthResponse) {
        self.lock_queued().push(response);
    }

    /// Script a 200 answer with the given body.
    pub fn push_body(&self, body: &str) {
        self.push(OAuthResponse {
            status: 200,
            body: body.to_owned(),
        });
    }

    /// Script a provider refusal with the given status.
    ///
    /// The body is the provider's own error vocabulary, which the API must never
    /// echo to the client.
    pub fn push_refusal(&self, status: u16, body: &str) {
        self.push(OAuthResponse {
            status,
            body: body.to_owned(),
        });
    }

    /// A GitHub round trip: the token, then the profile, then the emails.
    pub fn script_github(&self, subject: &str, login: &str, email: Option<&str>, verified: bool) {
        self.push_body(r#"{"access_token":"gho_test","token_type":"bearer","scope":"read:user"}"#);
        self.push_body(&format!(
            r#"{{"id":{subject},"login":"{login}","name":"{login}","email":null}}"#
        ));
        self.push_body(&github_emails(email, verified));
    }

    /// Every request the transport has served, oldest first.
    pub fn requests(&self) -> Vec<OAuthRequest> {
        self.lock_requests().clone()
    }

    /// How many provider calls are still unanswered in the script.
    pub fn queued(&self) -> usize {
        self.lock_queued().len()
    }

    fn lock_queued(&self) -> std::sync::MutexGuard<'_, Vec<OAuthResponse>> {
        self.queued
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn lock_requests(&self) -> std::sync::MutexGuard<'_, Vec<OAuthRequest>> {
        self.requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl OAuthClient for FakeOAuthClient {
    fn send<'a>(
        &'a self,
        request: OAuthRequest,
    ) -> OAuthFuture<'a, Result<OAuthResponse, jiuyue_auth::AuthError>> {
        Box::pin(async move {
            self.lock_requests().push(request);

            // An exhausted script is a real failure, not a default answer: a flow
            // that called the provider more times than it was scripted for has a
            // bug, and answering anyway would hide it.
            let response = self.lock_queued().remove(0);

            Ok(response)
        })
    }
}

/// The parameters of one form-encoded request body.
pub fn form_of(request: &OAuthRequest) -> std::collections::HashMap<String, String> {
    form_urlencoded::parse(request.form.as_bytes())
        .into_owned()
        .collect()
}

/// A GitHub `/user/emails` body.
pub fn github_emails(email: Option<&str>, verified: bool) -> String {
    match email {
        Some(email) => serde_json::json!([{
            "email": email,
            "primary": true,
            "verified": verified,
        }])
        .to_string(),
        None => "[]".to_owned(),
    }
}

/// The provider set the test instance is built with.
///
/// GitHub is configured; Google deliberately is not. That asymmetry is the
/// fixture for "a provider with no credentials is absent from the login page":
/// the same instance has one of each.
pub fn credentialed_providers() -> OAuthProviders {
    OAuthProviders::from_credentials(
        (
            Some("github-client-id".to_owned()),
            Some("github-client-secret".to_owned()),
        ),
        (None, None),
    )
}

/// The provider that has no credentials on the test instance.
pub const UNCONFIGURED: OAuthProvider = OAuthProvider::Google;
