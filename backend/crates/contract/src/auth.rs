//! REST request/response types for the identity module.
//!
//! These types are the single source of truth for the auth HTTP contract, exactly
//! as [`crate::envelope`] is for the WebSocket wire. `jiuyue-auth` and
//! `jiuyue-server` produce and consume them, and `ts-rs` exports them to
//! `frontend/src/generated`, so the frontend never re-declares a server shape.
//!
//! Two rules the rest of the stack relies on:
//!
//! - Numbers that are conceptually 64-bit are annotated `#[ts(type = "number")]`.
//!   Without it `ts-rs` would emit `bigint`, which `JSON.parse` never produces.
//! - Every failure the client must distinguish has its own [`ErrorCode`], and a
//!   validation failure names the offending field in [`FieldError::field`]. A
//!   duplicate email is therefore separable from a generic failure by machine,
//!   not by scraping a human message.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// `POST /auth/register` body.
///
/// Registration does not require email verification yet; the account is usable
/// immediately with `email_verified` false until a later ticket changes that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RegisterRequest {
    /// `@handle`, lowercase `[a-z0-9_]{3,32}`.
    pub username: String,
    /// Email address; normalised to lowercase by the server.
    pub email: String,
    /// Cleartext password. Never stored or logged; hashed with Argon2id on arrival.
    pub password: String,
    /// Optional display name; the server falls back to the username when absent.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `POST /auth/login` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LoginRequest {
    /// Email address; matched case-insensitively via lowercase normalisation.
    pub email: String,
    /// Cleartext password, verified against the stored Argon2id hash.
    pub password: String,
}

/// `POST /auth/refresh` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RefreshRequest {
    /// The opaque refresh token handed out at login.
    pub refresh_token: String,
}

/// A freshly minted pair of credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TokenPair {
    /// Short-lived signed JWT (HS256) presented as `Authorization: Bearer …`.
    pub access_token: String,
    /// Long-lived opaque token; only its SHA-256 digest is stored server-side.
    pub refresh_token: String,
    /// Always `"Bearer"`; spelled out so the client never hardcodes the scheme.
    pub token_type: String,
    /// Access-token lifetime in whole seconds from issue time.
    #[ts(type = "number")]
    pub expires_in: u64,
}

/// The publicly visible part of a User.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UserProfile {
    /// ULID of the User.
    pub id: String,
    /// `@handle`.
    pub username: String,
    /// Normalised (lowercase) email address.
    pub email: String,
    /// Display name shown to other users.
    pub display_name: String,
    /// Avatar URL, when one has been set.
    pub avatar_url: Option<String>,
    /// Whether the email has been verified (always false until the email ticket).
    pub email_verified: bool,
    /// Account creation time, milliseconds since the Unix epoch.
    #[ts(type = "number")]
    pub created_at_ms: i64,
}

/// Result of a successful register or login: who you are plus working credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AuthSession {
    /// The authenticated user.
    pub user: UserProfile,
    /// The credentials to present on subsequent requests.
    pub tokens: TokenPair,
}

/// `GET /auth/whoami` response — the minimal proof that an access token works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WhoAmI {
    /// ULID of the authenticated User.
    pub user_id: String,
    /// ULID of the Session (Device) the access token belongs to.
    pub session_id: String,
    /// The authenticated user's `@handle`.
    pub username: String,
}

/// Top-level error envelope returned by every auth endpoint on failure.
///
/// The body is always `{"error": { ... }}`, so a non-2xx response is
/// distinguishable from a success by shape and never needs a separate status map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ErrorBody {
    /// The failure itself.
    pub error: ErrorDetail,
}

/// Machine-readable failure description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ErrorDetail {
    /// Stable failure kind; the client branches on this, not on [`Self::message`].
    pub code: ErrorCode,
    /// Human-readable message (Chinese); a display fallback, not a contract.
    pub message: String,
    /// Field-level problems; empty when the failure is not attributable to a field.
    #[serde(default)]
    pub fields: Vec<FieldError>,
    /// Whole seconds the caller must wait before retrying, when the failure is a
    /// throttle or a lockout. `null` for every other failure.
    ///
    /// This travels as data rather than as prose so the client can render a real
    /// countdown instead of parsing a human message. It is the *length of the
    /// imposed backoff*, not a live countdown: it is identical for every source
    /// under the same policy at the same moment, which is what keeps the
    /// throttled response from becoming an account-enumeration oracle.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub retry_after_seconds: Option<u64>,
}

/// One field-level validation problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FieldError {
    /// Name of the request field, matching the `RegisterRequest` / `LoginRequest` field.
    pub field: String,
    /// Stable code the client maps to a localised message.
    pub code: FieldErrorCode,
    /// Human-readable message (Chinese) for this field.
    pub message: String,
}

/// Stable failure kinds for the JSON API.
///
/// One vocabulary shared by every module that answers with the [`ErrorBody`]
/// envelope — identity today, chat alongside it. Codes are additive: a client
/// branches on the ones it knows and treats the rest as a generic failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export)]
pub enum ErrorCode {
    /// One or more request fields failed validation; see [`ErrorDetail::fields`].
    ValidationFailed,
    /// The email is already registered; distinguishable from a generic failure.
    EmailTaken,
    /// The username is already taken.
    UsernameTaken,
    /// Email/password pair did not match. Deliberately does not say which was wrong.
    InvalidCredentials,
    /// Too many failed attempts from this source in the recent window; the caller
    /// must back off. [`ErrorDetail::retry_after_seconds`] carries the wait.
    TooManyAttempts,
    /// Repeated throttling from this source escalated to a lockout. Distinct from
    /// [`Self::TooManyAttempts`] so a client can say so plainly; the wait is in
    /// [`ErrorDetail::retry_after_seconds`].
    LockedOut,
    /// The access token is missing, malformed, expired, or its session was revoked.
    Unauthenticated,
    /// The server failed for an internal reason; the cause is only in the logs.
    Internal,
    /// The identity subsystem is not available on this instance.
    Unavailable,
    /// The addressed Conversation does not exist, or is not visible to the caller.
    NotFound,
    /// No account matches the requested `@handle`.
    UserNotFound,
    /// The caller is authenticated but is not a Participant of the Conversation.
    NotAParticipant,
    /// The caller is a Participant but their Role does not permit the action.
    /// Distinct from [`Self::NotAParticipant`]: they can see the Conversation,
    /// they just may not do this to it.
    Forbidden,
    /// The action conflicts with the current state (already a member, already the
    /// owner, the last owner trying to leave, and so on).
    Conflict,
}

/// Stable per-field validation codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export)]
pub enum FieldErrorCode {
    /// The field is empty or absent.
    Required,
    /// The field is present but does not match the accepted format.
    InvalidFormat,
    /// The field is shorter than the minimum length.
    TooShort,
    /// The field is longer than the maximum length.
    TooLong,
    /// The field does not meet the strength policy (password: needs letters and digits).
    Weak,
    /// The field's value is already in use by another account.
    Taken,
}

#[cfg(test)]
mod tests {
    use super::{ErrorBody, ErrorCode, FieldError, FieldErrorCode, RegisterRequest};

    #[test]
    fn optional_display_name_may_be_omitted() {
        let request: RegisterRequest = serde_json::from_str(
            r#"{"username":"alice","email":"alice@example.com","password":"secret123"}"#,
        )
        .expect("a registration without display_name must decode");

        assert_eq!(request.display_name, None);
    }

    #[test]
    fn error_body_uses_the_stable_screeching_snake_codes() {
        let body = ErrorBody {
            error: super::ErrorDetail {
                code: ErrorCode::ValidationFailed,
                message: "请求参数无效".to_owned(),
                fields: vec![FieldError {
                    field: "email".to_owned(),
                    code: FieldErrorCode::InvalidFormat,
                    message: "邮箱格式不正确".to_owned(),
                }],
                retry_after_seconds: None,
            },
        };

        let wire = serde_json::to_value(&body).expect("the error body must serialise");

        assert_eq!(wire["error"]["code"], "VALIDATION_FAILED");
        assert_eq!(wire["error"]["fields"][0]["field"], "email");
        assert_eq!(wire["error"]["fields"][0]["code"], "INVALID_FORMAT");
    }

    #[test]
    fn empty_fields_serialise_as_an_array_not_null() {
        let body = ErrorBody {
            error: super::ErrorDetail {
                code: ErrorCode::InvalidCredentials,
                message: "邮箱或密码不正确".to_owned(),
                fields: Vec::new(),
                retry_after_seconds: None,
            },
        };

        let wire = serde_json::to_value(&body).expect("the error body must serialise");

        assert_eq!(wire["error"]["fields"], serde_json::json!([]));
    }

    #[test]
    fn a_retry_hint_travels_as_a_number_and_is_absent_by_default() {
        let throttled = ErrorBody {
            error: super::ErrorDetail {
                code: ErrorCode::TooManyAttempts,
                message: "登录尝试过于频繁，请稍后再试".to_owned(),
                fields: Vec::new(),
                retry_after_seconds: Some(60),
            },
        };
        let wire = serde_json::to_value(&throttled).expect("the error body must serialise");
        assert_eq!(wire["error"]["code"], "TOO_MANY_ATTEMPTS");
        assert_eq!(wire["error"]["retry_after_seconds"], serde_json::json!(60));

        let locked = ErrorBody {
            error: super::ErrorDetail {
                code: ErrorCode::LockedOut,
                message: "登录失败次数过多，请稍后再试".to_owned(),
                fields: Vec::new(),
                retry_after_seconds: Some(900),
            },
        };
        let wire = serde_json::to_value(&locked).expect("the error body must serialise");
        assert_eq!(wire["error"]["code"], "LOCKED_OUT");

        // A code the client does not know must still decode, and a payload that
        // predates the field must decode with no hint rather than fail.
        let legacy: ErrorBody = serde_json::from_str(
            r#"{"error":{"code":"INVALID_CREDENTIALS","message":"邮箱或密码不正确","fields":[]}}"#,
        )
        .expect("a body without the retry hint must decode");
        assert_eq!(legacy.error.retry_after_seconds, None);
    }
}
