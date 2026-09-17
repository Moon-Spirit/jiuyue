//! Which providers exist, what they are called, and how a request is sent to one.
//!
//! Two things live here, and they are separate on purpose:
//!
//! - [`OAuthProviderConfig`] is *deployment* data: a client id, a secret, the two
//!   endpoints, the scopes. It comes from the environment through the server's
//!   `Config`, and `jiuyue-auth` never reads the environment itself.
//! - [`OAuthClient`] is the *wire*. The service speaks this trait and nothing
//!   else, so the HTTP client can be replaced in a test with something that
//!   answers from a fixture instead of from `github.com`. That seam is what makes
//!   the callback journeys testable end to end without a network.
//!
//! A provider with no client id or secret is **not configured**. It is absent
//! from the list the login page renders, and asking for it is refused. That is
//! deliberately different from offering a button that fails on click.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use jiuyue_contract::OAuthProvider;

use crate::error::AuthError;

/// GitHub's authorization endpoint.
pub const GITHUB_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
/// GitHub's token endpoint.
pub const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
/// Google's OpenID Connect authorization endpoint.
pub const GOOGLE_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's token endpoint.
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// GitHub scopes: the account id and the primary email.
pub const GITHUB_SCOPES: &[&str] = &["read:user", "user:email"];
/// Google scopes: the subject id, the address, and whether it is verified.
pub const GOOGLE_SCOPES: &[&str] = &["openid", "email", "profile"];

/// One provider, as this deployment is configured to use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProviderConfig {
    /// Which provider.
    pub provider: OAuthProvider,
    /// The OAuth application's client id. `None` means "not configured here".
    pub client_id: Option<String>,
    /// The OAuth application's client secret. Never leaves the server.
    pub client_secret: Option<String>,
    /// Where the browser is sent to authorize.
    pub authorize_url: String,
    /// Where the code is exchanged for a token (server to server).
    pub token_url: String,
    /// What the authorization request asks for.
    pub scopes: Vec<String>,
}

impl OAuthProviderConfig {
    /// The documented endpoints and scopes for a provider, with no credentials.
    ///
    /// Credentials are attached separately so the "not configured" state is
    /// structural: an endpoint set with no secret is simply not offered.
    pub fn unconfigured(provider: OAuthProvider) -> Self {
        match provider {
            OAuthProvider::GitHub => Self {
                provider,
                client_id: None,
                client_secret: None,
                authorize_url: GITHUB_AUTHORIZE_URL.to_owned(),
                token_url: GITHUB_TOKEN_URL.to_owned(),
                scopes: strings(GITHUB_SCOPES),
            },
            OAuthProvider::Google => Self {
                provider,
                client_id: None,
                client_secret: None,
                authorize_url: GOOGLE_AUTHORIZE_URL.to_owned(),
                token_url: GOOGLE_TOKEN_URL.to_owned(),
                scopes: strings(GOOGLE_SCOPES),
            },
        }
    }

    /// Attach credentials, keeping whatever is blank absent.
    pub fn with_credentials(
        mut self,
        client_id: Option<String>,
        client_secret: Option<String>,
    ) -> Self {
        self.client_id = non_blank(client_id);
        self.client_secret = non_blank(client_secret);
        self
    }

    /// Whether this instance can actually drive this provider.
    ///
    /// Both halves are required. A client id with no secret cannot complete the
    /// token exchange, so it is not configured — and it will not be offered.
    pub fn is_configured(&self) -> bool {
        self.client_id.is_some() && self.client_secret.is_some()
    }

    /// The client id, or a configuration error when this provider is not set up.
    pub fn require_client_id(&self) -> Result<&str, AuthError> {
        self.client_id
            .as_deref()
            .ok_or(AuthError::OAuthNotConfigured)
    }

    /// The client secret, or a configuration error when this provider is not set up.
    pub fn require_client_secret(&self) -> Result<&str, AuthError> {
        self.client_secret
            .as_deref()
            .ok_or(AuthError::OAuthNotConfigured)
    }

    /// The `redirect_uri` the provider will send the browser back to.
    ///
    /// Built from the deployment's public origin, never from a request header: a
    /// `redirect_uri` a caller could influence is how an authorization code gets
    /// delivered somewhere else.
    pub fn redirect_uri(&self, public_base_url: &str) -> String {
        let base = public_base_url.trim_end_matches('/');

        format!("{base}/auth/oauth/callback")
    }

    /// Assemble the authorization URL: endpoint plus the standard parameters.
    ///
    /// Every value is percent-encoded through `form_urlencoded`, so a scope or a
    /// redirect URI containing a reserved character cannot break out of its
    /// parameter and inject another.
    pub fn authorization_url(
        &self,
        public_base_url: &str,
        state: &str,
        code_challenge: &str,
    ) -> Result<String, AuthError> {
        let client_id = self.require_client_id()?;

        let mut serializer = form_urlencoded::Serializer::new(String::new());
        serializer
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &self.redirect_uri(public_base_url))
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", code_challenge)
            .append_pair(
                "code_challenge_method",
                crate::oauth::pkce::CHALLENGE_METHOD,
            );

        if self.provider == OAuthProvider::Google {
            // `offline` is what Google requires before it will consider a refresh
            // token; sign-in itself does not need one, and asking for it costs
            // nothing but a field.
            serializer.append_pair("access_type", "offline");
        }

        Ok(format!("{}?{}", self.authorize_url, serializer.finish()))
    }
}

/// Every provider this process knows about, configured or not.
#[derive(Debug, Clone, Default)]
pub struct OAuthProviders {
    by_provider: BTreeMap<&'static str, OAuthProviderConfig>,
}

impl OAuthProviders {
    /// Build from the per-provider credentials a deployment supplied.
    pub fn from_credentials(
        github: (Option<String>, Option<String>),
        google: (Option<String>, Option<String>),
    ) -> Self {
        let mut by_provider = BTreeMap::new();

        for provider in [OAuthProvider::GitHub, OAuthProvider::Google] {
            let (client_id, client_secret) = match provider {
                OAuthProvider::GitHub => github.clone(),
                OAuthProvider::Google => google.clone(),
            };

            by_provider.insert(
                provider.as_str(),
                OAuthProviderConfig::unconfigured(provider)
                    .with_credentials(client_id, client_secret),
            );
        }

        Self { by_provider }
    }

    /// The configuration for a provider, configured or not.
    pub fn get(&self, provider: OAuthProvider) -> Option<&OAuthProviderConfig> {
        self.by_provider.get(provider.as_str())
    }

    /// The configuration for a provider that this instance can drive.
    ///
    /// A provider that is known but has no credentials is refused here, not just
    /// filtered out of the offered list. Relying on the client to only ask for
    /// what it was shown would mean a hand-made request could drive a provider
    /// with no client secret — and the refusal must be the server's decision.
    pub fn require(&self, provider: OAuthProvider) -> Result<&OAuthProviderConfig, AuthError> {
        self.get(provider)
            .filter(|config| config.is_configured())
            .ok_or(AuthError::OAuthNotConfigured)
    }

    /// Only the providers that have credentials, in the fixed provider order.
    ///
    /// This is what `GET /auth/oauth/providers` renders from: a provider with no
    /// secret is absent rather than present and broken.
    pub fn configured(&self) -> Vec<&OAuthProviderConfig> {
        self.by_provider
            .values()
            .filter(|config| config.is_configured())
            .collect()
    }

    /// Whether any provider at all is usable on this instance.
    pub fn any_configured(&self) -> bool {
        self.configured()
            .iter()
            .any(|config| config.is_configured())
    }
}

/// A POST to a provider endpoint, already encoded.
///
/// The body is assembled by the caller because the token endpoint and the
/// profile endpoint take different parameters, and the seams that matter here are
/// "what URL", "what form body" and "was the response judged good" — not a second
/// HTTP abstraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthRequest {
    /// The one endpoint this request is allowed to reach.
    pub endpoint: OAuthEndpoint,
    /// Absolute URL to POST to.
    pub url: String,
    /// Already percent-encoded `application/x-www-form-urlencoded` body.
    pub form: String,
    /// Extra request headers (an `Accept`, an `Authorization` for Google's
    /// profile call).
    pub headers: Vec<(String, String)>,
}

/// Which provider endpoint a request is addressed to.
///
/// Carried explicitly so an adapter cannot confuse the token endpoint with the
/// profile endpoint when reading a response, and so a test seam can answer
/// without parsing the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthEndpoint {
    /// The token exchange (`application/x-www-form-urlencoded`).
    Token,
    /// The provider's own account/profile lookup.
    Profile,
}

/// A provider's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthResponse {
    /// HTTP status.
    pub status: u16,
    /// Raw response body.
    pub body: String,
}

impl OAuthResponse {
    /// Whether the provider answered 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A boxed, sendable future; the trait must be object-safe behind `Arc<dyn …>`.
pub type OAuthFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The outbound HTTP seam.
///
/// The only implementation that ships is [`HttpOAuthClient`] (in `client`), which
/// wraps `reqwest`; tests inject one that answers from a fixture. Nothing above
/// this trait knows the HTTP library, and nothing above it may retry or inspect a
/// raw socket.
pub trait OAuthClient: Send + Sync {
    /// Perform one request and return the provider's answer verbatim.
    ///
    /// A transport failure is [`AuthError::OAuthProviderError`], never a panic and
    /// never a silent success.
    fn send<'a>(
        &'a self,
        request: OAuthRequest,
    ) -> OAuthFuture<'a, Result<OAuthResponse, AuthError>>;
}

fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_owned())
        .filter(|trimmed| !trimmed.is_empty())
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::OAuthProviderConfig;
    use jiuyue_contract::OAuthProvider;

    #[test]
    fn a_provider_without_both_credentials_is_not_configured() {
        let bare = OAuthProviderConfig::unconfigured(OAuthProvider::GitHub);
        assert!(!bare.is_configured(), "no credentials at all");

        let half = OAuthProviderConfig::unconfigured(OAuthProvider::GitHub)
            .with_credentials(Some("client".to_owned()), None);
        assert!(
            !half.is_configured(),
            "a client id with no secret cannot complete the token exchange"
        );

        let blank_secret = OAuthProviderConfig::unconfigured(OAuthProvider::GitHub)
            .with_credentials(Some("client".to_owned()), Some("   ".to_owned()));
        assert!(!blank_secret.is_configured(), "a blank secret is absent");

        let complete = OAuthProviderConfig::unconfigured(OAuthProvider::GitHub)
            .with_credentials(Some(" client ".to_owned()), Some(" secret ".to_owned()));
        assert!(complete.is_configured());
        assert_eq!(
            complete.client_id.as_deref(),
            Some("client"),
            "surrounding whitespace must be trimmed"
        );
    }

    #[test]
    fn only_configured_providers_are_offered() {
        let providers = super::OAuthProviders::from_credentials(
            (Some("gh-id".to_owned()), Some("gh-secret".to_owned())),
            (None, None),
        );

        let offered = providers.configured();
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].provider, OAuthProvider::GitHub);
        assert!(
            providers.get(OAuthProvider::Google).is_some(),
            "the unconfigured provider is still known, just not offered"
        );
        assert!(
            !providers
                .get(OAuthProvider::Google)
                .expect("known")
                .is_configured()
        );
    }

    #[test]
    fn asking_for_an_unconfigured_provider_is_an_explicit_refusal() {
        let providers = super::OAuthProviders::from_credentials((None, None), (None, None));

        assert!(providers.configured().is_empty());
        assert!(providers.require(OAuthProvider::Google).is_err());
        assert!(matches!(
            providers
                .require(OAuthProvider::Google)
                .expect_err("must refuse"),
            crate::error::AuthError::OAuthNotConfigured
        ));
    }

    #[test]
    fn the_authorization_url_encodes_every_parameter() {
        let config = OAuthProviderConfig::unconfigured(OAuthProvider::GitHub)
            .with_credentials(Some("a b&c".to_owned()), Some("secret".to_owned()));

        let url = config
            .authorization_url("https://jiuyue.example/", "state value", "challenge+/=")
            .expect("a configured provider must assemble a URL");

        assert!(
            url.starts_with("https://github.com/login/oauth/authorize?"),
            "{url}"
        );
        assert!(
            url.contains("client_id=a+b%26c"),
            "a reserved character in the client id must be escaped: {url}"
        );
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Fjiuyue.example%2Fauth%2Foauth%2Fcallback"),
            "the redirect URI must be built from the configured origin: {url}"
        );
        assert!(url.contains("state=state+value"));
        assert!(url.contains("code_challenge=challenge%2B%2F%3D"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope=read%3Auser+user%3Aemail"));
        assert!(
            !url.contains("access_type"),
            "access_type is a Google parameter, not a GitHub one"
        );
    }

    #[test]
    fn google_asks_for_offline_access_and_the_openid_scopes() {
        let config = OAuthProviderConfig::unconfigured(OAuthProvider::Google)
            .with_credentials(Some("id".to_owned()), Some("secret".to_owned()));

        let url = config
            .authorization_url("https://jiuyue.example", "s", "c")
            .expect("configured");

        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("scope=openid+email+profile"));
    }

    #[test]
    fn the_redirect_uri_never_doubles_a_slash() {
        let config = OAuthProviderConfig::unconfigured(OAuthProvider::GitHub);

        assert_eq!(
            config.redirect_uri("https://jiuyue.example/"),
            "https://jiuyue.example/auth/oauth/callback"
        );
        assert_eq!(
            config.redirect_uri("https://jiuyue.example"),
            "https://jiuyue.example/auth/oauth/callback"
        );
    }
}
