//! Errors raised by the chat domain.
//!
//! Every fallible operation returns a `Result` built from [`ChatError`]; library
//! code never panics on a recoverable condition. The variants are a vocabulary,
//! not strings: the HTTP layer maps them to status codes and the socket layer to
//! [`ChatError::code`], so "not a participant" is never confused with "no such
//! conversation" by scraping a message.
//!
//! [`ChatError::Validation`] carries field-level detail, exactly as
//! `jiuyue_auth::AuthError::Validation` does, so the client can attach the problem
//! to the offending input.

use jiuyue_contract::{ErrorCode, FieldError};
use thiserror::Error;

/// Failures the chat domain can report.
#[derive(Debug, Error)]
pub enum ChatError {
    /// One or more request fields failed validation.
    #[error("request validation failed")]
    Validation(Vec<FieldError>),

    /// No account matches the requested `@handle`.
    #[error("no account matches that @handle")]
    UserNotFound,

    /// The addressed Conversation does not exist.
    #[error("the conversation does not exist")]
    ConversationNotFound,

    /// The caller is authenticated but is not a Participant of the Conversation.
    #[error("the caller is not a participant of the conversation")]
    NotAParticipant,

    /// A database operation failed.
    #[error("chat storage failed")]
    Database(#[source] sqlx::Error),

    /// An invariant the caller cannot influence did not hold (for example the
    /// winning row of a concurrent insert was unreadable).
    #[error("chat operation could not complete")]
    Internal,
}

impl ChatError {
    /// The stable wire code for this failure.
    ///
    /// One vocabulary shared with the REST error envelope
    /// ([`jiuyue_contract::ErrorBody`]), so a socket rejection and an HTTP failure
    /// are the same kind of thing to a client.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Validation(_) => ErrorCode::ValidationFailed,
            Self::UserNotFound => ErrorCode::UserNotFound,
            Self::ConversationNotFound => ErrorCode::NotFound,
            Self::NotAParticipant => ErrorCode::NotAParticipant,
            Self::Database(_) | Self::Internal => ErrorCode::Internal,
        }
    }

    /// A human-readable reason (Chinese); a display fallback, not a contract.
    pub fn message(&self) -> &'static str {
        match self {
            Self::Validation(_) => "消息内容不合法",
            Self::UserNotFound => "找不到该用户",
            Self::ConversationNotFound => "会话不存在",
            Self::NotAParticipant => "你不是该会话的参与者",
            Self::Database(_) | Self::Internal => "服务器内部错误",
        }
    }

    /// The field-level problems, when this is a validation failure.
    pub fn fields(&self) -> &[FieldError] {
        match self {
            Self::Validation(fields) => fields,
            _ => &[],
        }
    }
}

impl From<sqlx::Error> for ChatError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}
