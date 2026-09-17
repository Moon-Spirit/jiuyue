//! The third-party sign-in use cases.
//!
//! # The decision this module exists to get right
//!
//! When a provider asserts an email address that already belongs to an account,
//! there are three things this could do and only one of them is safe:
//!
//! - **Attach the provider to the existing account silently.** That is an
//!   account-take-over hole. Anyone who can make a provider assert an address —
//!   a provider that does not verify emails, a compromised provider account, a
//!   misconfigured tenant — walks into somebody else's account.
//! - **Create a second account with the same address.** A duplicate-account mess,
//!   and the unique constraint refuses it anyway.
//! - **Refuse, and tell the user to sign in their usual way and link from
//!   settings.** This one. See [`OAuthService::callback`], which returns
//!   [`AuthError::AccountExists`] and nothing else.
//!
//! The rule underneath it: **a provider identity is a credential for an account,
//! never a way to claim one by email.** A brand-new address through a provider
//! creates a new account — the provider is the authority on its own identity —
//! and that account's address is unverified unless the provider asserted
//! otherwise. A *matching* address is a refusal, and the test that asserts the
//! difference is `a_matching_email_never_hands_over_the_account`.
//!
//! # The limited session
//!
//! A first-time provider user has no username, and that is a real state rather
//! than an error. The account exists (the provider said who they are), but it
//! cannot be used until a handle is chosen.
//!
//! The limited session is an **opaque 256-bit token**, not a JWT. Nothing in the
//! API accepts it: every protected endpoint presents an access token and only
//! `Authorization: Bearer <JWT>` satisfies it, so the limited session is refused
//! everywhere by construction rather than by remembering to check a flag. Only
//! [`OAuthService::complete_sign_in`] redeems it, and redemption spends it before
//! the username write — the same one-shot protocol as `account_tokens`.
//!
//! An abandoned username step is recoverable: the provider identity still names
//! the account, so signing in with that provider again mints a *fresh* limited
//! token for the same account instead of sealing a username-less user away.

use std::sync::Arc;

use jiuyue_contract::auth::{CompleteOAuthSignInRequest, OAuthCallbackRequest};
use jiuyue_contract::{OAuthCallbackResponse, OAuthOnboarding, OAuthProvider, OAuthSession};
use sqlx::PgPool;

use crate::error::AuthError;
use crate::oauth::client::{OAuthIdentity, adapter_for, subject_fingerprint};
use crate::oauth::pkce::PkcePair;
use crate::oauth::provider::OAuthClient;
use crate::oauth::provider::OAuthProviders;
use crate::oauth::repository::{NewOAuthAccount, NewOAuthIdentity, NewOAuthState, OAuthRepository};
use crate::repository::{NewSession, SessionRepository, UserRow};
use crate::service::{AuthService, SessionContext, UNKNOWN_SOURCE, new_id, profile_from};
use crate::token::{hash_token, issue_opaque_token};
use crate::validation;
use jiuyue_contract::FieldErrorCode;

/// Third-party sign-in configuration.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    /// Configured providers.
    pub providers: OAuthProviders,
    /// How long a `state` (and its PKCE verifier) stays redeemable.
    pub state_ttl: std::time::Duration,
    /// How long a limited session may sit at the username step.
    pub onboarding_ttl: std::time::Duration,
}

impl OAuthConfig {
    /// A `state` lives minutes, not hours: the round trip is one browser
    /// redirect, so a `state` that is still redeemable an hour later is only a
    /// wider window for a replay.
    pub const DEFAULT_STATE_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

    /// A limited session lives long enough for a distracted user to come back,
    /// and no longer than a refresh token would.
    pub const DEFAULT_ONBOARDING_TTL: std::time::Duration =
        std::time::Duration::from_secs(24 * 60 * 60);

    /// Build with the documented lifetimes.
    pub fn new(providers: OAuthProviders) -> Self {
        Self {
            providers,
            state_ttl: Self::DEFAULT_STATE_TTL,
            onboarding_ttl: Self::DEFAULT_ONBOARDING_TTL,
        }
    }
}

/// A sign-in that completed, or an account that still needs a username.
///
/// The two are one enum rather than two optional fields so "a session with no
/// user" cannot be constructed by accident.
#[derive(Debug, Clone)]
pub enum CallbackOutcome {
    /// The caller is signed in.
    SignedIn {
        /// The authenticated user.
        user: UserRow,
        /// Which session was opened.
        session_id: String,
        /// The refresh token for the new session.
        refresh_token: String,
        /// Where the client asked to be returned to, when it asked.
        redirect_path: Option<String>,
    },
    /// The account exists but has no username yet.
    Onboarding(OAuthOnboarding),
}

/// The signature information the callback needs to open a session.
///
/// Passed in rather than read from a request so the service stays free of HTTP
/// concerns; the handler owns the headers.
#[derive(Debug, Clone)]
pub struct OAuthCallbackContext {
    /// The passwordless fingerprint of the caller's device metadata.
    pub session: SessionContext,
}

/// Third-party sign-in.
pub struct OAuthService {
    repository: OAuthRepository,
    sessions: SessionRepository,
    /// The transport. Behind the trait so a test drives the callback from a
    /// fixture instead of from `github.com`.
    client: Arc<dyn OAuthClient>,
    config: OAuthConfig,
    /// The public origin the redirect URI is built from; never a request header.
    public_base_url: String,
}

impl OAuthService {
    /// Assemble the service.
    pub fn new(
        pool: PgPool,
        client: Arc<dyn OAuthClient>,
        config: OAuthConfig,
        public_base_url: String,
    ) -> Self {
        Self {
            repository: OAuthRepository::new(pool.clone()),
            sessions: SessionRepository::new(pool),
            client,
            config,
            public_base_url,
        }
    }

    /// The providers this instance can actually drive.
    ///
    /// A provider with no credentials is absent from this list, which is what
    /// keeps a broken button off the login page.
    pub fn configured_providers(&self) -> Vec<OAuthProvider> {
        self.config
            .providers
            .configured()
            .into_iter()
            .map(|config| config.provider)
            .collect()
    }

    /// Begin a round trip at a provider and return the URL to send the browser to.
    ///
    /// `return_to` is a relative path the client would like to land on afterwards.
    /// It is refused when it is not one: a value that could become an absolute URL
    /// is how this endpoint would become an open redirect.
    pub async fn start(
        &self,
        provider: OAuthProvider,
        return_to: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<String, AuthError> {
        let config = self.config.providers.require(provider)?;

        let redirect_path = match return_to.map(str::trim).filter(|path| !path.is_empty()) {
            Some(path) if is_relative_path(path) => Some(path.to_owned()),
            Some(_) => {
                return Err(AuthError::Validation(vec![field(
                    "return_to",
                    FieldErrorCode::InvalidFormat,
                    "返回地址必须是站内相对路径",
                )]));
            }
            None => None,
        };

        let state_token = issue_opaque_token()?;
        // The verifier is derived from the state, so the one column that holds the
        // state's digest also holds the verifier — no second secret to persist.
        let pkce = PkcePair::from_state_token(&state_token);

        self.repository
            .insert_state(&NewOAuthState {
                id: &new_id(),
                state_hash: &hash_token(&state_token),
                provider,
                code_verifier_hash: &pkce.verifier,
                redirect_path: redirect_path.as_deref(),
                user_id,
                ttl_secs: self.config.state_ttl.as_secs_f64(),
            })
            .await?;

        config.authorization_url(&self.public_base_url, &state_token, &pkce.challenge)
    }

    /// Redeem a callback: resolve the provider identity and act on it.
    ///
    /// The order is deliberate. The `state` is spent **first**, before the
    /// provider is called, so a replayed callback cannot cause a second exchange
    /// — and so a `state` that is being replayed is refused without spending a
    /// provider round trip on it.
    pub async fn callback(
        &self,
        request: OAuthCallbackRequest,
        context: OAuthCallbackContext,
    ) -> Result<CallbackOutcome, AuthError> {
        // The callback records the device the browser came back on, but it does
        // not open the session here: a first-time user gets a limited session
        // first and only reaches a real one through `complete_sign_in`, which
        // carries its own device metadata. A returning user's session is opened
        // with this context, below.
        let device = context;
        let state = self
            .repository
            .consume_state(&hash_token(&request.state))
            .await?
            .ok_or(AuthError::OAuthStateInvalid)?;

        // The state names the provider, and the callback must agree. A state
        // minted for GitHub cannot be redeemed against a Google code even if a
        // client says so.
        let provider = request.provider;
        if state.provider != provider {
            tracing::warn!(
                expected = state.provider.as_str(),
                got = provider.as_str(),
                "OAuth callback named a provider the state was not issued for"
            );
            return Err(AuthError::OAuthStateInvalid);
        }

        let identity = self
            .exchange_identity(provider, &state.code_verifier_hash, &request.code)
            .await?;

        tracing::info!(
            provider = provider.as_str(),
            subject = %subject_fingerprint(provider, &identity.subject),
            "resolved a provider identity"
        );

        if let Some(user_id) = state.user_id {
            self.link(provider, &identity, &user_id).await?;

            return self.signed_in(user_id, state.redirect_path, device).await;
        }

        let Some(user) = self
            .repository
            .find_user_by_identity(provider, &identity.subject)
            .await?
        else {
            return self
                .create_account(provider, &identity, state.redirect_path)
                .await;
        };

        // The identity is already bound. Whether the account is still being
        // onboarded is asked of the one thing that knows: an outstanding limited
        // session. `password_hash IS NULL` does NOT answer it — a *finished*
        // OAuth account has no password either, and treating the two alike would
        // send a returning user back to the username step forever.
        let Some(pending_id) = self
            .repository
            .active_pending_session(provider, &identity.subject)
            .await?
        else {
            return self.signed_in(user.id, state.redirect_path, device).await;
        };

        self.resume_onboarding(provider, &identity, &user, state.redirect_path, &pending_id)
            .await
    }

    /// Finish a first-time provider sign-in by choosing a username.
    ///
    /// The limited token is the only thing that names the account, and it is spent
    /// in the same statement that claims the handle. A username that lost a race
    /// wrote nothing, so the same token may be used again — which is what makes a
    /// "that name is taken, try another" reply possible without a second round
    /// trip through the provider.
    pub async fn complete_sign_in(
        &self,
        limited_token: &str,
        request: CompleteOAuthSignInRequest,
        context: OAuthCallbackContext,
    ) -> Result<CallbackOutcome, AuthError> {
        let token = limited_token.trim();
        if token.is_empty() {
            return Err(AuthError::Unauthenticated);
        }

        let problems = validation::validate_username_choice(&request);
        if !problems.is_empty() {
            return Err(AuthError::Validation(problems));
        }

        let digest = hash_token(token);

        let username = validation::normalize_username(&request.username);
        let display_name = validation::display_name(request.display_name.as_deref(), &username);

        let user = self
            .repository
            .claim_username(&digest, &username, &display_name)
            .await?;

        tracing::info!(user_id = %user.id, "OAuth account finished onboarding");

        // The account is complete, so the session the limited token stood in for
        // can be a real one now. `context` is the completion request's metadata,
        // which is the request that actually opened the device.
        self.open_session(user, context, None).await
    }

    /// Link a provider identity to the account in `user_id`.
    ///
    /// Refused when the identity already belongs to somebody else: a provider
    /// identity belongs to exactly one account, and "link it to mine instead" is
    /// not a thing a request may ask for.
    async fn link(
        &self,
        provider: OAuthProvider,
        identity: &OAuthIdentity,
        user_id: &str,
    ) -> Result<(), AuthError> {
        match self
            .repository
            .identity_owner(provider, &identity.subject)
            .await?
        {
            // Already this account's identity: linking again is a no-op success,
            // not a conflict. The user asked for a state, and the state holds.
            Some(owner) if owner == user_id => Ok(()),
            Some(_) => Err(AuthError::OAuthIdentityTaken),
            None => {
                let linked = self
                    .repository
                    .link_identity(&NewOAuthIdentity {
                        id: &new_id(),
                        user_id,
                        provider,
                        subject: &identity.subject,
                        display_name: identity.display_name.as_deref(),
                        email: identity.email.as_deref(),
                        email_verified: identity.email_verified,
                    })
                    .await?;

                if linked {
                    tracing::info!(
                        user_id = %user_id,
                        provider = provider.as_str(),
                        "linked a provider identity"
                    );
                    Ok(())
                } else {
                    // Lost the unique index to a concurrent link.
                    Err(AuthError::OAuthIdentityTaken)
                }
            }
        }
    }

    /// Create a brand-new account for a first-time provider identity.
    ///
    /// The take-over guard runs before any write: if the address the provider
    /// reported already belongs to an account, no account is created and no
    /// identity is bound. The user is told to sign in their usual way and link
    /// from settings.
    async fn create_account(
        &self,
        provider: OAuthProvider,
        identity: &OAuthIdentity,
        redirect_path: Option<String>,
    ) -> Result<CallbackOutcome, AuthError> {
        if let Some(email) = identity.email.as_deref() {
            if self.repository.email_is_taken(email).await? {
                tracing::warn!(
                    provider = provider.as_str(),
                    "refused a provider sign-in: the address already belongs to an account"
                );
                return Err(AuthError::AccountExists);
            }
        }

        let base = suggested_username(identity, provider);
        let mut attempt = 0;

        // A generated handle is unique by retry, not by luck. The loop is bounded
        // because each attempt appends fresh randomness, so a run of collisions
        // is a scheduler anomaly rather than a case to loop on forever.
        loop {
            let username = generated_username(&base, attempt)?;
            let user_id = new_id();
            // A provider that reported no address still has to give the account
            // something for the NOT NULL unique column. The reserved `.invalid`
            // TLD (RFC 2606) can never be a real mailbox, so this is visibly a
            // placeholder rather than an address mail could one day reach.
            let email = identity
                .email
                .clone()
                .unwrap_or_else(|| format!("{username}@oauth.invalid"));
            let display_name = identity
                .display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| username.clone());
            let pending_id = new_id();
            let limited_token = issue_opaque_token()?;

            let account = NewOAuthAccount {
                user_id: &user_id,
                username: &username,
                email: Some(email.as_str()),
                display_name: &display_name,
                email_verified: identity.email_verified,
                identity: NewOAuthIdentity {
                    id: &new_id(),
                    user_id: &user_id,
                    provider,
                    subject: &identity.subject,
                    display_name: identity.display_name.as_deref(),
                    email: identity.email.as_deref(),
                    email_verified: identity.email_verified,
                },
                pending_id: &pending_id,
                pending_token_hash: &hash_token(&limited_token),
                pending_ttl_secs: self.config.onboarding_ttl.as_secs_f64(),
            };

            match self.repository.create_oauth_account(&account).await {
                Ok(user) => {
                    tracing::info!(
                        user_id = %user.id,
                        provider = provider.as_str(),
                        "created an account from a provider identity"
                    );

                    return Ok(CallbackOutcome::Onboarding(onboarding(
                        limited_token,
                        &username,
                        identity,
                        redirect_path.clone(),
                    )));
                }
                Err(AuthError::UsernameTaken) => {
                    attempt += 1;
                    if attempt >= MAX_USERNAME_ATTEMPTS {
                        tracing::error!(
                            base = %base,
                            "could not find a free handle for a new provider account"
                        );
                        return Err(AuthError::UsernameTaken);
                    }
                }
                // The email was taken between the guard and the insert. A race,
                // and the same refusal: never attach, never duplicate.
                Err(AuthError::EmailTaken) => return Err(AuthError::AccountExists),
                Err(error) => return Err(error),
            }
        }
    }

    /// Hand a returning, still-unfinished account a fresh limited session.
    ///
    /// The old token is retired as the new one is written, so exactly one live
    /// limited session exists per provider identity — a token the user lost in a
    /// crash cannot be replayed after they come back.
    async fn resume_onboarding(
        &self,
        provider: OAuthProvider,
        identity: &OAuthIdentity,
        user: &UserRow,
        redirect_path: Option<String>,
        previous: &str,
    ) -> Result<CallbackOutcome, AuthError> {
        let limited_token = issue_opaque_token()?;

        self.repository
            .consume_pending_session_by_id(previous)
            .await?;

        self.repository
            .mint_pending_session(
                &new_id(),
                &hash_token(&limited_token),
                provider,
                &identity.subject,
                &user.id,
                self.config.onboarding_ttl.as_secs_f64(),
            )
            .await?;

        tracing::info!(
            user_id = %user.id,
            "resumed an unfinished provider sign-in at the username step"
        );

        Ok(CallbackOutcome::Onboarding(onboarding(
            limited_token,
            &suggested_username(identity, provider),
            identity,
            redirect_path,
        )))
    }

    /// Exchange an authorization code for the provider's identity.
    ///
    /// The access token exists only inside this function's frame: it is used for
    /// the profile call and dropped. Nothing in this feature ever calls the
    /// provider again, so storing it would be a credential to leak that earns
    /// nothing.
    async fn exchange_identity(
        &self,
        provider: OAuthProvider,
        code_verifier_hash: &str,
        code: &str,
    ) -> Result<OAuthIdentity, AuthError> {
        if code.trim().is_empty() {
            return Err(AuthError::OAuthStateInvalid);
        }

        let config = self.config.providers.require(provider)?;
        let adapter = adapter_for(provider);

        // The raw verifier is not recoverable from its digest, so the caller
        // passes the value it presented and this verifies the commitment instead.
        let request =
            adapter.token_request(config, &self.public_base_url, code, code_verifier_hash)?;

        let response = self.client.send(request).await?;
        let access_token = adapter.access_token(&response)?;

        adapter.fetch_identity(&*self.client, &access_token).await
    }

    /// Open a session for an account and assemble the signed-in outcome.
    async fn signed_in(
        &self,
        user_id: String,
        redirect_path: Option<String>,
        context: OAuthCallbackContext,
    ) -> Result<CallbackOutcome, AuthError> {
        let user = self
            .repository
            .find_user_by_id(&user_id)
            .await?
            .ok_or(AuthError::AccountMissing)?;

        self.open_session(user, context, redirect_path).await
    }

    /// Open a real session: issue the refresh token, write the row, hand back the
    /// pieces the HTTP layer needs to sign the access token.
    async fn open_session(
        &self,
        user: UserRow,
        context: OAuthCallbackContext,
        redirect_path: Option<String>,
    ) -> Result<CallbackOutcome, AuthError> {
        let session_id = new_id();
        let refresh_token = issue_opaque_token()?;

        self.sessions
            .create_session(&NewSession {
                id: &session_id,
                user_id: &user.id,
                refresh_token_hash: &hash_token(&refresh_token),
                device_label: context.session.device_label.as_deref(),
                user_agent: context.session.user_agent.as_deref(),
                ip_address: context.session.ip_address.as_deref(),
                refresh_ttl_secs: refresh_ttl_secs(),
            })
            .await?;

        tracing::info!(user_id = %user.id, session_id = %session_id, "OAuth session opened");

        Ok(CallbackOutcome::SignedIn {
            user,
            session_id,
            refresh_token,
            redirect_path,
        })
    }
}

impl OAuthCallbackContext {
    /// No device metadata, for paths that have none to give.
    pub fn empty() -> Self {
        Self {
            session: SessionContext::default(),
        }
    }

    /// The source the device metadata resolves to, matching the login limiter.
    pub fn source(&self) -> &str {
        self.session.ip_address.as_deref().unwrap_or(UNKNOWN_SOURCE)
    }
}

/// How many generated handles are tried before giving up.
const MAX_USERNAME_ATTEMPTS: usize = 8;

/// Characters a generated suffix may use, matching `^[a-z0-9_]{3,32}$`.
const SUFFIX_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// A candidate handle built from a preferred base plus `attempt` characters of
/// randomness.
///
/// Every attempt is a fresh random suffix rather than a counter: a counter would
/// make "alice2" predictable and would collide again on the next concurrent
/// sign-up, while randomness makes the second collision vanishingly unlikely.
/// Once the base plus the suffix would exceed the 32-character limit, the base is
/// dropped entirely — a shorter handle than the provider suggested, but one that
/// is always valid.
fn generated_username(base: &str, attempt: usize) -> Result<String, AuthError> {
    let suffix_len = SUFFIX_DIGITS.saturating_add(attempt).min(MAX_SUFFIX);
    let suffix = random_suffix(suffix_len)?;

    let room = validation::USERNAME_MAX_CHARS.saturating_sub(suffix.len());
    let trimmed: String = base.chars().take(room).collect();
    let candidate = format!("{trimmed}{suffix}");

    // A base that is entirely unusable (a provider login of one character, for
    // instance) still has to yield a valid handle, and two characters of
    // randomness is not enough on its own — so pad to the minimum.
    if candidate.chars().count() < validation::USERNAME_MIN_CHARS {
        let padding = random_suffix(
            validation::USERNAME_MIN_CHARS.saturating_sub(candidate.chars().count()),
        )?;

        return Ok(format!("{candidate}{padding}"));
    }

    Ok(candidate)
}

/// Digits in the first suffix attempt: six characters of randomness.
const SUFFIX_DIGITS: usize = 6;

/// Largest suffix, so a handle can never exceed the 32-character maximum.
const MAX_SUFFIX: usize = 10;

/// `n` random characters from [`SUFFIX_ALPHABET`].
///
/// Drawn as bytes rather than by filtering a base64url string: filtering would
/// silently produce fewer characters than asked for, and a handle's randomness is
/// the thing keeping it from colliding.
fn random_suffix(n: usize) -> Result<String, AuthError> {
    let mut bytes = vec![0_u8; n];
    getrandom::fill(&mut bytes).map_err(AuthError::Random)?;

    Ok(bytes
        .into_iter()
        .map(|byte| char::from(SUFFIX_ALPHABET[usize::from(byte) % SUFFIX_ALPHABET.len()]))
        .collect())
}

/// The handle base suggested by a provider identity.
///
/// Presentation is a UI concern and the provider's login may be unusable as a
/// handle, so this is only a *preference*: the generated handle below always
/// satisfies the format, and the user chooses their real one at the next step.
fn suggested_username(identity: &OAuthIdentity, provider: OAuthProvider) -> String {
    let preferred = identity.display_name.as_deref().or_else(|| {
        identity
            .email
            .as_deref()
            .and_then(|email| email.split('@').next())
    });

    sanitise(preferred).unwrap_or_else(|| match provider {
        OAuthProvider::GitHub => "github".to_owned(),
        OAuthProvider::Google => "google".to_owned(),
    })
}

/// Reduce arbitrary provider text to the handle alphabet.
fn sanitise(value: Option<&str>) -> Option<String> {
    let cleaned: String = value?
        .chars()
        .map(|c| {
            let lower = c.to_ascii_lowercase();
            if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' {
                lower
            } else {
                '_'
            }
        })
        .take(validation::USERNAME_MAX_CHARS - SUFFIX_DIGITS)
        .collect();

    let collapsed = cleaned.trim_matches('_').to_owned();

    (!collapsed.is_empty()).then_some(collapsed)
}

/// The onboarding half of a callback response, shared by create and resume.
///
/// `redirect_path` travels with it so the client can honour it at the moment the
/// account finally becomes usable, rather than losing the intent at the step in
/// between.
fn onboarding(
    limited_token: String,
    suggested_username: &str,
    identity: &OAuthIdentity,
    redirect_path: Option<String>,
) -> OAuthOnboarding {
    OAuthOnboarding {
        limited_token,
        suggested_username: suggested_username.to_owned(),
        email: identity.email.clone(),
        redirect_path,
    }
}

/// Whether a return path is a relative path and not a URL.
///
/// `//evil.example` is protocol-relative, so a single leading slash is required
/// and a second one is refused. This is the open-redirect guard, and the database
/// has the same check so a path cannot be written around it.
fn is_relative_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains("://")
        && !path.chars().any(char::is_whitespace)
}

/// A refresh-token lifetime in seconds for a session opened by an OAuth sign-in.
///
/// Taken from the same constant `AuthService` uses, so an OAuth session and a
/// password session cannot end up with different lifetimes.
fn refresh_ttl_secs() -> f64 {
    f64::from(
        u32::try_from(crate::service::AuthConfig::DEFAULT_REFRESH_TOKEN_TTL.as_secs())
            .unwrap_or(u32::MAX),
    )
}

/// A field problem, for the validation this module owns.
fn field(name: &str, code: FieldErrorCode, message: &str) -> jiuyue_contract::FieldError {
    jiuyue_contract::FieldError {
        field: name.to_owned(),
        code,
        message: message.to_owned(),
    }
}

/// Assemble the signed-in half of a callback response.
pub fn session_outcome(
    auth: &AuthService,
    outcome: CallbackOutcome,
) -> Result<OAuthCallbackResponse, AuthError> {
    match outcome {
        CallbackOutcome::Onboarding(onboarding) => Ok(OAuthCallbackResponse {
            session: None,
            onboarding: Some(onboarding),
        }),
        CallbackOutcome::SignedIn {
            user,
            session_id,
            refresh_token,
            redirect_path,
        } => {
            let issued = auth.issue_access_token(&user.id, &session_id)?;

            Ok(OAuthCallbackResponse {
                session: Some(OAuthSession {
                    user: profile_from(user),
                    tokens: auth.token_pair(issued, refresh_token),
                    redirect_path,
                }),
                onboarding: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SUFFIX, OAuthConfig, generated_username, is_relative_path, random_suffix, sanitise,
        suggested_username,
    };
    use crate::oauth::client::OAuthIdentity;
    use crate::validation::{USERNAME_MAX_CHARS, USERNAME_MIN_CHARS, is_valid_username};
    use jiuyue_contract::OAuthProvider;

    fn identity(login: Option<&str>, email: Option<&str>) -> OAuthIdentity {
        OAuthIdentity {
            subject: "1".to_owned(),
            email: email.map(str::to_owned),
            email_verified: false,
            display_name: login.map(str::to_owned),
        }
    }

    #[test]
    fn generated_handles_are_always_valid_and_unique_between_attempts() {
        let first = generated_username("alice", 0).expect("minting");
        let second = generated_username("alice", 0).expect("minting");

        assert!(
            is_valid_username(&first),
            "{first} must satisfy the handle rule"
        );
        assert!(first.starts_with("alice"), "{first}");
        assert_ne!(first, second, "each attempt carries fresh randomness");
    }

    #[test]
    fn generated_handles_never_exceed_the_limit_even_from_a_long_base() {
        let long = "a".repeat(80);

        for attempt in [0, 3, 7] {
            let handle = generated_username(&long, attempt).expect("minting");
            assert!(
                handle.chars().count() <= USERNAME_MAX_CHARS,
                "attempt {attempt} produced {} characters: {handle}",
                handle.chars().count()
            );
            assert!(is_valid_username(&handle), "{handle}");
        }
    }

    #[test]
    fn generated_handles_meet_the_minimum_even_from_a_one_character_base() {
        let handle = generated_username("a", 0).expect("minting");

        assert!(
            handle.chars().count() >= USERNAME_MIN_CHARS,
            "a one-character base must still produce a valid handle: {handle}"
        );
        assert!(is_valid_username(&handle));
    }

    #[test]
    fn the_suggested_handle_is_a_preference_not_a_guarantee() {
        assert_eq!(
            suggested_username(&identity(Some("Octo-Cat"), None), OAuthProvider::GitHub),
            "octo_cat",
            "characters outside the alphabet become underscores"
        );
        assert_eq!(
            suggested_username(
                &identity(None, Some("Alice.B@example.com")),
                OAuthProvider::Google
            ),
            "alice_b",
            "the address local part is the fallback"
        );
        assert_eq!(
            suggested_username(&identity(Some("---"), None), OAuthProvider::GitHub),
            "github",
            "a provider name that sanitises to nothing falls back to the provider"
        );
        assert_eq!(
            suggested_username(&identity(None, None), OAuthProvider::Google),
            "google"
        );
        assert!(sanitise(None).is_none());
        assert!(sanitise(Some("___")).is_none());
    }

    #[test]
    fn a_suffix_is_never_longer_than_the_cap() {
        let suffix = random_suffix(MAX_SUFFIX).expect("minting");

        assert_eq!(suffix.len(), MAX_SUFFIX);
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn only_a_relative_path_is_accepted_as_a_return_target() {
        assert!(is_relative_path("/chat"));
        assert!(is_relative_path("/a/b?c=d"));

        assert!(!is_relative_path("https://evil.example"), "absolute");
        assert!(!is_relative_path("//evil.example"), "protocol-relative");
        assert!(
            !is_relative_path("chat"),
            "not rooted, so it cannot be resolved safely"
        );
        assert!(!is_relative_path("/a b"), "whitespace");
        assert!(
            !is_relative_path("/a://b"),
            "a scheme smuggled into the path"
        );
    }

    #[test]
    fn the_default_lifetimes_are_the_documented_ones() {
        let config = OAuthConfig::new(super::OAuthProviders::from_credentials(
            (None, None),
            (None, None),
        ));

        assert_eq!(config.state_ttl.as_secs(), 600);
        assert_eq!(config.onboarding_ttl.as_secs(), 86_400);
    }
}
