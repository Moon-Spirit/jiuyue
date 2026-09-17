//! REST handlers for the identity module.
//!
//! Mounted under `/auth`, which the frontend reaches as `/api/auth` through the
//! same Vite/Caddy prefix rewrite that serves `/health` (see `frontend/vite.config.ts`).
//!
//! The handlers are deliberately thin: they read the bearer token and the request
//! metadata, hand the work to [`jiuyue_auth::AuthService`], and translate the
//! result. The one thing they own is the *HTTP shape of failure* — every error
//! leaves as the contract's [`ErrorBody`] via [`ApiError`], so the client never has
//! to parse a status code or a prose message to know what happened.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use jiuyue_auth::SessionContext;
use jiuyue_auth::oauth::service::OAuthCallbackContext;
use jiuyue_contract::auth::{
    CompleteOAuthSignInRequest, ForgotPasswordRequest, OAuthCallbackRequest, OAuthProvider,
    OAuthProviderInfo, OAuthProviders, RequestAccepted, ResendVerificationRequest,
    ResetPasswordRequest, VerifyEmailRequest,
};
use jiuyue_contract::{
    AuthSession, LoginRequest, OAuthCallbackResponse, RefreshRequest, RegisterRequest, TokenPair,
    UserProfile, WhoAmI,
};

use super::api::{ApiError, bearer_token};
use crate::state::AppState;

/// The `/auth` routes.
///
/// Registered even when the instance has no database: the handlers then answer
/// [`jiuyue_contract::ErrorCode::Unavailable`] with `503`, which is a truthful and
/// debuggable response, instead of the route silently 404-ing.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/refresh", post(refresh))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(current_user))
        .route("/auth/whoami", get(whoami))
        // The two email journeys. Verify and reset redeem a one-shot link; the
        // other two ask for a link to be sent (again) and deliberately answer the
        // same way whether or not the address has an account.
        .route("/auth/verify-email", post(verify_email))
        .route("/auth/resend-verification", post(resend_verification))
        .route("/auth/forgot-password", post(forgot_password))
        .route("/auth/reset-password", post(reset_password))
        // Third-party sign-in. `providers` is what the login page renders from,
        // so a provider with no credentials is absent rather than broken; `start`
        // and `callback` are the two halves of the redirect; `link` is the same
        // round trip, but bound to the signed-in account.
        .route("/auth/oauth/providers", get(oauth_providers))
        .route("/auth/oauth/start", post(oauth_start))
        .route("/auth/oauth/callback", post(oauth_callback))
        .route("/auth/oauth/complete", post(oauth_complete))
        .route("/auth/oauth/link", post(oauth_link))
}

/// `POST /auth/oauth/start` body.
///
/// `return_to`, when present, must be a relative path: the client is asking to be
/// sent somewhere in *this* application after signing in, and a value that could
/// become an absolute URL would turn this endpoint into an open redirect.
#[derive(Debug, Clone, serde::Deserialize)]
struct OAuthStartBody {
    provider: OAuthProvider,
    #[serde(default)]
    return_to: Option<String>,
}

/// `POST /auth/oauth/start` response.
#[derive(Debug, Clone, serde::Serialize)]
struct OAuthStartResponse {
    authorize_url: String,
}

/// `GET /auth/oauth/providers` — the providers this instance can actually drive.
///
/// No credentials are needed to *start* a sign-in beyond the client id, which is
/// public by definition, so the authorize URLs are safe to hand to the browser.
/// The client secret never appears here or anywhere else on the wire.
///
/// An instance with no provider configured answers `{"providers": []}`, which is
/// the honest answer and lets the login page render nothing rather than a
/// broken button.
async fn oauth_providers(State(state): State<AppState>) -> Result<Json<OAuthProviders>, ApiError> {
    let auth = state.auth()?;

    // `auth.oauth()` is `Ok` only when the instance was built with an OAuth
    // client; without one there is nothing to offer, which is `[]` and not an
    // error — the login page should just have no third-party buttons.
    let Ok(oauth) = auth.oauth() else {
        return Ok(Json(OAuthProviders {
            providers: Vec::new(),
        }));
    };

    let mut providers = Vec::new();
    for provider in oauth.configured_providers() {
        providers.push(OAuthProviderInfo {
            provider,
            display_name: provider.display_name().to_owned(),
            authorize_url: oauth.start(provider, None, None).await?,
        });
    }

    Ok(Json(OAuthProviders { providers }))
}

/// `POST /auth/oauth/start` — begin a round trip and return where to send the browser.
///
/// The URL is returned as JSON rather than as a `302` so the client controls the
/// navigation (and so a test can read it). Everything secret stays server-side:
/// the client id is public, the state and the PKCE verifier are not.
async fn oauth_start(
    State(state): State<AppState>,
    Json(body): Json<OAuthStartBody>,
) -> Result<Json<OAuthStartResponse>, ApiError> {
    let auth = state.auth()?;

    let authorize_url = auth
        .oauth()?
        .start(body.provider, body.return_to.as_deref(), None)
        .await?;

    Ok(Json(OAuthStartResponse { authorize_url }))
}

/// `POST /auth/oauth/callback` — redeem the provider's redirect.
///
/// The `state` is spent before the provider is called, so a replayed callback is
/// refused without a second exchange. On success either a full session comes
/// back, or — for a first-time user — a limited one and the username step; see
/// [`OAuthCallbackResponse`].
async fn oauth_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OAuthCallbackRequest>,
) -> Result<Json<OAuthCallbackResponse>, ApiError> {
    let auth = state.auth()?;

    let outcome = auth
        .oauth()?
        .callback(body, oauth_context(&headers))
        .await?;

    Ok(Json(jiuyue_auth::session_outcome(&auth, outcome)?))
}

/// `POST /auth/oauth/complete` — choose a username and finish a first-time sign-in.
///
/// The limited token is the bearer credential: it is the *only* thing that names
/// the account, and it is spent in the same statement that claims the handle.
/// `USERNAME_TAKEN` is retryable with the same token, because a lost race wrote
/// nothing.
async fn oauth_complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CompleteOAuthSignInRequest>,
) -> Result<Json<OAuthCallbackResponse>, ApiError> {
    let auth = state.auth()?;
    let token = bearer_token(&headers)?;

    let outcome = auth
        .oauth()?
        .complete_sign_in(&token, body, oauth_context(&headers))
        .await?;

    Ok(Json(jiuyue_auth::session_outcome(&auth, outcome)?))
}

/// `POST /auth/oauth/link` — start a linking round trip for the signed-in account.
///
/// Authenticated on purpose: the whole point of the linking rule is that a
/// provider is only ever attached by someone already holding the account. The
/// account id comes from the access token, never from the request body.
async fn oauth_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OAuthStartBody>,
) -> Result<Json<OAuthStartResponse>, ApiError> {
    let auth = state.auth()?;
    let token = bearer_token(&headers)?;

    let session = auth.authenticate(&token).await?;

    let authorize_url = auth
        .oauth()?
        .start(
            body.provider,
            body.return_to.as_deref(),
            Some(&session.user_id),
        )
        .await?;

    Ok(Json(OAuthStartResponse { authorize_url }))
}

/// The callback's view of the caller, from the same headers login records.
///
/// Deliberately the same extraction as [`session_context`], so an OAuth session
/// and a password session record the same device metadata.
fn oauth_context(headers: &HeaderMap) -> OAuthCallbackContext {
    OAuthCallbackContext {
        session: session_context(headers),
    }
}

/// `POST /auth/register` — create an account and sign it in immediately.
async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<AuthSession>), ApiError> {
    let auth = state.auth()?;

    let session = auth
        .register(body, session_context(&headers))
        .await
        .map_err(ApiError::from)?;

    Ok((StatusCode::CREATED, Json(session)))
}

/// `POST /auth/login` — verify credentials and open a new session.
async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Json<AuthSession>, ApiError> {
    let auth = state.auth()?;

    let session = auth
        .login(body, session_context(&headers))
        .await
        .map_err(ApiError::from)?;

    Ok(Json(session))
}

/// `POST /auth/refresh` — trade a refresh token for a new access token.
async fn refresh(
    State(state): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<TokenPair>, ApiError> {
    let auth = state.auth()?;

    let tokens = auth
        .refresh(&body.refresh_token)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(tokens))
}

/// `POST /auth/logout` — revoke the session the access token belongs to.
///
/// Returns `204 No Content`; the access token stops working on the very next
/// request because the session row is revoked rather than the token expiring.
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<StatusCode, ApiError> {
    let auth = state.auth()?;
    let token = bearer_token(&headers)?;

    auth.logout(&token).await.map_err(ApiError::from)?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /auth/me` — the authenticated user's profile.
async fn current_user(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<UserProfile>, ApiError> {
    let auth = state.auth()?;
    let token = bearer_token(&headers)?;

    let profile = auth.current_user(&token).await.map_err(ApiError::from)?;

    Ok(Json(profile))
}

/// `GET /auth/whoami` — the minimal proof that a protected path was reached.
async fn whoami(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<WhoAmI>, ApiError> {
    let auth = state.auth()?;
    let token = bearer_token(&headers)?;

    let session = auth.authenticate(&token).await.map_err(ApiError::from)?;

    Ok(Json(WhoAmI {
        user_id: session.user_id,
        session_id: session.session_id,
        username: session.username,
    }))
}

/// `POST /auth/verify-email` — redeem a verification link and return the profile.
///
/// The updated [`UserProfile`] comes back so the client can flip its own
/// `email_verified` without a second round trip. No bearer token is required: the
/// link itself is the proof, and the user may well be following it in a different
/// browser from the one they registered with.
async fn verify_email(
    State(state): State<AppState>,
    Json(body): Json<VerifyEmailRequest>,
) -> Result<Json<UserProfile>, ApiError> {
    let auth = state.auth()?;

    let profile = auth
        .verify_email(&body.token)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(profile))
}

/// `POST /auth/resend-verification` — send a fresh verification link.
///
/// Answers `202` with a fixed body, whether or not the address belongs to an
/// unverified account: the endpoint must not be usable to probe which addresses
/// are registered.
async fn resend_verification(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ResendVerificationRequest>,
) -> Result<(StatusCode, Json<RequestAccepted>), ApiError> {
    let auth = state.auth()?;

    auth.resend_verification(body, session_context(&headers))
        .await
        .map_err(ApiError::from)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(RequestAccepted { accepted: true }),
    ))
}

/// `POST /auth/forgot-password` — send a password-reset link.
///
/// The response is identical for an address with an account and one without, and
/// the work done is the same too (see `AuthService::forgot_password`), so this
/// cannot become an account-existence oracle.
async fn forgot_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ForgotPasswordRequest>,
) -> Result<(StatusCode, Json<RequestAccepted>), ApiError> {
    let auth = state.auth()?;

    auth.forgot_password(body, session_context(&headers))
        .await
        .map_err(ApiError::from)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(RequestAccepted { accepted: true }),
    ))
}

/// `POST /auth/reset-password` — redeem a reset link and set a new password.
///
/// Answers `204`: there is nothing to return, and the session the caller may have
/// held is intentionally revoked by the reset.
async fn reset_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ResetPasswordRequest>,
) -> Result<StatusCode, ApiError> {
    let auth = state.auth()?;

    auth.reset_password(body, session_context(&headers))
        .await
        .map_err(ApiError::from)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Collect the device metadata a login records (the device ticket renders it).
///
/// The client address comes from `X-Forwarded-For` because the app sits behind
/// Caddy; there is no direct peer address to trust. It is the **last** entry that
/// is believed, not the first: Caddy *appends* the address it observed to any
/// value the client sent, so earlier entries are attacker-controlled. Reading the
/// first one would let anyone rotate the key login limiting counts under — and
/// the limiter is only as good as its key.
fn session_context(headers: &HeaderMap) -> SessionContext {
    SessionContext {
        device_label: None,
        user_agent: header_value(headers, "user-agent"),
        ip_address: header_value(headers, "x-forwarded-for").and_then(|forwarded| {
            forwarded
                .rsplit(',')
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        }),
    }
}

/// Read a header as owned text, ignoring non-UTF-8 values.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}
