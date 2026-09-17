//! The one HTTP failure shape every JSON endpoint answers with.
//!
//! A handler returns [`ApiError`] and the client receives the contract's
//! [`ErrorBody`] — `{"error": {"code", "message", "fields", "retry_after_seconds"}}`
//! — plus a status code. The code is the machine-readable part; the message is a
//! display fallback, and `retry_after_seconds` carries the wait for a throttle or
//! a lockout as a number instead of prose. Nothing here leaks an internal cause:
//! the cause is logged, the client gets a stable code.
//!
//! This module also owns bearer-token extraction, because "what does a missing or
//! malformed `Authorization` header mean" must have exactly one answer across
//! every protected endpoint.

use axum::Json;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION, header::RETRY_AFTER};
use axum::response::{IntoResponse, Response};
use jiuyue_auth::AuthError;
use jiuyue_chat::ChatError;
use jiuyue_contract::{ErrorBody, ErrorCode, ErrorDetail, FieldError, FieldErrorCode};
use jiuyue_realtime::RealtimeError;

use crate::state::ServiceUnavailable;

/// An error response: the status plus the contract's machine-readable body.
pub struct ApiError {
    status: StatusCode,
    body: ErrorBody,
    /// Mirrored into the `Retry-After` header for a throttle or lockout.
    retry_after_seconds: Option<u64>,
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
            retry_after_seconds: None,
            body: ErrorBody {
                error: ErrorDetail {
                    code,
                    message: message.to_owned(),
                    fields,
                    retry_after_seconds: None,
                },
            },
        }
    }

    /// A failure that carries a wait: throttling (`429`) or a lockout (`423`).
    ///
    /// The wait travels twice on purpose — in the body as
    /// [`jiuyue_contract::ErrorDetail::retry_after_seconds`], which the client
    /// contract already describes, and in the standard `Retry-After` header, so
    /// an intermediary that understands HTTP but not this API can still back off.
    fn with_retry_after(
        status: StatusCode,
        code: ErrorCode,
        message: &str,
        retry_after_seconds: u64,
    ) -> Self {
        Self {
            status,
            retry_after_seconds: Some(retry_after_seconds),
            body: ErrorBody {
                error: ErrorDetail {
                    code,
                    message: message.to_owned(),
                    fields: Vec::new(),
                    retry_after_seconds: Some(retry_after_seconds),
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
            // Throttled and locked out are distinct on purpose: a client shows a
            // different message and a different countdown for each. `423 Locked`
            // is the WebDAV code for "the source is locked"; `429` is the
            // standard "slow down".
            AuthError::TooManyAttempts {
                retry_after_seconds,
            } => Self::with_retry_after(
                StatusCode::TOO_MANY_REQUESTS,
                ErrorCode::TooManyAttempts,
                "登录尝试过于频繁，请稍后再试",
                retry_after_seconds,
            ),
            AuthError::LockedOut {
                retry_after_seconds,
            } => Self::with_retry_after(
                StatusCode::LOCKED,
                ErrorCode::LockedOut,
                "登录失败次数过多，请稍后再试",
                retry_after_seconds,
            ),
            // The account is fine and the token is good — the action is simply
            // gated on a verified address. `403` says "authenticated, not allowed";
            // the code lets the client prompt to open the inbox instead of showing
            // a generic denial.
            AuthError::EmailNotVerified => Self::new(
                StatusCode::FORBIDDEN,
                ErrorCode::EmailNotVerified,
                "请先验证邮箱，再开始新的会话",
                Vec::new(),
            ),
            // `410 Gone` is the honest code for a one-shot link: the resource
            // existed and is no longer usable. Expired and invalid are separate
            // codes because "ask for a new link" reads differently from "this link
            // is not valid".
            AuthError::TokenExpired => Self::new(
                StatusCode::GONE,
                ErrorCode::TokenExpired,
                "链接已过期，请重新获取一封邮件",
                Vec::new(),
            ),
            AuthError::TokenInvalid => Self::new(
                StatusCode::BAD_REQUEST,
                ErrorCode::TokenInvalid,
                "链接无效或已被使用，请重新获取一封邮件",
                Vec::new(),
            ),
            AuthError::AccountMissing => Self::new(
                StatusCode::NOT_FOUND,
                ErrorCode::UserNotFound,
                "找不到该账号",
                Vec::new(),
            ),
            // The account-take-over refusal, and the most important answer this
            // API gives. A provider asserted an address that already belongs to an
            // account, so nothing was linked and nothing was created. `409` is the
            // honest code: the request conflicts with the state of the world, and
            // the client's instruction is to sign in the usual way and link the
            // provider from settings.
            AuthError::AccountExists => Self::new(
                StatusCode::CONFLICT,
                ErrorCode::OAuthAccountExists,
                "该邮箱已注册，请用原来的方式登录后在设置中绑定第三方账号",
                Vec::new(),
            ),
            // This provider identity already belongs to another account. One
            // provider identity, one account — linking it elsewhere is not a
            // request that can be granted.
            AuthError::OAuthIdentityTaken => Self::new(
                StatusCode::CONFLICT,
                ErrorCode::OAuthAccountExists,
                "该第三方账号已绑定到其他账号",
                Vec::new(),
            ),
            // The state did not come from here, is already spent, or has expired.
            // Answering 400 with a code the client can branch on is deliberate:
            // "start over" is actionable, and the alternative — proceeding — is the
            // login-CSRF hole this refusal exists to close.
            AuthError::OAuthStateInvalid => Self::new(
                StatusCode::BAD_REQUEST,
                ErrorCode::OAuthStateInvalid,
                "登录请求已失效，请重新发起第三方登录",
                Vec::new(),
            ),
            // The provider refused or was unreachable. `502 Bad Gateway` says
            // "an upstream failed", which is true, and the provider's own text is
            // never echoed: it is their vocabulary and may carry a secret.
            AuthError::OAuthProviderError(detail) => {
                tracing::warn!(detail = %detail, "third-party sign-in failed at the provider");
                Self::new(
                    StatusCode::BAD_GATEWAY,
                    ErrorCode::OAuthProviderError,
                    "第三方登录暂时不可用，请稍后再试或用邮箱登录",
                    Vec::new(),
                )
            }
            // No credentials for this provider on this instance. Normally
            // unreachable, because it is not offered — refused explicitly so a
            // hand-made request cannot drive a half-configured provider.
            AuthError::OAuthNotConfigured => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::OAuthNotConfigured,
                "本实例未配置该第三方登录",
                Vec::new(),
            ),
            // A limited session used where a finished account is required. The
            // client's move is to send the user back to the username step.
            AuthError::UsernameRequired => Self::new(
                StatusCode::FORBIDDEN,
                ErrorCode::UsernameRequired,
                "请先设置用户名，再使用该账号",
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

impl From<RealtimeError> for ApiError {
    /// A realtime failure on a REST request is a server fault, never a client one.
    ///
    /// The only realtime errors that can reach a handler are "the presence store
    /// failed" and "the presence audience could not be resolved" — the client's
    /// input has already been parsed and validated by then. So the cause is logged
    /// and the client gets the stable internal-error code, like every other
    /// unexpected failure.
    fn from(error: RealtimeError) -> Self {
        tracing::error!(%error, "realtime request failed");
        Self::internal()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let retry_after = self.retry_after_seconds;
        let mut response = (self.status, Json(self.body)).into_response();

        if let Some(seconds) = retry_after {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(RETRY_AFTER, value);
            }
        }

        response
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
