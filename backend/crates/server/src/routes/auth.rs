//! REST handlers for the identity module.
//!
//! Mounted under `/auth`, which the frontend reaches as `/api/auth` through the
//! same Vite/Caddy prefix rewrite that serves `/health` (see `frontend/vite.config.ts`).
//!
//! The handlers are deliberately thin: they read the bearer token and the request
//! metadata, hand the work to [`jiuyue_auth::AuthService`], and translate the
//! result. The one thing they own is the *HTTP shape of failure* — every error
//! leaves as the contract's [`ErrorBody`], so the client never has to parse a
//! status code or a prose message to know what happened.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use jiuyue_auth::{AuthError, SessionContext};
use jiuyue_contract::{
    AuthSession, ErrorBody, ErrorCode, ErrorDetail, FieldError, FieldErrorCode, LoginRequest,
    RefreshRequest, RegisterRequest, TokenPair, UserProfile, WhoAmI,
};

use crate::state::{AppState, AuthUnavailable};

/// The `/auth` routes.
///
/// Registered even when the instance has no database: the handlers then answer
/// [`ErrorCode::Unavailable`] with `503`, which is a truthful and debuggable
/// response, instead of the route silently 404-ing.
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
/// Caddy; there is no direct peer address to trust.
fn session_context(headers: &HeaderMap) -> SessionContext {
    SessionContext {
        device_label: None,
        user_agent: header_value(headers, "user-agent"),
        ip_address: header_value(headers, "x-forwarded-for").and_then(|forwarded| {
            forwarded
                .split(',')
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

/// Extract the token from an `Authorization: Bearer <token>` header.
fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiError::unauthenticated)?;

    let (scheme, token) = value
        .split_once(' ')
        .ok_or_else(ApiError::unauthenticated)?;

    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return Err(ApiError::unauthenticated());
    }

    Ok(token.trim().to_owned())
}

/// An error response: the status plus the contract's machine-readable body.
struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    fn new(status: StatusCode, code: ErrorCode, message: &str, fields: Vec<FieldError>) -> Self {
        Self {
            status,
            body: ErrorBody {
                error: ErrorDetail {
                    code,
                    message: message.to_owned(),
                    fields,
                },
            },
        }
    }

    /// The failure for a missing, malformed or rejected bearer token.
    fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthenticated,
            "登录状态已失效，请重新登录",
            Vec::new(),
        )
    }

    /// A field-level problem that duplicates an account insert.
    fn taken(field: &str, message: &str) -> Vec<FieldError> {
        vec![FieldError {
            field: field.to_owned(),
            code: FieldErrorCode::Taken,
            message: message.to_owned(),
        }]
    }
}

impl From<AuthUnavailable> for ApiError {
    fn from(error: AuthUnavailable) -> Self {
        tracing::warn!(%error, "identity endpoint called without a configured database");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "当前实例未启用账号服务",
            Vec::new(),
        )
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Validation(fields) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::ValidationFailed,
                "请求参数无效",
                fields,
            ),
            AuthError::EmailTaken => Self::new(
                StatusCode::CONFLICT,
                ErrorCode::EmailTaken,
                "该邮箱已被注册",
                ApiError::taken("email", "该邮箱已被注册"),
            ),
            AuthError::UsernameTaken => Self::new(
                StatusCode::CONFLICT,
                ErrorCode::UsernameTaken,
                "该用户名已被占用",
                ApiError::taken("username", "该用户名已被占用"),
            ),
            AuthError::InvalidCredentials => Self::new(
                StatusCode::UNAUTHORIZED,
                ErrorCode::InvalidCredentials,
                "邮箱或密码不正确",
                Vec::new(),
            ),
            AuthError::Unauthenticated => Self::unauthenticated(),
            // A token that does not verify is a credential problem, not a server
            // fault: malformed, expired, tampered with, or foreign-signed. Log
            // the reason (never the token) and answer 401.
            AuthError::Token(error) => {
                tracing::debug!(%error, "rejected an access token that did not verify");
                Self::unauthenticated()
            }
            internal => {
                // Log the cause, return none of it: the client gets a stable code.
                tracing::error!(error = %internal, "identity request failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorCode::Internal,
                    "服务器内部错误",
                    Vec::new(),
                )
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}
