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
use jiuyue_contract::auth::{
    ForgotPasswordRequest, RequestAccepted, ResendVerificationRequest, ResetPasswordRequest,
    VerifyEmailRequest,
};
use jiuyue_contract::{
    AuthSession, LoginRequest, RefreshRequest, RegisterRequest, TokenPair, UserProfile, WhoAmI,
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
