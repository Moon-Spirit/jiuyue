//! The identity use cases.
//!
//! [`AuthService`] is the whole surface the HTTP layer needs: register, log in,
//! refresh, log out, resolve an access token, and run the two email journeys
//! (verify an address, reset a password). It composes the narrower pieces
//! ([`crate::validation`], [`crate::password`], [`crate::token`],
//! [`crate::links`], [`crate::mailer`]) and the repositories, so a handler never
//! has to know how a password is hashed, how a link is minted, or how mail is
//! delivered.
//!
//! # Email never blocks a request
//!
//! Issuing a link writes one row (our own database, fast) and then hands the
//! message to [`Mailer`] on a spawned task. A slow SMTP provider therefore cannot
//! make registration or a password-reset request hang; the request returns as soon
//! as the row is committed.
//!
//! # The endpoints that must not be an account oracle
//!
//! `forgot_password` and `resend_verification` answer identically whether or not
//! the address exists, and do the same database work either way: the link insert
//! is `INSERT … SELECT … WHERE EXISTS (user)`, so a miss writes no row while
//! executing the same statements. The rate limiter keys on the *source*, never the
//! address, so a throttled answer cannot be told apart either.

use std::sync::Arc;
use std::time::Duration;

use jiuyue_contract::auth::{
    ForgotPasswordRequest, ResendVerificationRequest, ResetPasswordRequest,
};
use jiuyue_contract::{AuthSession, LoginRequest, RegisterRequest, TokenPair, UserProfile};
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;

use crate::account_tokens::{AccountTokenRepository, NewToken, TokenPurpose};
use crate::error::AuthError;
use crate::limiter::{
    InProcessLoginAttemptStore, LoginAttempt, LoginAttemptPolicy, LoginAttemptStore,
};
use crate::links;
use crate::mailer::{InMemoryMailer, Mailer, OutgoingMessage};
use crate::password::PasswordHasher;
use crate::repository::{NewAccount, NewSession, SessionRepository, UserRow};
use crate::token::{IssuedAccess, TokenIssuer};
use crate::validation;

/// How the identity module is configured.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// HS256 signing secret. Comes from configuration; never defaulted.
    pub jwt_secret: String,
    /// Access-token lifetime.
    pub access_token_ttl: Duration,
    /// Refresh-token (session) lifetime.
    pub refresh_token_ttl: Duration,
    /// Public base URL the frontend is served from, used to build the links inside
    /// verification and reset emails (for example `https://jiuyue.example`).
    pub public_base_url: String,
    /// How long an email-verification link stays valid.
    pub verification_token_ttl: Duration,
    /// How long a password-reset link stays valid.
    pub reset_token_ttl: Duration,
}

impl AuthConfig {
    /// 15 minutes: short enough that a leaked access token is not a lasting
    /// credential, long enough that clients are not refreshing constantly.
    pub const DEFAULT_ACCESS_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

    /// 30 days: the "stay signed in" horizon, and the session's hard expiry.
    pub const DEFAULT_REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

    /// 24 hours: a verification link survives a workday and an overnight sleep,
    /// but is not a permanent token sitting in an old inbox.
    pub const DEFAULT_VERIFICATION_TOKEN_TTL: Duration = Duration::from_secs(24 * 60 * 60);

    /// 1 hour: a reset link is used within minutes of being requested, so it does
    /// not need a long life — and a shorter one limits a leaked inbox.
    pub const DEFAULT_RESET_TOKEN_TTL: Duration = Duration::from_secs(60 * 60);

    /// Where links point when no deployment URL is configured. The Vite dev
    /// server's default origin, so a developer can click the link in the log.
    pub const DEFAULT_PUBLIC_BASE_URL: &'static str = "http://localhost:5173";

    /// Configuration with the documented default lifetimes and dev base URL.
    pub fn new(jwt_secret: impl Into<String>) -> Self {
        Self {
            jwt_secret: jwt_secret.into(),
            access_token_ttl: Self::DEFAULT_ACCESS_TOKEN_TTL,
            refresh_token_ttl: Self::DEFAULT_REFRESH_TOKEN_TTL,
            public_base_url: Self::DEFAULT_PUBLIC_BASE_URL.to_owned(),
            verification_token_ttl: Self::DEFAULT_VERIFICATION_TOKEN_TTL,
            reset_token_ttl: Self::DEFAULT_RESET_TOKEN_TTL,
        }
    }
}

/// Where a login came from, for the device list a later ticket will add.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionContext {
    /// Client-supplied device label.
    pub device_label: Option<String>,
    /// Observed `User-Agent`.
    pub user_agent: Option<String>,
    /// Observed client address (honours the proxy's forwarding header).
    pub ip_address: Option<String>,
}

impl SessionContext {
    /// The key attempts are counted under.
    ///
    /// This is the *source*, never the account: keying on the email would make a
    /// throttled response differ for a registered and an unregistered address,
    /// which is exactly the enumeration oracle the limiter exists to avoid. An
    /// unknown source collapses to one shared bucket, which is the honest
    /// fallback — better than an unbounded key or no limiting at all.
    pub fn source(&self) -> &str {
        self.ip_address
            .as_deref()
            .filter(|address| !address.is_empty())
            .unwrap_or(UNKNOWN_SOURCE)
    }
}

/// Key used when the client address could not be determined.
pub const UNKNOWN_SOURCE: &str = "unknown";

/// A resolved access token: who the caller is and which session proved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedSession {
    /// ULID of the authenticated User.
    pub user_id: String,
    /// ULID of the Session the token belongs to.
    pub session_id: String,
    /// The user's `@handle`.
    pub username: String,
}

/// Assembles an [`AuthService`] from the seams a deployment chooses.
///
/// The defaults ([`InProcessLoginAttemptStore`] for both limiter buckets,
/// [`InMemoryMailer`] for delivery) are the single-node / development choices. A
/// test or a production deployment overrides the ones it cares about and leaves
/// the rest.
pub struct AuthServiceBuilder {
    pool: PgPool,
    config: AuthConfig,
    login_attempts: Option<Arc<dyn LoginAttemptStore>>,
    email_attempts: Option<Arc<dyn LoginAttemptStore>>,
    mailer: Option<Arc<dyn Mailer>>,
}

impl AuthServiceBuilder {
    /// Count login attempts in the given store.
    pub fn login_store(mut self, store: Arc<dyn LoginAttemptStore>) -> Self {
        self.login_attempts = Some(store);
        self
    }

    /// Count mail-triggering requests (forgot-password, resend-verification) in the
    /// given store.
    ///
    /// Deliberately a *separate* bucket from login: a burst of reset requests from
    /// a shared address must not lock out sign-ins, and vice versa.
    pub fn email_store(mut self, store: Arc<dyn LoginAttemptStore>) -> Self {
        self.email_attempts = Some(store);
        self
    }

    /// Deliver mail through the given transport.
    pub fn mailer(mut self, mailer: Arc<dyn Mailer>) -> Self {
        self.mailer = Some(mailer);
        self
    }

    /// Build the service, hashing one throwaway password as the timing equaliser.
    pub async fn build(self) -> Result<AuthService, AuthError> {
        let hasher = PasswordHasher::new();
        let tokens = TokenIssuer::new(
            &self.config.jwt_secret,
            self.config.access_token_ttl,
            self.config.refresh_token_ttl,
        )?;
        let dummy_hash = hasher.hash("jiuyue-timing-equaliser").await?;

        let login_attempts = self.login_attempts.unwrap_or_else(default_attempt_store);
        let email_attempts = self.email_attempts.unwrap_or_else(default_attempt_store);
        let mailer = self
            .mailer
            .unwrap_or_else(|| Arc::new(InMemoryMailer::new()));

        Ok(AuthService {
            repository: SessionRepository::new(self.pool.clone()),
            account_tokens: AccountTokenRepository::new(self.pool),
            hasher,
            tokens,
            login_attempts,
            email_attempts,
            mailer,
            public_base_url: self.config.public_base_url,
            verification_token_ttl: self.config.verification_token_ttl,
            reset_token_ttl: self.config.reset_token_ttl,
            dummy_hash,
        })
    }
}

/// The process-local limiter with the production policy.
fn default_attempt_store() -> Arc<dyn LoginAttemptStore> {
    Arc::new(InProcessLoginAttemptStore::new(
        LoginAttemptPolicy::default(),
    ))
}

/// The identity module's public API.
pub struct AuthService {
    repository: SessionRepository,
    account_tokens: AccountTokenRepository,
    hasher: PasswordHasher,
    tokens: TokenIssuer,
    /// The login limiter. Held behind the trait so a multi-node deployment swaps
    /// in a shared store without touching this file (see [`crate::limiter`]).
    login_attempts: Arc<dyn LoginAttemptStore>,
    /// The limiter guarding the endpoints that send mail, kept separate from the
    /// login bucket so one cannot exhaust the other.
    email_attempts: Arc<dyn LoginAttemptStore>,
    /// Where mail is handed off. Sending is spawned, never awaited here.
    mailer: Arc<dyn Mailer>,
    /// Base URL for the links inside emails.
    public_base_url: String,
    /// Verification-link lifetime.
    verification_token_ttl: Duration,
    /// Reset-link lifetime.
    reset_token_ttl: Duration,
    /// A real hash of a throwaway password, used to make "no such account" cost
    /// the same as "wrong password" (see [`AuthService::absorb_timing`]).
    dummy_hash: String,
}

impl AuthService {
    /// Build the service with the documented defaults.
    pub async fn new(pool: PgPool, config: AuthConfig) -> Result<Self, AuthError> {
        Self::builder(pool, config).build().await
    }

    /// Start assembling a service, overriding only the seams that differ.
    pub fn builder(pool: PgPool, config: AuthConfig) -> AuthServiceBuilder {
        AuthServiceBuilder {
            pool,
            config,
            login_attempts: None,
            email_attempts: None,
            mailer: None,
        }
    }

    /// Build the service with a chosen login-attempt store.
    ///
    /// This is the seam the Redis-backed store will be injected through, and the
    /// one tests use to run with tiny windows instead of production thresholds.
    pub async fn with_login_store(
        pool: PgPool,
        config: AuthConfig,
        login_attempts: Arc<dyn LoginAttemptStore>,
    ) -> Result<Self, AuthError> {
        Self::builder(pool, config)
            .login_store(login_attempts)
            .build()
            .await
    }

    /// Create an account, sign the new device in, and email a verification link.
    ///
    /// Email verification is not required to *hold* the account: it is usable
    /// immediately and simply carries `email_verified: false` until a link is
    /// followed. Opening a Conversation is the one gated action.
    pub async fn register(
        &self,
        request: RegisterRequest,
        context: SessionContext,
    ) -> Result<AuthSession, AuthError> {
        let problems = validation::validate_registration(&request);
        if !problems.is_empty() {
            return Err(AuthError::Validation(problems));
        }

        let username = validation::normalize_username(&request.username);
        let email = validation::normalize_email(&request.email);
        let display_name = validation::display_name(request.display_name.as_deref(), &username);
        let password_hash = self.hasher.hash(&request.password).await?;

        let user_id = new_id();
        let session_id = new_id();
        let refresh_token = self.tokens.issue_refresh_token()?;
        let refresh_token_hash = TokenIssuer::hash_refresh_token(&refresh_token);

        let account = NewAccount {
            id: &user_id,
            username: &username,
            email: &email,
            password_hash: &password_hash,
            display_name: &display_name,
        };
        let session = NewSession {
            id: &session_id,
            user_id: &user_id,
            refresh_token_hash: &refresh_token_hash,
            device_label: context.device_label.as_deref(),
            user_agent: context.user_agent.as_deref(),
            ip_address: context.ip_address.as_deref(),
            refresh_ttl_secs: self.refresh_ttl_secs(),
        };

        let user = self.repository.create_account(&account, &session).await?;
        tracing::info!(user_id = %user.id, session_id = %session_id, "account registered");

        // The account exists and the caller is signed in; a failure to queue the
        // verification mail must not undo either. It is logged and can be retried
        // through `resend_verification`.
        self.issue_verification_link(&user).await;

        self.finish_sign_in(user, &session_id, refresh_token)
    }

    /// Verify credentials and open a new session (a new Device).
    ///
    /// The limiter is consulted *before* anything else — before validation and,
    /// above all, before any credential work — so a throttled source costs the
    /// server almost nothing and cannot be used to tell a registered address from
    /// an unregistered one.
    pub async fn login(
        &self,
        request: LoginRequest,
        context: SessionContext,
    ) -> Result<AuthSession, AuthError> {
        let source = context.source();
        refuse_while_limited(&*self.login_attempts, source).await?;

        let problems = validation::validate_login(&request);
        if !problems.is_empty() {
            return Err(AuthError::Validation(problems));
        }

        let email = validation::normalize_email(&request.email);
        let user = self.repository.find_user_by_email(&email).await?;

        let Some(user) = user else {
            self.absorb_timing(&request.password).await?;
            return Err(self.reject(source).await);
        };

        // OAuth-only accounts have no password, so there is nothing to verify.
        let Some(password_hash) = user.password_hash.clone() else {
            self.absorb_timing(&request.password).await?;
            return Err(self.reject(source).await);
        };

        if !self
            .hasher
            .verify(&request.password, &password_hash)
            .await?
        {
            tracing::debug!(user_id = %user.id, "login rejected: password mismatch");
            return Err(self.reject(source).await);
        }

        // A successful sign-in forgives the source. A user who mistyped twice and
        // then remembered their password must not carry those failures forward.
        self.login_attempts.record_success(source).await;

        let session_id = new_id();
        let refresh_token = self.tokens.issue_refresh_token()?;
        let refresh_token_hash = TokenIssuer::hash_refresh_token(&refresh_token);

        let session = NewSession {
            id: &session_id,
            user_id: &user.id,
            refresh_token_hash: &refresh_token_hash,
            device_label: context.device_label.as_deref(),
            user_agent: context.user_agent.as_deref(),
            ip_address: context.ip_address.as_deref(),
            refresh_ttl_secs: self.refresh_ttl_secs(),
        };

        self.repository.create_session(&session).await?;
        tracing::info!(user_id = %user.id, session_id = %session_id, "session opened");

        self.finish_sign_in(user, &session_id, refresh_token)
    }

    /// Trade a refresh token for a fresh access token.
    ///
    /// The refresh token is not rotated yet — that is the device ticket's job.
    /// What matters now is that the session must still be active: a revoked
    /// session cannot refresh.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenPair, AuthError> {
        let digest = TokenIssuer::hash_refresh_token(refresh_token);
        let session = self
            .repository
            .find_active_session_by_token_hash(&digest)
            .await?
            .ok_or(AuthError::Unauthenticated)?;

        let issued = self.tokens.issue_access(&session.user_id, &session.id)?;
        self.repository.touch_session(&session.id).await?;

        Ok(self.token_pair(issued, refresh_token.to_owned()))
    }

    /// Revoke the session an access token belongs to.
    ///
    /// Revocation is a single row update; because `authenticate` re-reads that row,
    /// the next request with the same access token fails. No cache stands between
    /// the logout and its effect.
    pub async fn logout(&self, access_token: &str) -> Result<(), AuthError> {
        let claims = self.tokens.verify_access(access_token)?;
        self.repository.revoke_session(&claims.sid).await?;
        tracing::info!(session_id = %claims.sid, "session revoked");

        Ok(())
    }

    /// Resolve an access token to its session, requiring the session to be active
    /// and to belong to the user the token names.
    pub async fn authenticate(
        &self,
        access_token: &str,
    ) -> Result<AuthenticatedSession, AuthError> {
        let claims = self.tokens.verify_access(access_token)?;

        let session = self
            .repository
            .find_active_session_by_id(&claims.sid)
            .await?
            .filter(|session| session.user_id == claims.sub)
            .ok_or(AuthError::Unauthenticated)?;

        Ok(AuthenticatedSession {
            user_id: session.user_id,
            session_id: session.id,
            username: session.username,
        })
    }

    /// Resolve an access token, additionally requiring a verified email.
    ///
    /// This is the server-side enforcement of the verified-only rule; the UI
    /// hiding a button is never the control. Callers that are gated on
    /// verification use this instead of [`AuthService::authenticate`].
    pub async fn authenticate_verified(
        &self,
        access_token: &str,
    ) -> Result<AuthenticatedSession, AuthError> {
        let session = self.authenticate(access_token).await?;

        let user = self
            .repository
            .find_user_by_id(&session.user_id)
            .await?
            .ok_or(AuthError::Unauthenticated)?;

        if user.email_verified_at.is_none() {
            return Err(AuthError::EmailNotVerified);
        }

        Ok(session)
    }

    /// Load the authenticated user's profile.
    pub async fn current_user(&self, access_token: &str) -> Result<UserProfile, AuthError> {
        let session = self.authenticate(access_token).await?;
        let user = self
            .repository
            .find_user_by_id(&session.user_id)
            .await?
            .ok_or(AuthError::Unauthenticated)?;

        Ok(profile_from(user))
    }

    /// Redeem an email-verification link and return the updated profile.
    ///
    /// The token is spent *first*; if marking the account verified then fails, the
    /// link stays spent and the user must request another. That ordering is the
    /// difference between "single use" and "usually single use".
    pub async fn verify_email(&self, token: &str) -> Result<UserProfile, AuthError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(AuthError::TokenInvalid);
        }

        let digest = TokenIssuer::hash_link_token(token);

        match self
            .account_tokens
            .consume(&digest, TokenPurpose::EmailVerification)
            .await?
        {
            Some(user_id) => {
                let user = self.repository.mark_email_verified(&user_id).await?;
                tracing::info!(user_id = %user.id, "email verified");

                Ok(profile_from(user))
            }
            None => Err(self
                .refuse_link(&digest, TokenPurpose::EmailVerification)
                .await),
        }
    }

    /// Email a fresh verification link, if an unverified account uses the address.
    ///
    /// Always `Ok`: the caller must not be able to tell a real address from an
    /// unknown one, nor an already-verified account from either.
    pub async fn resend_verification(
        &self,
        request: ResendVerificationRequest,
        context: SessionContext,
    ) -> Result<(), AuthError> {
        let source = context.source();
        refuse_while_limited(&*self.email_attempts, source).await?;

        let problems = validation::validate_resend_verification(&request);
        if !problems.is_empty() {
            return Err(AuthError::Validation(problems));
        }

        let email = validation::normalize_email(&request.email);
        let user = self.repository.find_user_by_email(&email).await?;

        if let Some(user) = user.filter(|user| user.email_verified_at.is_none()) {
            self.issue_verification_link(&user).await;
        }

        // Every accepted request spends one unit of the source's budget, whether
        // or not it caused an email: the count is a property of the caller, never
        // of the address.
        self.email_attempts.record_failure(source).await;

        Ok(())
    }

    /// Email a password-reset link, if an account uses the address.
    ///
    /// Always `Ok`. The lookup runs for every address, and the link issue runs for
    /// every address too — `INSERT … SELECT … WHERE EXISTS (user)` writes nothing
    /// when no account matches while executing the same statements, so a request
    /// for an unknown address does the same work and takes the same time as one for
    /// a known address.
    pub async fn forgot_password(
        &self,
        request: ForgotPasswordRequest,
        context: SessionContext,
    ) -> Result<(), AuthError> {
        let source = context.source();
        refuse_while_limited(&*self.email_attempts, source).await?;

        let problems = validation::validate_forgot_password(&request);
        if !problems.is_empty() {
            return Err(AuthError::Validation(problems));
        }

        let email = validation::normalize_email(&request.email);
        let user = self.repository.find_user_by_email(&email).await?;

        // A miss still gets a token id, so the issue path below is byte-for-byte
        // the same sequence of statements either way.
        let user_id = match &user {
            Some(user) => user.id.clone(),
            None => new_id(),
        };

        // A failure here must not turn into a different HTTP answer for a hit than
        // for a miss; it is logged and the endpoint still reports acceptance.
        if let Err(error) = self.issue_reset_link(&user_id, user.as_ref()).await {
            tracing::error!(%error, "could not issue a password-reset link");
        }

        self.email_attempts.record_failure(source).await;

        Ok(())
    }

    /// Redeem a password-reset link: set the new password and sign out everywhere.
    ///
    /// The token is spent before the (expensive) hash and the password write, so a
    /// half-completed reset cannot be replayed.
    pub async fn reset_password(
        &self,
        request: ResetPasswordRequest,
        context: SessionContext,
    ) -> Result<(), AuthError> {
        refuse_while_limited(&*self.email_attempts, context.source()).await?;

        let problems = validation::validate_password_reset(&request);
        if !problems.is_empty() {
            return Err(AuthError::Validation(problems));
        }

        let digest = TokenIssuer::hash_link_token(&request.token);

        let Some(user_id) = self
            .account_tokens
            .consume(&digest, TokenPurpose::PasswordReset)
            .await?
        else {
            return Err(self.refuse_link(&digest, TokenPurpose::PasswordReset).await);
        };

        let password_hash = self.hasher.hash(&request.password).await?;
        self.repository
            .update_password(&user_id, &password_hash)
            .await?;
        // A password change ends every session opened with the old one.
        self.repository.revoke_all_sessions(&user_id).await?;

        tracing::info!(user_id = %user_id, "password reset");

        Ok(())
    }

    /// Mint a verification link for `user` and queue its email.
    ///
    /// Best effort by design: registration has already succeeded, so a storage or
    /// queueing failure is logged rather than surfaced.
    async fn issue_verification_link(&self, user: &UserRow) {
        match self
            .create_link(&user.id, TokenPurpose::EmailVerification)
            .await
        {
            Ok(Some(token)) => {
                let message = links::verification_message(
                    user,
                    &self.public_base_url,
                    &token,
                    self.verification_token_ttl,
                );
                self.deliver(message);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(user_id = %user.id, %error, "could not create a verification link")
            }
        }
    }

    /// Mint a reset link, emailing it only when a real account was found.
    ///
    /// For an unknown address the link row is not written (the account does not
    /// exist), but the attempt is identical — which is what keeps the endpoint from
    /// being an oracle.
    async fn issue_reset_link(
        &self,
        user_id: &str,
        user: Option<&UserRow>,
    ) -> Result<(), AuthError> {
        let Some(token) = self
            .create_link(user_id, TokenPurpose::PasswordReset)
            .await?
        else {
            return Ok(());
        };

        if let Some(user) = user {
            let message =
                links::reset_message(user, &self.public_base_url, &token, self.reset_token_ttl);
            self.deliver(message);
        }

        Ok(())
    }

    /// Mint a one-shot token, store its digest, and return the raw token.
    ///
    /// `Ok(None)` means no such user exists, so no link could be attached to one.
    async fn create_link(
        &self,
        user_id: &str,
        purpose: TokenPurpose,
    ) -> Result<Option<String>, AuthError> {
        let token = self.tokens.issue_link_token()?;
        let token_hash = TokenIssuer::hash_link_token(&token);
        let id = new_id();
        let ttl = match purpose {
            TokenPurpose::EmailVerification => self.verification_token_ttl,
            TokenPurpose::PasswordReset => self.reset_token_ttl,
        };

        let issued = self
            .account_tokens
            .issue(&NewToken {
                id: &id,
                user_id,
                purpose,
                token_hash: &token_hash,
                ttl_secs: ttl.as_secs_f64(),
            })
            .await?;

        Ok(issued.then_some(token))
    }

    /// Hand a message to the transport without awaiting it.
    ///
    /// The spawn is what keeps a slow provider off the request path; a delivery
    /// failure is logged and forgotten.
    fn deliver(&self, message: OutgoingMessage) {
        let mailer = Arc::clone(&self.mailer);

        tokio::spawn(async move {
            if let Err(error) = mailer.send(message).await {
                tracing::error!(%error, "email delivery failed");
            }
        });
    }

    /// Turn a refused link into the most useful distinguishable error.
    ///
    /// `consume` cannot tell "expired" from "already used" from "never existed",
    /// and it deliberately must not (its filter is the atomic redemption). This
    /// follow-up read exists only to phrase the refusal: a token that is unspent
    /// but past its `expires_at` is [`AuthError::TokenExpired`], everything else
    /// is [`AuthError::TokenInvalid`].
    async fn refuse_link(&self, digest: &str, purpose: TokenPurpose) -> AuthError {
        match self.account_tokens.find(digest, purpose).await {
            Ok(Some(stored)) if stored.is_expired() && !stored.is_consumed() => {
                AuthError::TokenExpired
            }
            Ok(_) => AuthError::TokenInvalid,
            Err(error) => {
                tracing::error!(%error, "could not inspect a refused link");
                AuthError::TokenInvalid
            }
        }
    }

    /// Record a failed credential check and return the error to report.
    ///
    /// The state change is deliberately *not* reflected in this response: the
    /// contract is "N failures, then the *next* attempt is throttled", so the
    /// failure that trips the limiter still answers "wrong password".
    async fn reject(&self, source: &str) -> AuthError {
        match self.login_attempts.record_failure(source).await {
            LoginAttempt::Throttled { .. } | LoginAttempt::LockedOut { .. } => {
                tracing::info!(source = %source, "login limiter tripped");
            }
            LoginAttempt::Allowed => {}
        }

        AuthError::InvalidCredentials
    }

    /// Issue the access token and assemble the response for a signed-in user.
    fn finish_sign_in(
        &self,
        user: UserRow,
        session_id: &str,
        refresh_token: String,
    ) -> Result<AuthSession, AuthError> {
        let issued = self.tokens.issue_access(&user.id, session_id)?;

        Ok(AuthSession {
            user: profile_from(user),
            tokens: self.token_pair(issued, refresh_token),
        })
    }

    fn token_pair(&self, issued: IssuedAccess, refresh_token: String) -> TokenPair {
        TokenPair {
            access_token: issued.token,
            refresh_token,
            token_type: "Bearer".to_owned(),
            expires_in: issued.expires_in,
        }
    }

    /// Spend the same effort a real password check would, then discard the answer.
    ///
    /// Without this, a request for an unknown email returns noticeably faster than
    /// one with a wrong password, which turns login into an account-enumeration
    /// oracle.
    async fn absorb_timing(&self, password: &str) -> Result<(), AuthError> {
        self.hasher
            .verify(password, &self.dummy_hash)
            .await
            .map(|_| ())
    }

    /// Refresh-token lifetime in seconds, handed to the database as a number.
    ///
    /// The database turns it into `now() + make_interval(...)`, so the expiry is
    /// computed by the same clock the active-session check reads. Deriving it in
    /// Rust instead would let the application clock and the database clock
    /// disagree about whether a session is still alive.
    fn refresh_ttl_secs(&self) -> f64 {
        f64::from(u32::try_from(self.tokens.refresh_ttl().as_secs()).unwrap_or(u32::MAX))
    }
}

/// Refuse an attempt the limiter is already throttling or locking out.
async fn refuse_while_limited(
    store: &dyn LoginAttemptStore,
    source: &str,
) -> Result<(), AuthError> {
    match store.verdict(source).await {
        LoginAttempt::Allowed => Ok(()),
        LoginAttempt::Throttled {
            retry_after_seconds,
        } => {
            tracing::warn!(source = %source, "request throttled");
            Err(AuthError::TooManyAttempts {
                retry_after_seconds,
            })
        }
        LoginAttempt::LockedOut {
            retry_after_seconds,
        } => {
            tracing::warn!(source = %source, "source locked out");
            Err(AuthError::LockedOut {
                retry_after_seconds,
            })
        }
    }
}

/// A fresh ULID primary key, matching the `users` / `sessions` conventions.
fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Project a stored account onto its public shape.
fn profile_from(user: UserRow) -> UserProfile {
    UserProfile {
        id: user.id,
        username: user.username,
        email: user.email,
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        email_verified: user.email_verified_at.is_some(),
        created_at_ms: unix_millis(user.created_at),
    }
}

/// Milliseconds since the Unix epoch, in the range JavaScript can represent.
fn unix_millis(value: OffsetDateTime) -> i64 {
    value
        .unix_timestamp()
        .saturating_mul(1000)
        .saturating_add(i64::from(value.millisecond()))
}

#[cfg(test)]
mod tests {
    use sqlx::types::time::OffsetDateTime;

    use super::{AuthConfig, profile_from, unix_millis};
    use crate::repository::UserRow;

    #[test]
    fn default_lifetimes_are_the_documented_ones() {
        let config = AuthConfig::new("secret");

        assert_eq!(config.access_token_ttl.as_secs(), 900);
        assert_eq!(config.refresh_token_ttl.as_secs(), 2_592_000);
        assert_eq!(config.verification_token_ttl.as_secs(), 86_400);
        assert_eq!(config.reset_token_ttl.as_secs(), 3_600);
        assert_eq!(config.public_base_url, "http://localhost:5173");
        assert_eq!(config.jwt_secret, "secret");
    }

    #[test]
    fn timestamps_become_unix_milliseconds() {
        let instant = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("a valid fixed timestamp")
            .replace_millisecond(500)
            .expect("a valid millisecond");

        assert_eq!(unix_millis(instant), 1_700_000_000_500);
    }

    #[test]
    fn a_profile_hides_the_password_and_expands_verification() {
        let user = UserRow {
            id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            username: "alice".to_owned(),
            email: "alice@example.com".to_owned(),
            display_name: "Alice".to_owned(),
            avatar_url: None,
            password_hash: Some("$argon2id$...".to_owned()),
            email_verified_at: None,
            created_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp"),
        };

        let profile = profile_from(user);

        assert!(
            !profile.email_verified,
            "a NULL verification date means false"
        );
        assert_eq!(profile.created_at_ms, 1_700_000_000_000);
        assert_eq!(profile.username, "alice");
    }

    #[test]
    fn a_verified_account_reports_it() {
        let user = UserRow {
            id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            username: "alice".to_owned(),
            email: "alice@example.com".to_owned(),
            display_name: "Alice".to_owned(),
            avatar_url: None,
            password_hash: None,
            email_verified_at: Some(
                OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp"),
            ),
            created_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("timestamp"),
        };

        assert!(profile_from(user).email_verified);
    }
}
