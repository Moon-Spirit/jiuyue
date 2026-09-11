//! Uniform error model: every failure is JSON `{"error": "<machine_code>", "message": "<human>"}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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
    /// M5 friends: the pair is already befriended (either direction).
    AlreadyFriends,
    /// M5 friends: a pending request already exists between the pair in
    /// EITHER direction.
    RequestAlreadyPending,
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

    /// 404 — generic resource absence (e.g. an unknown friend-request id, or
    /// a request the caller is not a party of — existence is never revealed
    /// to non-participants).
    #[error("resource not found")]
    ResourceNotFound,

    /// 400 — friend-system self-action (requesting yourself as a peer).
    #[error("self request")]
    SelfRequest,

    /// 422 — semantically invalid input (username format, weak password, bad target).
    #[error("validation failed: {0}")]
    Validation(String),

    /// 400 — malformed request (bad JSON, missing fields).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// 415 — upload body is not an accepted media type, or the declared
    /// `Content-Type` disagrees with the sniffed magic bytes (M8 media).
    #[error("unsupported media type")]
    UnsupportedMediaType,

    /// 413 — upload exceeded the per-kind size cap (M8 media).
    #[error("payload too large")]
    PayloadTooLarge,

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
            AppError::Conflict(ConflictKind::AlreadyFriends) => (
                StatusCode::CONFLICT,
                "already_friends",
                "you are already friends with this user".to_string(),
            ),
            AppError::Conflict(ConflictKind::RequestAlreadyPending) => (
                StatusCode::CONFLICT,
                "request_already_pending",
                "a friend request between you two is already pending".to_string(),
            ),
            AppError::ResourceNotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "resource not found".to_string(),
            ),
            AppError::SelfRequest => (
                StatusCode::BAD_REQUEST,
                "self_request",
                "cannot target yourself".to_string(),
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
            AppError::UnsupportedMediaType => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_type",
                "unsupported media type".to_string(),
            ),
            AppError::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "upload exceeds the size limit".to_string(),
            ),
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
