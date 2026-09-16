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
