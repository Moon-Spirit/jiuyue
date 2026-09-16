//! The identity use cases.
//!
//! [`AuthService`] is the whole surface the HTTP layer needs: register, log in,
//! refresh, log out, and resolve an access token to a session. It composes the
//! three narrower pieces ([`crate::validation`], [`crate::password`],
//! [`crate::token`]) and the repository, so a handler never has to know how a
//! password is hashed, how a session is revoked, or which table holds what.
//!
//! Everything here is written for the follow-on device ticket: sessions already
//! carry device metadata, and every session decision goes through
//! [`SessionRepository`], so adding listing and rotation means adding methods,
//! not rewriting this module.

use std::sync::Arc;
use std::time::Duration;

use jiuyue_contract::{AuthSession, LoginRequest, RegisterRequest, TokenPair, UserProfile};
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;

use crate::error::AuthError;
use crate::limiter::{
    InProcessLoginAttemptStore, LoginAttempt, LoginAttemptPolicy, LoginAttemptStore,
};
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
}

impl AuthConfig {
    /// 15 minutes: short enough that a leaked access token is not a lasting
    /// credential, long enough that clients are not refreshing constantly.
    pub const DEFAULT_ACCESS_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

    /// 30 days: the "stay signed in" horizon, and the session's hard expiry.
    pub const DEFAULT_REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

    /// Configuration with the documented default lifetimes.
    pub fn new(jwt_secret: impl Into<String>) -> Self {
        Self {
            jwt_secret: jwt_secret.into(),
            access_token_ttl: Self::DEFAULT_ACCESS_TOKEN_TTL,
            refresh_token_ttl: Self::DEFAULT_REFRESH_TOKEN_TTL,
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
    /// The key login attempts are counted under.
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

/// The identity module's public API.
pub struct AuthService {
    repository: SessionRepository,
    hasher: PasswordHasher,
    tokens: TokenIssuer,
    /// The login limiter. Held behind the trait so a multi-node deployment swaps
    /// in a shared store without touching this file (see [`crate::limiter`]).
    login_attempts: Arc<dyn LoginAttemptStore>,
    /// A real hash of a throwaway password, used to make "no such account" cost
    /// the same as "wrong password" (see [`AuthService::absorb_timing`]).
    dummy_hash: String,
}

impl AuthService {
    /// Build the service, rejecting an unusable signing secret.
    ///
    /// Hashing one throwaway password here is a deliberate one-off startup cost:
    /// it produces the timing-equaliser hash without a hardcoded constant.
    ///
    /// Login is limited by the process-local [`InProcessLoginAttemptStore`] with
    /// the default policy; use [`AuthService::with_login_store`] to supply a
    /// different one.
    pub async fn new(pool: PgPool, config: AuthConfig) -> Result<Self, AuthError> {
        Self::with_login_store(
            pool,
            config,
            Arc::new(InProcessLoginAttemptStore::new(
                LoginAttemptPolicy::default(),
            )),
        )
        .await
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
        let hasher = PasswordHasher::new();
        let tokens = TokenIssuer::new(
            &config.jwt_secret,
            config.access_token_ttl,
            config.refresh_token_ttl,
        )?;
        let dummy_hash = hasher.hash("jiuyue-timing-equaliser").await?;

        Ok(Self {
            repository: SessionRepository::new(pool),
            hasher,
            tokens,
            login_attempts,
            dummy_hash,
        })
    }

    /// Create an account and sign the new device in.
    ///
    /// Email verification is not required: the account is usable immediately and
    /// simply carries `email_verified: false` until the email ticket lands.
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
        self.refuse_while_limited(source).await?;

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

    /// Refuse an attempt the limiter is already throttling or locking out.
    async fn refuse_while_limited(&self, source: &str) -> Result<(), AuthError> {
        match self.login_attempts.verdict(source).await {
            LoginAttempt::Allowed => Ok(()),
            LoginAttempt::Throttled {
                retry_after_seconds,
            } => {
                tracing::warn!(source = %source, "login throttled");
                Err(AuthError::TooManyAttempts {
                    retry_after_seconds,
                })
            }
            LoginAttempt::LockedOut {
                retry_after_seconds,
            } => {
                tracing::warn!(source = %source, "login locked out");
                Err(AuthError::LockedOut {
                    retry_after_seconds,
                })
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

    /// Trade a refresh token for a fresh access token.
    ///
    /// The refresh token is not rotated yet �?that is the device ticket's job.
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
}
