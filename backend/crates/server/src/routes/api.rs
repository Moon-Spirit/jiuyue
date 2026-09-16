//! The one HTTP failure shape every JSON endpoint answers with.
//!
//! A handler returns [`ApiError`] and the client receives the contract's
//! [`ErrorBody`] — `{"error": {"code", "message", "fields"}}` — plus a status
//! code. The code is the machine-readable part; the message is a display
//! fallback. Nothing here leaks an internal cause: the cause is logged, the
//! client gets a stable code.
//!
//! This module also owns bearer-token extraction, because "what does a missing or
//! malformed `Authorization` header mean" must have exactly one answer across
//! every protected endpoint.

use axum::Json;
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use jiuyue_auth::AuthError;
use jiuyue_chat::ChatError;
use jiuyue_contract::{ErrorBody, ErrorCode, ErrorDetail, FieldError, FieldErrorCode};

use crate::state::ServiceUnavailable;

/// An error response: the status plus the contract's machine-readable body.
pub struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    /// Assemble a failure from its parts.
    pub fn new(
        status: StatusCode,
        code: ErrorCode,
        message: &str,
        fields: Vec<FieldError>,
    ) -> Self {
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
    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthenticated,
            "登录状态已失效，请重新登录",
            Vec::new(),
        )
    }

    /// A server fault the client cannot act on beyond retrying.
    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "服务器内部错误",
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

impl From<ServiceUnavailable> for ApiError {
    fn from(error: ServiceUnavailable) -> Self {
        tracing::warn!(
            service = error.service,
            "endpoint called without its configured subsystem"
        );
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            &format!("当前实例未启用{}服务", error.service),
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
                Self::internal()
            }
        }
    }
}

impl From<ChatError> for ApiError {
    fn from(error: ChatError) -> Self {
        match error {
            ChatError::Validation(fields) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::ValidationFailed,
                "请求参数无效",
                fields,
            ),
            ChatError::UserNotFound => Self::new(
                StatusCode::NOT_FOUND,
                ErrorCode::UserNotFound,
                "找不到该用户",
                Vec::new(),
            ),
            ChatError::ConversationNotFound => Self::new(
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "会话不存在",
                Vec::new(),
            ),
            ChatError::NotAParticipant => Self::new(
                StatusCode::FORBIDDEN,
                ErrorCode::NotAParticipant,
                "你不是该会话的参与者",
                Vec::new(),
            ),
            ChatError::NotPermitted { .. } => Self::new(
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                "你的权限不允许此操作",
                Vec::new(),
            ),
            // State conflicts: already a member, the target is not one, the owner
            // trying to leave with Participants behind, or a Group action on a
            // Direct Conversation. Each carries its own Chinese message, all share
            // the machine code so a client can branch once.
            conflict @ (ChatError::NotAGroup
            | ChatError::AlreadyMember
            | ChatError::MemberNotFound
            | ChatError::OwnerCannotLeave) => Self::new(
                StatusCode::CONFLICT,
                ErrorCode::Conflict,
                conflict.message(),
                Vec::new(),
            ),
            ChatError::Database(source) => {
                tracing::error!(%source, "chat request failed");
                Self::internal()
            }
            ChatError::Internal => {
                tracing::error!("chat request failed on an internal invariant");
                Self::internal()
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

/// Extract the token from an `Authorization: Bearer <token>` header.
pub fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
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
