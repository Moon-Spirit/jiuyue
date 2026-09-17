//! Third-party sign-in: GitHub and Google.
//!
//! # The one decision that matters
//!
//! **A provider identity is a credential for an account, never a way to claim one
//! by email.**
//!
//! The provider's `subject` — its immutable id for the account — is the only
//! stable identity. It is what `oauth_identities` is keyed on, what makes the
//! same provider identity land on the same account twice, and what makes it
//! impossible for a provider to walk into an account by asserting an address it
//! happens to know.
//!
//! A provider email that matches an existing account **refuses**. It does not
//! link (that is the account-take-over hole: anyone who can make a provider
//! assert an address could enter the account it belongs to), and it does not
//! create a second account with the same address (a duplicate mess the unique
//! constraint refuses anyway). The user is told to sign in the way they normally
//! do and link the provider from settings. See
//! [`OAuthService::callback`](service::OAuthService::callback).
//!
//! # The limited session
//!
//! A first-time provider user has no username, and that is a real state rather
//! than an error. Until they choose one, they hold a **limited session**: an
//! opaque 256-bit token, not a JWT. No protected endpoint accepts it — every one
//! of them requires an access token, and the two are not interchangeable — so it
//! is refused everywhere by construction. Only
//! [`OAuthService::complete_sign_in`](service::OAuthService::complete_sign_in)
//! redeems it, and redemption spends it before the username write.
//!
//! # What is stored, and what is not
//!
//! - The provider's identity: `(provider, subject)`, plus its display name and
//!   reported address for the account-holder to read.
//! - `state` and the PKCE verifier, as SHA-256 digests (`oauth_states`).
//! - A limited-session token, as a SHA-256 digest (`oauth_pending_sessions`).
//! - **No provider access or refresh token.** The exchange happens, the identity
//!   is read, the token is dropped. Nothing here calls the provider again on the
//!   user's behalf, so keeping one would be a second credential to leak that
//!   earns nothing.
//!
//! # The seam
//!
//! [`OAuthClient`](provider::OAuthClient) is the only thing that touches the
//! network. The service speaks it and nothing else, so the callback journeys are
//! driven end to end from fixtures — which is also the honest limit of what has
//! been exercised: see the module docs on the adapters for what has not been run
//! against a live provider.

pub mod client;
pub mod pkce;
pub mod provider;
pub mod repository;
pub mod service;

pub use client::{
    GitHubAdapter, GithubAddress, GoogleAdapter, HttpOAuthClient, OAuthIdentity,
    OAuthProviderAdapter, adapter_for, github_subject, pick_github_address, pick_github_email,
};
pub use pkce::{CHALLENGE_METHOD, PkcePair};
pub use provider::{
    GITHUB_AUTHORIZE_URL, GITHUB_SCOPES, GITHUB_TOKEN_URL, GOOGLE_AUTHORIZE_URL, GOOGLE_SCOPES,
    GOOGLE_TOKEN_URL, OAuthClient, OAuthEndpoint, OAuthFuture, OAuthProviderConfig, OAuthProviders,
    OAuthRequest, OAuthResponse,
};
pub use repository::{
    NewOAuthAccount, NewOAuthIdentity, NewOAuthState, OAuthRepository, StoredOAuthState,
};
pub use service::{
    CallbackOutcome, OAuthCallbackContext, OAuthConfig, OAuthService, session_outcome,
};
