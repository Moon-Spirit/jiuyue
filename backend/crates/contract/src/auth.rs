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
/// Registration succeeds and signs the new Device in immediately; the account
/// starts with `email_verified` false and a verification link is emailed. Some
/// actions (opening a Conversation) are gated on verification — see
/// `docs/adr/0015-email-verification-and-the-mailer-seam.md`.
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

/// `POST /auth/verify-email` body.
///
/// The token is the opaque value from the verification link. It is single-use and
/// time-limited, and the server stores only its digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct VerifyEmailRequest {
    /// The token carried by the verification link (`?token=…`).
    pub token: String,
}

/// `POST /auth/forgot-password` body.
///
/// The response never says whether the address exists — see
/// [`crate::auth::RequestAccepted`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ForgotPasswordRequest {
    /// Address to send a reset link to, if an account uses it.
    pub email: String,
}

/// `POST /auth/reset-password` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ResetPasswordRequest {
    /// The token carried by the reset link (`?token=…`); single-use and expiring.
    pub token: String,
    /// The new cleartext password; hashed with Argon2id on arrival, never stored.
    pub password: String,
}

/// `POST /auth/resend-verification` body.
///
/// Like forgot-password, the answer does not reveal whether the address exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ResendVerificationRequest {
    /// Address to re-send the verification link to, if an unverified account uses it.
    pub email: String,
}

/// A third-party identity provider.
///
/// A closed set, not free text: the value is both the `oauth_identities.provider`
/// key and what the client sends, and an unknown one is a validation failure
/// rather than a lookup that quietly finds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum OAuthProvider {
    /// `github.com` — one `user` endpoint, emails via a second call.
    GitHub,
    /// Google — OpenID Connect with an ID token carrying `email` and
    /// `email_verified`.
    Google,
}

impl OAuthProvider {
    /// The stored spelling (matches the `oauth_identities.provider` check).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Google => "google",
        }
    }

    /// Parse the stored spelling; `None` for anything else.
    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "github" => Some(Self::GitHub),
            "google" => Some(Self::Google),
            _ => None,
        }
    }

    /// The name shown on the sign-in button.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::Google => "Google",
        }
    }
}

/// `GET /auth/oauth/providers` response.
///
/// Only providers the instance actually has credentials for appear here, which is
/// what lets the login page render a button per entry instead of guessing: a
/// provider with no client id/secret is *absent*, never present and failing on
/// click. The `authorize_url` needs no secret, so it is safe to hand to the
/// browser; the client secret never leaves the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OAuthProviderInfo {
    /// Which provider.
    pub provider: OAuthProvider,
    /// Display name for the button.
    pub display_name: String,
    /// Absolute URL the browser should be sent to, with `state` and the PKCE
    /// `code_challenge` already attached.
    pub authorize_url: String,
}

/// `GET /auth/oauth/providers` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OAuthProviders {
    /// Configured providers, in a stable order. May be empty.
    pub providers: Vec<OAuthProviderInfo>,
}

/// `POST /auth/oauth/callback` body.
///
/// No `redirect_uri` is accepted: it is fixed by configuration, because a
/// redirect target the caller could choose is the open-redirect half of the
/// OAuth code-intercept problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OAuthCallbackRequest {
    /// Which provider is answering.
    pub provider: OAuthProvider,
    /// The authorization code from the provider's redirect.
    pub code: String,
    /// The `state` the round trip was started with; single-use and expiring.
    pub state: String,
}

/// The username a first-time provider user chose.
///
/// The limited session travels in the `Authorization` header, so it is not a
/// field here — this body is only the choice itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CompleteOAuthSignInRequest {
    /// The `@handle` to claim; validated exactly as a registration username is.
    pub username: String,
    /// Optional display name; the server falls back to the username.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// What must happen before a first-time provider user has an account they can use.
///
/// Its presence is the machine-readable statement "this caller holds a *limited*
/// session". The limited token is opaque, so every protected endpoint — which
/// accepts an access token and only an access token — refuses it without needing
/// to know this type exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OAuthOnboarding {
    /// Bearer token for `POST /auth/oauth/complete` and nothing else.
    pub limited_token: String,
    /// What the provider called the account, so the step can suggest a handle.
    /// Not a username and not necessarily valid as one.
    pub suggested_username: String,
    /// The address the provider reported, when it reported one.
    pub email: Option<String>,
    /// Where the client wanted to land, when it asked and the flow was not deep
    /// enough to honour it yet. Passed through so the intent is not lost at the
    /// username step.
    pub redirect_path: Option<String>,
}

/// `POST /auth/oauth/callback` response.
///
/// Exactly one half is present. `tokens`/`user` mean the caller is signed in;
/// `onboarding` means the caller must first choose a username. Modelling it as
/// one optional *pair* rather than two independent optionals is what makes "a
/// session without a user" unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OAuthCallbackResponse {
    /// Present when the sign-in completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session: Option<OAuthSession>,
    /// Present when the account exists but has no username yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub onboarding: Option<OAuthOnboarding>,
}

/// The signed-in half of [`OAuthCallbackResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OAuthSession {
    /// The authenticated user.
    pub user: UserProfile,
    /// Credentials for subsequent requests.
    pub tokens: TokenPair,
    /// Where the client asked to be returned to, when it asked. A relative path.
    pub redirect_path: Option<String>,
}

/// Generic "we accepted your request" body.
///
/// Used only by the two endpoints that must answer identically whether or not an
/// account exists (`forgot-password`, `resend-verification`). Returning one fixed
/// shape — rather than `{"sent": true}` for a hit and something else for a miss —
/// is what keeps the response from becoming an account-existence oracle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RequestAccepted {
    /// Always `true`: the request was accepted, independent of any account lookup.
    pub accepted: bool,
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
    /// Whether the email has been verified by following the emailed link.
    /// `false` restricts the account from opening Conversations.
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
    /// The account exists but its email has not been verified, and the action is
    /// gated on verification. Distinct so the client can prompt the user to open
    /// their inbox (and offer a re-send) instead of showing a generic denial.
    EmailNotVerified,
    /// The one-shot link is well-formed but past its lifetime. Separate from
    /// [`Self::TokenInvalid`] because "expired, ask for a new one" is actionable
    /// advice to a real user.
    TokenExpired,
    /// The one-shot link is unknown, malformed, or already spent. Deliberately
    /// does not separate "has been used" from "never existed": both mean "start
    /// over and request a fresh link".
    TokenInvalid,
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
    /// A third-party sign-in was attempted with an email that already belongs to
    /// another account.
    ///
    /// This is deliberately **not** an automatic link. Attaching the provider to
    /// the existing account would let anyone who can make a provider assert an
    /// address walk into it; creating a second account with the same address is
    /// impossible and wrong anyway. The client's instruction is: sign in the way
    /// you normally do, then link the provider from settings.
    ///
    /// The wire spelling is pinned because serde's `SCREAMING_SNAKE_CASE` splits
    /// `OAuthAccountExists` at the capital `A` of `Auth`, which would produce
    /// `O_AUTH_ACCOUNT_EXISTS` — a name no client should have to know.
    #[serde(rename = "OAUTH_ACCOUNT_EXISTS")]
    OAuthAccountExists,
    /// The `state` is unknown, already spent, or past its short lifetime, or the
    /// round trip otherwise did not originate here. Never proceed: this is the
    /// login-CSRF refusal.
    #[serde(rename = "OAUTH_STATE_INVALID")]
    OAuthStateInvalid,
    /// The provider itself refused: a bad `code`, a failed token exchange, an
    /// unusable profile, or a network failure. The provider's own error text is
    /// logged, never returned — it is their vocabulary, and it may echo a secret.
    #[serde(rename = "OAUTH_PROVIDER_ERROR")]
    OAuthProviderError,
    /// The requested provider has no credentials configured on this instance.
    /// Normally unreachable, because an unconfigured provider is absent from
    /// `GET /auth/oauth/providers`; it is refused explicitly rather than trusting
    /// the client to only ask for what it was offered.
    #[serde(rename = "OAUTH_NOT_CONFIGURED")]
    OAuthNotConfigured,
    /// The caller holds a limited session and tried to use it somewhere other
    /// than the username step. Distinct so the client can route them back rather
    /// than showing a generic "session expired".
    UsernameRequired,
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
    /// The field does not match a sibling field it must repeat. Emitted by the
    /// client for a password-confirmation box; the server never needs to send it,
    /// because it never receives a confirmation field.
    Mismatch,
}

#[cfg(test)]
mod tests {
    use super::{
        ErrorBody, ErrorCode, FieldError, FieldErrorCode, ForgotPasswordRequest, RegisterRequest,
        RequestAccepted, ResendVerificationRequest, ResetPasswordRequest, VerifyEmailRequest,
    };

    #[test]
    fn the_one_shot_link_requests_round_trip() {
        let verify: VerifyEmailRequest =
            serde_json::from_str(r#"{"token":"abc"}"#).expect("a verify body must decode");
        assert_eq!(verify.token, "abc");

        let reset: ResetPasswordRequest =
            serde_json::from_str(r#"{"token":"abc","password":"secret123"}"#)
                .expect("a reset body must decode");
        assert_eq!(reset.password, "secret123");

        let forgot: ForgotPasswordRequest = serde_json::from_str(r#"{"email":"a@b.com"}"#)
            .expect("a forgot-password body must decode");
        assert_eq!(forgot.email, "a@b.com");

        let resend: ResendVerificationRequest =
            serde_json::from_str(r#"{"email":"a@b.com"}"#).expect("a resend body must decode");
        assert_eq!(resend.email, "a@b.com");

        let accepted = serde_json::to_value(RequestAccepted { accepted: true })
            .expect("the accepted body must serialise");
        assert_eq!(accepted, serde_json::json!({ "accepted": true }));
    }

    #[test]
    fn the_verification_failure_codes_are_stable_on_the_wire() {
        for (code, expected) in [
            (ErrorCode::EmailNotVerified, "EMAIL_NOT_VERIFIED"),
            (ErrorCode::TokenExpired, "TOKEN_EXPIRED"),
            (ErrorCode::TokenInvalid, "TOKEN_INVALID"),
        ] {
            let wire = serde_json::to_value(code).expect("a code must serialise");
            assert_eq!(wire, serde_json::json!(expected));
        }
    }

    #[test]
    fn the_oauth_failure_codes_are_stable_on_the_wire() {
        for (code, expected) in [
            (ErrorCode::OAuthAccountExists, "OAUTH_ACCOUNT_EXISTS"),
            (ErrorCode::OAuthStateInvalid, "OAUTH_STATE_INVALID"),
            (ErrorCode::OAuthProviderError, "OAUTH_PROVIDER_ERROR"),
            (ErrorCode::OAuthNotConfigured, "OAUTH_NOT_CONFIGURED"),
            (ErrorCode::UsernameRequired, "USERNAME_REQUIRED"),
        ] {
            let wire = serde_json::to_value(code).expect("a code must serialise");
            assert_eq!(wire, serde_json::json!(expected));
        }
    }

    #[test]
    fn a_provider_round_trips_between_its_wire_and_stored_spellings() {
        use super::OAuthProvider;

        for (provider, wire) in [
            (OAuthProvider::GitHub, "github"),
            (OAuthProvider::Google, "google"),
        ] {
            assert_eq!(
                serde_json::to_value(provider).expect("a provider must serialise"),
                serde_json::json!(wire)
            );
            assert_eq!(provider.as_str(), wire, "the stored spelling");
            assert_eq!(OAuthProvider::from_str_opt(wire), Some(provider));
        }

        assert_eq!(
            OAuthProvider::from_str_opt("facebook"),
            None,
            "an unknown provider must not resolve to one we support"
        );
    }

    #[test]
    fn a_callback_answers_with_exactly_one_half() {
        use super::{OAuthCallbackResponse, OAuthOnboarding, TokenPair, UserProfile};

        let signed_in = OAuthCallbackResponse {
            session: Some(super::OAuthSession {
                user: UserProfile {
                    id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
                    username: "alice".to_owned(),
                    email: "alice@example.com".to_owned(),
                    display_name: "alice".to_owned(),
                    avatar_url: None,
                    email_verified: false,
                    created_at_ms: 1_700_000_000_000,
                },
                tokens: TokenPair {
                    access_token: "access".to_owned(),
                    refresh_token: "refresh".to_owned(),
                    token_type: "Bearer".to_owned(),
                    expires_in: 900,
                },
                redirect_path: Some("/chat".to_owned()),
            }),
            onboarding: None,
        };

        let wire = serde_json::to_value(&signed_in).expect("a callback response must serialise");
        assert!(wire.get("session").is_some(), "the session half is present");
        assert!(
            wire.get("onboarding").is_none(),
            "the absent half is omitted rather than null, so a client can branch on presence"
        );

        let first_time = OAuthCallbackResponse {
            session: None,
            onboarding: Some(OAuthOnboarding {
                limited_token: "limited".to_owned(),
                suggested_username: "octocat".to_owned(),
                email: None,
                redirect_path: Some("/chat".to_owned()),
            }),
        };

        let wire = serde_json::to_value(&first_time).expect("a callback response must serialise");
        assert!(wire.get("onboarding").is_some());
        assert!(wire.get("session").is_none());

        // The onboarding half must decode, including a provider that reported no
        // address at all.
        let decoded: OAuthCallbackResponse = serde_json::from_value(serde_json::json!({
            "onboarding": {
                "limited_token": "t",
                "suggested_username": "u",
                "email": null,
                "redirect_path": null,
            }
        }))
        .expect("the onboarding half must decode");
        assert!(decoded.session.is_none());
        assert_eq!(decoded.onboarding.expect("the onboarding half").email, None);
    }

    #[test]
    fn a_callback_request_requires_the_state() {
        use super::OAuthCallbackRequest;

        let decoded: OAuthCallbackRequest =
            serde_json::from_str(r#"{"provider":"github","code":"c","state":"s"}"#)
                .expect("a callback body must decode");
        assert_eq!(decoded.provider, super::OAuthProvider::GitHub);
        assert_eq!(decoded.code, "c");
        assert_eq!(decoded.state, "s");

        assert!(
            serde_json::from_str::<OAuthCallbackRequest>(r#"{"provider":"github","code":"c"}"#)
                .is_err(),
            "a body without a state must not decode: the CSRF check cannot be optional"
        );
    }

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
