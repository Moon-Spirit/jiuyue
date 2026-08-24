//! Uniform error model: every failure is JSON `{"error": "<machine_code>", "message": "<human>"}`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    UsernameTaken,
    IdentityAlreadyBound,
    /// M3 key distribution: the target user's bundle has no one-time keys
    /// left to claim (pool drained by earlier fetches).
    NoOneTimeKeys,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// 401 — wrong code/password/token/unknown identifier. One generic body so
    /// callers cannot distinguish "unknown account" from "bad secret".
    #[error("invalid credentials")]
    InvalidCredentials,
    /// Register-path verification-code failure. Deliberately distinct from
    /// `InvalidCredentials` so clients show an actionable hint instead of
    /// "wrong password" on the SIGN-UP screen.
    #[error("invalid or expired verification code")]
    InvalidCode,

    /// 409 — unique constraint violated.
    #[error("conflict: {0:?}")]
    Conflict(ConflictKind),

    /// 404 — referenced resource does not exist.
    #[error("peer user not found")]
    PeerNotFound,

    /// 422 — semantically invalid input (username format, weak password, bad target).
    #[error("validation failed: {0}")]
    Validation(String),

    /// 400 — malformed request (bad JSON, missing fields).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// 500 — unexpected; details logged, never leaked to the client.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    /// Constructor for `map_err`: accepts any error convertible into `anyhow::Error`.
    pub fn internal<E: Into<anyhow::Error>>(err: E) -> Self {
        Self::Internal(err.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            AppError::InvalidCredentials => (
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "invalid credentials".to_string(),
            ),
            AppError::InvalidCode => (
                StatusCode::UNAUTHORIZED,
                "invalid_or_expired_code",
                "invalid or expired verification code".to_string(),
            ),
            AppError::Conflict(ConflictKind::UsernameTaken) => (
                StatusCode::CONFLICT,
                "username_taken",
                "username is already taken".to_string(),
            ),
            AppError::Conflict(ConflictKind::IdentityAlreadyBound) => (
                StatusCode::CONFLICT,
                "identity_already_bound",
                "this email/phone is already bound to an account".to_string(),
            ),
            AppError::Conflict(ConflictKind::NoOneTimeKeys) => (
                StatusCode::CONFLICT,
                "no_one_time_keys",
                "no one-time keys left for this user".to_string(),
            ),
            AppError::PeerNotFound => (
                StatusCode::NOT_FOUND,
                "peer_not_found",
                "no user with that username".to_string(),
            ),
            AppError::Validation(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_error",
                msg.clone(),
            ),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg.clone()),
            AppError::Internal(err) => {
                tracing::error!(error = %format!("{err:#}"), "internal server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal server error".to_string(),
                )
            }
        };
        (
            status,
            Json(ErrorBody {
                error: code,
                message,
            }),
        )
            .into_response()
    }
}
