//! The provider adapters and the real HTTP client behind [`OAuthClient`].
//!
//! Each adapter is the *protocol* for one provider: how the token exchange is
//! shaped, what the profile response looks like, and how an identity is read out
//! of it. The HTTP call itself goes through [`OAuthClient`], so an adapter never
//! opens a socket and a test never needs one.
//!
//! # What is deliberately not here
//!
//! No provider access token or refresh token is kept. The exchange happens, the
//! identity is read, the token is dropped. Nothing in this feature calls the
//! provider on the user's behalf afterwards, so keeping a token would only be a
//! second credential to leak that earns nothing.
//!
//! # What this code has not been run against
//!
//! There is no outbound network in the environment this was written in, so the
//! two adapters are exercised through the [`OAuthClient`] seam against fixtures
//! and never against the live providers. The request shapes follow each
//! provider's published contract; the parsing is pinned by tests built from their
//! documented response bodies.

use std::time::Duration;

use jiuyue_contract::OAuthProvider;

use crate::error::AuthError;
use crate::oauth::provider::{
    OAuthClient, OAuthEndpoint, OAuthFuture, OAuthProviderConfig, OAuthRequest, OAuthResponse,
};
use crate::token::hash_token;

/// How long an outbound provider call may take.
///
/// Short on purpose: this is a request-path call inside a callback the user is
/// watching. A provider that cannot answer in ten seconds is a provider that will
/// not answer, and the user should be told so rather than left staring at a
/// spinner on a 2 vCPU box whose connection pool the call is holding.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The identity a provider asserted about an account.
///
/// The subject is the credential; everything else is either for display or a
/// hint. `email_verified` is recorded but, deliberately, not consulted by the
/// linking rule — an address a provider vouches for is still not proof that this
/// provider identity is the holder of an account we already have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthIdentity {
    /// The provider's immutable id for the account. Never derived from anything
    /// the user can change.
    pub subject: String,
    /// The address the provider reported, normalised to lowercase.
    pub email: Option<String>,
    /// Whether the provider asserted the address is verified.
    pub email_verified: bool,
    /// What the provider calls the account (login/name), for display.
    pub display_name: Option<String>,
}

/// The protocol of one provider: how to talk to it, and what it says back.
pub trait OAuthProviderAdapter: Send + Sync {
    /// Which provider this adapter speaks for.
    fn provider(&self) -> OAuthProvider;

    /// Build the token-exchange request for an authorization code.
    fn token_request(
        &self,
        config: &OAuthProviderConfig,
        public_base_url: &str,
        code: &str,
        code_verifier: &str,
    ) -> Result<OAuthRequest, AuthError>;

    /// Read an access token out of the token-endpoint response.
    fn access_token(&self, response: &OAuthResponse) -> Result<String, AuthError>;

    /// Build the profile request for an access token.
    fn profile_request(&self, access_token: &str) -> OAuthRequest;

    /// Read an identity out of a profile response, without any further calls.
    ///
    /// `Err(AuthError::OAuthProviderNeedsProfile)` means "this provider's identity
    /// cannot be read from one response" — GitHub's, whose address needs a second
    /// endpoint. The caller handles that by going through
    /// [`OAuthProviderAdapter::fetch_identity`] instead, which owns the sequencing.
    ///
    /// A provider answer that does not name an account is an error, never an empty
    /// identity that a caller might match by accident.
    fn identity(&self, response: &OAuthResponse) -> Result<OAuthIdentity, AuthError>;

    /// Read an identity, performing however many round trips the provider needs.
    ///
    /// The default is the one-call case. Only a provider whose identity genuinely
    /// spans two endpoints overrides it, which is why the seam is a method here
    /// and not a special case in the service.
    fn fetch_identity<'a>(
        &'a self,
        client: &'a dyn OAuthClient,
        access_token: &'a str,
    ) -> OAuthFuture<'a, Result<OAuthIdentity, AuthError>> {
        let request = self.profile_request(access_token);

        Box::pin(async move {
            let response = client.send(request).await?;
            self.identity(&response)
        })
    }
}

/// The GitHub adapter.
///
/// Two round trips: `/user` gives the id and login, and `/user/emails` gives the
/// address (the primary one, if GitHub marked one primary — otherwise the first
/// verified one, otherwise the first).
#[derive(Debug, Default, Clone, Copy)]
pub struct GitHubAdapter;

impl OAuthProviderAdapter for GitHubAdapter {
    fn provider(&self) -> OAuthProvider {
        OAuthProvider::GitHub
    }

    fn token_request(
        &self,
        config: &OAuthProviderConfig,
        public_base_url: &str,
        code: &str,
        code_verifier: &str,
    ) -> Result<OAuthRequest, AuthError> {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        serializer
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("client_id", config.require_client_id()?)
            .append_pair("client_secret", config.require_client_secret()?)
            .append_pair("redirect_uri", &config.redirect_uri(public_base_url))
            .append_pair("code_verifier", code_verifier);

        Ok(OAuthRequest {
            endpoint: OAuthEndpoint::Token,
            url: config.token_url.clone(),
            form: serializer.finish(),
            headers: vec![("Accept".to_owned(), "application/json".to_owned())],
        })
    }

    fn access_token(&self, response: &OAuthResponse) -> Result<String, AuthError> {
        let parsed = parse_response(response)?;
        let token = parsed
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty());

        token.map(str::to_owned).ok_or_else(|| {
            AuthError::OAuthProviderError(
                "the GitHub token response carried no access token".into(),
            )
        })
    }

    fn profile_request(&self, access_token: &str) -> OAuthRequest {
        github_request("/user", access_token)
    }

    fn identity(&self, response: &OAuthResponse) -> Result<OAuthIdentity, AuthError> {
        let value = parse_value(response)?;
        let (subject, display_name) = github_subject(&value)?;

        // `/user` carries an address only when the user published a public one,
        // which may be absent or stale. The email journey overrides this method
        // and uses `/user/emails`; this path is what a caller that only made the
        // single call gets, and it is honest about not knowing verification.
        let email = value
            .get("email")
            .and_then(serde_json::Value::as_str)
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty());

        Ok(OAuthIdentity {
            subject,
            email,
            email_verified: false,
            display_name,
        })
    }

    /// GitHub needs two calls: `/user` for the id, `/user/emails` for the address.
    fn fetch_identity<'a>(
        &'a self,
        client: &'a dyn OAuthClient,
        access_token: &'a str,
    ) -> OAuthFuture<'a, Result<OAuthIdentity, AuthError>> {
        Box::pin(async move {
            let profile = client.send(self.profile_request(access_token)).await?;
            let mut identity = self.identity(&profile)?;

            let emails = client
                .send(github_request("/user/emails", access_token))
                .await?;
            let body = parse_value(&emails)?;

            // The chosen address carries its own verification, so the flag
            // reported for the account is the one that belongs to the address
            // that was actually chosen — never a different entry's.
            if let Some(chosen) = pick_github_address(&body) {
                identity.email = Some(chosen.email);
                identity.email_verified = chosen.verified;
            }

            Ok(identity)
        })
    }
}

/// The address GitHub's email list should be read as, and whether GitHub
/// verified it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubAddress {
    /// The address, lowercased.
    pub email: String,
    /// Whether GitHub marked this entry verified.
    pub verified: bool,
}

/// A GitHub API request with the headers their API requires.
///
/// GitHub rejects an API call with no `User-Agent`, so it is always set; the
/// `Accept` pins the API version rather than riding the default.
fn github_request(path: &str, access_token: &str) -> OAuthRequest {
    OAuthRequest {
        endpoint: OAuthEndpoint::Profile,
        url: format!("https://api.github.com{path}"),
        form: String::new(),
        headers: vec![
            (
                "Accept".to_owned(),
                "application/vnd.github+json".to_owned(),
            ),
            ("Authorization".to_owned(), format!("Bearer {access_token}")),
            (
                "User-Agent".to_owned(),
                format!("jiuyue/{}", env!("CARGO_PKG_VERSION")),
            ),
        ],
    }
}

/// Read a GitHub address out of a `/user/emails` (or `/user`) body.
///
/// Preference order: GitHub's primary address, then a verified one, then the
/// first entry at all. Each candidate is taken whole — the address *and* its own
/// `verified` flag — so the account's verification state always describes the
/// address that was chosen rather than a neighbour's.
///
/// Public because the GitHub journey is two calls and the caller owns the merge.
pub fn pick_github_address(value: &serde_json::Value) -> Option<GithubAddress> {
    let entries = match value {
        serde_json::Value::Array(entries) => entries.as_slice(),
        serde_json::Value::Object(_) => std::slice::from_ref(value),
        _ => return None,
    };

    let choose = |predicate: fn(&serde_json::Value) -> bool| {
        entries
            .iter()
            .find(|entry| predicate(entry))
            .and_then(address_of)
    };

    choose(|entry| entry.get("primary").and_then(serde_json::Value::as_bool) == Some(true))
        .or_else(|| {
            choose(|entry| entry.get("verified").and_then(serde_json::Value::as_bool) == Some(true))
        })
        .or_else(|| choose(|_| true))
}

/// One entry of a GitHub email list, if it names a usable address.
fn address_of(entry: &serde_json::Value) -> Option<GithubAddress> {
    let email = entry
        .get("email")
        .and_then(serde_json::Value::as_str)?
        .trim()
        .to_lowercase();

    if email.is_empty() {
        return None;
    }

    Some(GithubAddress {
        email,
        verified: entry.get("verified").and_then(serde_json::Value::as_bool) == Some(true),
    })
}

/// Just the address a GitHub email list should be read as, for callers that do
/// not care whether GitHub verified it.
pub fn pick_github_email(value: &serde_json::Value) -> Option<String> {
    pick_github_address(value).map(|address| address.email)
}

/// The Google adapter.
///
/// One round trip: the token response carries an `id_token`, and the adapter reads
/// the identity out of the profile endpoint's JSON. This implementation reads the
/// UserInfo endpoint rather than verifying the ID token's signature, because the
/// token was just fetched over TLS straight from Google in this same call — a
/// signature check would be re-proving what the channel already proved. (If an ID
/// token is ever accepted from a client rather than fetched here, that reasoning
/// stops holding and the signature MUST be checked.)
#[derive(Debug, Default, Clone, Copy)]
pub struct GoogleAdapter;

impl OAuthProviderAdapter for GoogleAdapter {
    fn provider(&self) -> OAuthProvider {
        OAuthProvider::Google
    }

    fn token_request(
        &self,
        config: &OAuthProviderConfig,
        public_base_url: &str,
        code: &str,
        code_verifier: &str,
    ) -> Result<OAuthRequest, AuthError> {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        serializer
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("client_id", config.require_client_id()?)
            .append_pair("client_secret", config.require_client_secret()?)
            .append_pair("redirect_uri", &config.redirect_uri(public_base_url))
            .append_pair("code_verifier", code_verifier);

        Ok(OAuthRequest {
            endpoint: OAuthEndpoint::Token,
            url: config.token_url.clone(),
            form: serializer.finish(),
            headers: vec![("Accept".to_owned(), "application/json".to_owned())],
        })
    }

    fn access_token(&self, response: &OAuthResponse) -> Result<String, AuthError> {
        let parsed = parse_response(response)?;
        parsed
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                AuthError::OAuthProviderError(
                    "the Google token response carried no access token".into(),
                )
            })
    }

    fn profile_request(&self, access_token: &str) -> OAuthRequest {
        OAuthRequest {
            endpoint: OAuthEndpoint::Profile,
            url: "https://openidconnect.googleapis.com/v1/userinfo".to_owned(),
            form: String::new(),
            headers: vec![("Authorization".to_owned(), format!("Bearer {access_token}"))],
        }
    }

    fn identity(&self, response: &OAuthResponse) -> Result<OAuthIdentity, AuthError> {
        let parsed = parse_response(response)?;

        let subject = parsed
            .get("sub")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AuthError::OAuthProviderError("the Google profile carried no subject".into())
            })?;

        let email = parsed
            .get("email")
            .and_then(serde_json::Value::as_str)
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty());

        // Google reports `email_verified` as a JSON boolean in UserInfo. A string
        // "true" is accepted too: it is what the ID token carries, and refusing it
        // would make the two representations disagree.
        let email_verified = parsed
            .get("email_verified")
            .map(|value| {
                value.as_bool() == Some(true) || value.as_str().is_some_and(|text| text == "true")
            })
            .unwrap_or(false);

        let display_name = parsed
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty());

        Ok(OAuthIdentity {
            subject: subject.to_owned(),
            email,
            email_verified,
            display_name,
        })
    }
}

/// Build the adapter for a provider.
pub fn adapter_for(provider: OAuthProvider) -> Box<dyn OAuthProviderAdapter> {
    match provider {
        OAuthProvider::GitHub => Box::new(GitHubAdapter),
        OAuthProvider::Google => Box::new(GoogleAdapter),
    }
}

/// Read the GitHub subject and display name out of a `/user` body.
///
/// The subject is `id` — an integer that never changes — not `login`, which a user
/// can rename and another user can reclaim.
pub fn github_subject(value: &serde_json::Value) -> Result<(String, Option<String>), AuthError> {
    let subject = value
        .get("id")
        .and_then(|id| match id {
            serde_json::Value::Number(number) => Some(number.to_string()),
            serde_json::Value::String(text) => Some(text.clone()),
            _ => None,
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AuthError::OAuthProviderError("the GitHub profile carried no account id".into())
        })?;

    let display_name = value
        .get("login")
        .or_else(|| value.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty());

    Ok((subject, display_name))
}

/// Parse a provider response, refusing a non-2xx or unparseable one.
///
/// The provider's own error text is put into the log by the caller that catches
/// this and never into an HTTP response — see [`AuthError::OAuthProviderError`].
fn parse_response(response: &OAuthResponse) -> Result<serde_json::Value, AuthError> {
    if !response.is_success() {
        return Err(AuthError::OAuthProviderError(format!(
            "the provider answered HTTP {}",
            response.status
        )));
    }

    serde_json::from_str(&response.body)
        .map_err(|_| AuthError::OAuthProviderError("the provider answered unparseable JSON".into()))
}

/// Parse a provider response that must be JSON (a profile body).
fn parse_value(response: &OAuthResponse) -> Result<serde_json::Value, AuthError> {
    parse_response(response)
}

/// The real HTTP client.
///
/// The only implementation of [`OAuthClient`] that ships. Everything above it is
/// transport-agnostic, which is what lets the callback journeys run in CI with no
/// network at all.
#[derive(Debug, Clone)]
pub struct HttpOAuthClient {
    http: reqwest::Client,
}

impl HttpOAuthClient {
    /// Build a client with the documented timeout.
    pub fn new() -> Result<Self, AuthError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| {
                tracing::error!(%error, "could not build the OAuth HTTP client");
                AuthError::OAuthProviderError("HTTP client construction failed".into())
            })?;

        Ok(Self { http })
    }
}

impl OAuthClient for HttpOAuthClient {
    fn send<'a>(
        &'a self,
        request: OAuthRequest,
    ) -> OAuthFuture<'a, Result<OAuthResponse, AuthError>> {
        Box::pin(async move {
            let mut builder = self
                .http
                .post(&request.url)
                .header("Content-Type", "application/x-www-form-urlencoded");

            if !request.form.is_empty() {
                builder = builder.body(request.form.clone());
            }

            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }

            let response = builder.send().await.map_err(|error| {
                // The transport cause is logged here and never returned: it can
                // carry the full URL, which for the token endpoint is not secret
                // but for a profile call would leak an access token in a query.
                tracing::warn!(endpoint = ?request.endpoint, %error, "OAuth provider call failed");
                AuthError::OAuthProviderError("the provider could not be reached".into())
            })?;

            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();

            let body = response.text().await.map_err(|error| {
                tracing::warn!(%error, "OAuth provider response body could not be read");
                AuthError::OAuthProviderError("the provider's response could not be read".into())
            })?;

            if status < 300 && !content_type.contains("json") {
                // A form-encoded body from a token endpoint. Converted to the
                // same shape the adapters parse, so the adapter never has to know
                // which encoding arrived — and a profile body that is somehow not
                // JSON is left alone, to fail honestly at parse time.
                if let Some(normalised) = normalise_form_body(&body) {
                    return Ok(OAuthResponse {
                        status,
                        body: normalised,
                    });
                }
            }

            Ok(OAuthResponse { status, body })
        })
    }
}

/// Re-encode an `application/x-www-form-urlencoded` body as the JSON object the
/// adapters parse.
///
/// `None` for a body with no pairs at all, which is not a token response and
/// should fall through to the adapter's own (failing) parse rather than becoming
/// a successful empty object.
fn normalise_form_body(body: &str) -> Option<String> {
    let parsed: serde_json::Map<String, serde_json::Value> =
        form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| {
                (
                    key.into_owned(),
                    serde_json::Value::String(value.into_owned()),
                )
            })
            .collect();

    (!parsed.is_empty()).then(|| serde_json::Value::Object(parsed).to_string())
}

/// The digest of a provider subject, for a log line that must not carry the
/// provider's own identifier verbatim.
pub(crate) fn subject_fingerprint(provider: OAuthProvider, subject: &str) -> String {
    hash_token(&format!("{}:{subject}", provider.as_str()))
}

#[cfg(test)]
mod tests {
    use super::{
        GitHubAdapter, GoogleAdapter, OAuthProviderAdapter, pick_github_address, pick_github_email,
    };
    use crate::oauth::provider::{OAuthEndpoint, OAuthProviderConfig, OAuthRequest, OAuthResponse};
    use jiuyue_contract::OAuthProvider;

    fn config(provider: OAuthProvider) -> OAuthProviderConfig {
        OAuthProviderConfig::unconfigured(provider)
            .with_credentials(Some("id".to_owned()), Some("secret".to_owned()))
    }

    fn ok(body: &str) -> OAuthResponse {
        OAuthResponse {
            status: 200,
            body: body.to_owned(),
        }
    }

    #[test]
    fn the_github_token_request_carries_the_verifier_and_the_fixed_redirect() {
        let request = GitHubAdapter
            .token_request(
                &config(OAuthProvider::GitHub),
                "https://jiuyue.example/",
                "the-code",
                "the-verifier",
            )
            .expect("a configured provider must build a request");

        assert_eq!(request.endpoint, OAuthEndpoint::Token);
        assert_eq!(request.url, super::super::provider::GITHUB_TOKEN_URL);
        let form: std::collections::HashMap<_, _> = form_urlencoded::parse(request.form.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("authorization_code")
        );
        assert_eq!(form.get("code").map(String::as_str), Some("the-code"));
        assert_eq!(
            form.get("code_verifier").map(String::as_str),
            Some("the-verifier")
        );
        assert_eq!(
            form.get("redirect_uri").map(String::as_str),
            Some("https://jiuyue.example/auth/oauth/callback")
        );
        assert_eq!(
            form.get("client_secret").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn a_github_token_response_yields_the_token() {
        let json = ok(r#"{"access_token":"gho_x","token_type":"bearer"}"#);
        assert_eq!(GitHubAdapter.access_token(&json).expect("json"), "gho_x");
    }

    #[test]
    fn a_form_encoded_token_body_is_normalised_into_the_json_shape() {
        // GitHub answers the token endpoint as `application/x-www-form-urlencoded`
        // when it does not honour an `Accept: application/json`, so the client
        // normalises it before any adapter sees it. This asserts the normalisation
        // itself, because it is what makes the adapter's one parse path sufficient.
        let normalised =
            super::normalise_form_body("access_token=gho_y&token_type=bearer&scope=read%3Auser")
                .expect("a form body must normalise");

        let parsed: serde_json::Value =
            serde_json::from_str(&normalised).expect("the normalised body must be JSON");
        assert_eq!(parsed["access_token"], "gho_y");
        assert_eq!(
            parsed["scope"], "read:user",
            "percent-encoded values must be decoded"
        );

        assert!(
            super::normalise_form_body("").is_none(),
            "an empty body is not a token, so it must fall through to the adapter"
        );
        assert!(
            super::normalise_form_body("not a form body at all").is_some(),
            "any non-empty form-shaped body normalises"
        );
    }

    #[test]
    fn a_provider_error_response_is_never_an_identity() {
        let denied = OAuthResponse {
            status: 400,
            body: r#"{"error":"bad_verification_code"}"#.to_owned(),
        };

        assert!(
            GitHubAdapter.access_token(&denied).is_err(),
            "a non-2xx must not yield a token"
        );

        let empty = ok(r#"{"error":"no token here"}"#);
        assert!(
            GitHubAdapter.access_token(&empty).is_err(),
            "a 200 with no access token is still a provider failure"
        );
    }

    #[test]
    fn the_github_address_preference_is_primary_then_verified_then_first() {
        let primary = serde_json::json!([
            {"email":"secondary@example.com","primary":false,"verified":true},
            {"email":"Primary@Example.com","primary":true,"verified":true}
        ]);
        let chosen = pick_github_address(&primary).expect("a primary address");
        assert_eq!(
            chosen.email, "primary@example.com",
            "the primary address wins and is normalised"
        );
        assert!(chosen.verified, "and its own flag comes with it");

        let verified_only = serde_json::json!([
            {"email":"unverified@example.com","primary":false,"verified":false},
            {"email":"verified@example.com","primary":false,"verified":true}
        ]);
        assert_eq!(
            pick_github_email(&verified_only).as_deref(),
            Some("verified@example.com")
        );

        let neither = serde_json::json!([
            {"email":"first@example.com","primary":false,"verified":false},
            {"email":"second@example.com","primary":false,"verified":false}
        ]);
        assert_eq!(
            pick_github_email(&neither).as_deref(),
            Some("first@example.com")
        );

        assert_eq!(pick_github_email(&serde_json::json!([])), None);
        assert_eq!(pick_github_email(&serde_json::json!({"email":""})), None);
    }

    #[test]
    fn github_reports_the_verification_of_the_chosen_address_not_a_neighbour() {
        // The primary address is unverified while another entry is verified. The
        // account must be reported unverified: reporting true here would claim
        // GitHub vouched for an address it did not.
        let body = serde_json::json!([
            {"email":"verified@example.com","primary":false,"verified":true},
            {"email":"primary@example.com","primary":true,"verified":false}
        ]);

        let chosen = pick_github_address(&body).expect("a primary address");

        assert_eq!(chosen.email, "primary@example.com");
        assert!(
            !chosen.verified,
            "verification must describe the address that was chosen"
        );
    }

    #[tokio::test]
    async fn the_github_email_journey_merges_both_calls_and_reports_verification() {
        use crate::oauth::provider::{OAuthEndpoint, OAuthRequest, OAuthResponse};

        /// Answers each GitHub endpoint from a fixed body.
        struct TwoCallClient;

        impl crate::oauth::provider::OAuthClient for TwoCallClient {
            fn send<'a>(
                &'a self,
                request: OAuthRequest,
            ) -> crate::oauth::provider::OAuthFuture<
                'a,
                Result<OAuthResponse, crate::error::AuthError>,
            > {
                Box::pin(async move {
                    let body = if request.url.ends_with("/user/emails") {
                        r#"[{"email":"octocat@example.com","primary":true,"verified":true}]"#
                    } else {
                        r#"{"id":583231,"login":"octocat","email":null}"#
                    };
                    assert_eq!(request.endpoint, OAuthEndpoint::Profile);

                    Ok(OAuthResponse {
                        status: 200,
                        body: body.to_owned(),
                    })
                })
            }
        }

        let identity = GitHubAdapter
            .fetch_identity(&TwoCallClient, "gho_test")
            .await
            .expect("the two-call journey must produce an identity");

        assert_eq!(identity.subject, "583231", "the subject comes from /user");
        assert_eq!(
            identity.email.as_deref(),
            Some("octocat@example.com"),
            "the address comes from /user/emails"
        );
        assert!(identity.email_verified);
    }

    #[test]
    fn the_github_subject_is_the_numeric_id_not_the_login() {
        let profile = serde_json::json!({"id": 583231, "login": "octocat", "name": "The Octocat"});

        let (subject, name) = super::github_subject(&profile).expect("a profile with an id");

        assert_eq!(subject, "583231", "the id is stable; the login is not");
        assert_eq!(name.as_deref(), Some("octocat"));
    }

    #[test]
    fn a_github_profile_without_an_id_is_refused() {
        assert!(super::github_subject(&serde_json::json!({"login": "octocat"})).is_err());
    }

    #[test]
    fn the_google_identity_reads_the_subject_the_address_and_its_verification() {
        let profile = ok(
            r#"{"sub":"1122334455","email":"Alice@Example.com","email_verified":true,"name":"Alice"}"#,
        );

        let identity = GoogleAdapter
            .identity(&profile)
            .expect("a valid Google profile");

        assert_eq!(identity.subject, "1122334455");
        assert_eq!(identity.email.as_deref(), Some("alice@example.com"));
        assert!(identity.email_verified);
        assert_eq!(identity.display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn a_google_profile_without_a_subject_is_refused() {
        let profile = ok(r#"{"email":"alice@example.com","email_verified":true}"#);

        assert!(
            GoogleAdapter.identity(&profile).is_err(),
            "an identity with no stable subject must not be usable"
        );
    }

    #[test]
    fn google_reports_an_unverified_address_as_unverified() {
        let profile = ok(r#"{"sub":"1","email":"alice@example.com"}"#);

        let identity = GoogleAdapter.identity(&profile).expect("a valid profile");

        assert!(
            !identity.email_verified,
            "an absent email_verified is false, never assumed true"
        );
        assert_eq!(identity.email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn the_google_token_request_uses_the_fixed_redirect() {
        let request = GoogleAdapter
            .token_request(
                &config(OAuthProvider::Google),
                "https://jiuyue.example",
                "c",
                "v",
            )
            .expect("configured");

        assert_eq!(request.url, super::super::provider::GOOGLE_TOKEN_URL);
        let form: std::collections::HashMap<_, _> = form_urlencoded::parse(request.form.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            form.get("redirect_uri").map(String::as_str),
            Some("https://jiuyue.example/auth/oauth/callback")
        );
    }

    #[test]
    fn a_subject_fingerprint_does_not_carry_the_subject() {
        let fingerprint = super::subject_fingerprint(OAuthProvider::GitHub, "583231");

        assert_eq!(fingerprint.len(), 64, "a hex SHA-256 digest");
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(fingerprint, "583231");
        assert!(
            !fingerprint.contains("583231"),
            "the provider's identifier must not survive into a log line"
        );
        assert_ne!(
            fingerprint,
            super::subject_fingerprint(OAuthProvider::Google, "583231"),
            "the same subject on two providers is two identities"
        );
    }

    #[test]
    fn the_adapter_for_each_provider_speaks_for_that_provider() {
        assert_eq!(
            super::adapter_for(OAuthProvider::GitHub).provider(),
            OAuthProvider::GitHub
        );
        assert_eq!(
            super::adapter_for(OAuthProvider::Google).provider(),
            OAuthProvider::Google
        );
    }

    #[test]
    fn a_profile_request_targets_the_profile_endpoint_with_a_bearer_header() {
        let request: OAuthRequest = GoogleAdapter.profile_request("token-value");

        assert_eq!(request.endpoint, OAuthEndpoint::Profile);
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer token-value")
        );
    }
}
