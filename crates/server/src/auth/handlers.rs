//! HTTP handlers for the auth endpoints.

use crate::auth::extract::AuthUser;
use crate::auth::{jwt, password, tokens, ws_ticket};
use crate::error::{AppError, ConflictKind};
use crate::state::AppState;
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use rand::Rng;
use serde::{Deserialize, Serialize};
use time::{Duration as TimeDuration, OffsetDateTime};
use uuid::Uuid;

pub const CODE_TTL_SECS: i64 = 300;

// ---------------------------------------------------------------------------
// Request/response payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Email,
    Phone,
}

impl Channel {
    fn as_str(&self) -> &'static str {
        match self {
            Channel::Email => "email",
            Channel::Phone => "phone",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RequestCodeRequest {
    pub channel: Channel,
    pub target: String,
}

#[derive(Debug, Serialize)]
pub struct RequestCodeResponse {
    pub expires_in_secs: i64,
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub channel: Channel,
    pub target: String,
    pub code: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub user_id: Uuid,
    pub username: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub identifier: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct TokenPairResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

/// Login answers with the caller's profile alongside tokens so clients can
/// display identity without decoding JWTs (register already does the same).
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub user_id: Uuid,
    pub username: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Serialize)]
pub struct WsTicketResponse {
    pub ticket: String,
}

// ---------------------------------------------------------------------------
// Input validation helpers
// ---------------------------------------------------------------------------

/// Normalizes and sanity-checks a registration/login target.
fn normalize_target(channel: &Channel, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    match channel {
        Channel::Email => {
            let lowered = trimmed.to_lowercase();
            let (local, domain) = lowered.split_once('@')?;
            if local.is_empty() || domain.is_empty() || !domain.contains('.') {
                return None;
            }
            (lowered.len() >= 5 && lowered.len() <= 254).then_some(lowered)
        }
        Channel::Phone => {
            let digits = trimmed.strip_prefix('+').unwrap_or(trimmed);
            let plausible = (5..=20).contains(&digits.len())
                && digits.chars().all(|c| c.is_ascii_digit());
            plausible.then(|| trimmed.to_string())
        }
    }
}

/// `^[a-z0-9_]{3,32}$` without pulling in a regex dependency.
fn is_valid_username(username: &str) -> bool {
    (3..=32).contains(&username.len())
        && username
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn validate_credentials_inputs(username: &str, password: &str) -> Result<(), AppError> {
    if !is_valid_username(username) {
        return Err(AppError::Validation(
            "username must be 3-32 characters of [a-z0-9_] only".into(),
        ));
    }
    if password.len() < 8 {
        return Err(AppError::Validation(
            "password must be at least 8 characters".into(),
        ));
    }
    if password.len() > 128 {
        return Err(AppError::Validation(
            "password must be at most 128 characters".into(),
        ));
    }
    Ok(())
}

fn bad_json(rejection: JsonRejection) -> AppError {
    AppError::BadRequest(rejection.body_text())
}

fn six_digit_code() -> String {
    format!("{:06}", rand::rng().random_range(0..1_000_000u32))
}

/// Maps insert failures onto the public error model by constraint name.
fn classify_insert_error(err: sqlx::Error) -> AppError {
    if let Some(db_err) = err.as_database_error()
        && db_err.code().as_deref() == Some("23505")
    {
        return match db_err.constraint() {
            Some("users_username_key") => AppError::Conflict(ConflictKind::UsernameTaken),
            Some("auth_identities_kind_value_key") => {
                AppError::Conflict(ConflictKind::IdentityAlreadyBound)
            }
            _ => AppError::Internal(anyhow::anyhow!("unexpected unique violation: {err}")),
        };
    }
    AppError::Internal(err.into())
}

struct SessionTokens {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

/// Creates a refresh-token row and signs an access token for `user_id`.
async fn issue_session(state: &AppState, user_id: Uuid) -> anyhow::Result<SessionTokens> {
    let (refresh_wire, refresh_hash) = tokens::generate();
    let expires_at = OffsetDateTime::now_utc() + TimeDuration::days(tokens::REFRESH_TTL_DAYS);
    sqlx::query(
        "INSERT INTO refresh_tokens (id, user_id, token_hash, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(refresh_hash)
    .bind(expires_at)
    .execute(&state.pool)
    .await?;
    let access_token = jwt::sign_access(&state.jwt_secret, user_id)?;
    Ok(SessionTokens {
        access_token,
        refresh_token: refresh_wire,
        expires_in: jwt::ACCESS_TTL_SECS,
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/auth/request-code
pub async fn request_code(
    State(state): State<AppState>,
    payload: Result<Json<RequestCodeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<RequestCodeResponse>), AppError> {
    let Json(req) = payload.map_err(bad_json)?;
    let target = normalize_target(&req.channel, &req.target).ok_or_else(|| {
        AppError::Validation(format!(
            "invalid {} address format",
            req.channel.as_str()
        ))
    })?;

    let code = six_digit_code();
    // Dev convenience: the well-known dev code is honored at verification time
    // (see below); the random code is still issued and logged for realism.
    tracing::info!("verification code for {} is {}", target, code);
    state.codes.issue(
        req.channel.as_str(),
        &target,
        std::time::Duration::from_secs(u64::try_from(CODE_TTL_SECS).unwrap_or(300)),
        code,
    );

    Ok((
        StatusCode::OK,
        Json(RequestCodeResponse {
            expires_in_secs: CODE_TTL_SECS,
        }),
    ))
}

/// POST /api/auth/register — verifies the code, creates user + identity,
/// issues a session (auto-login).
pub async fn register(
    State(state): State<AppState>,
    payload: Result<Json<RegisterRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<RegisterResponse>), AppError> {
    let Json(req) = payload.map_err(bad_json)?;
    let channel_str = req.channel.as_str();
    let target = normalize_target(&req.channel, &req.target).ok_or_else(|| {
        AppError::Validation(format!("invalid {} address format", channel_str))
    })?;
    let username = req.username.trim().to_string();
    validate_credentials_inputs(&username, &req.password)?;

    // Wrong code vs unknown target: identical generic 401 (anti-enumeration).
    let dev_code_allowed = cfg!(debug_assertions);
    if !state.codes.verify(channel_str, &target, &req.code, dev_code_allowed) {
        return Err(AppError::InvalidCredentials);
    }
    state.codes.consume(channel_str, &target);

    let user_id = Uuid::now_v7();
    let password_hash =
        password::hash_password(&req.password).map_err(AppError::internal)?;

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    sqlx::query("INSERT INTO users (id, username, display_name, password_hash) VALUES ($1, $2, '', $3)")
        .bind(user_id)
        .bind(&username)
        .bind(password_hash)
        .execute(&mut *tx)
        .await
        .map_err(classify_insert_error)?;
    sqlx::query(
        "INSERT INTO auth_identities (id, user_id, kind, value, verified_at) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(channel_str)
    .bind(&target)
    .bind(OffsetDateTime::now_utc())
    .execute(&mut *tx)
    .await
    .map_err(classify_insert_error)?;
    tx.commit().await.map_err(AppError::internal)?;

    let session = issue_session(&state, user_id).await.map_err(AppError::internal)?;
    tracing::info!(%user_id, %username, "user registered via {channel_str}");
    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            user_id,
            username,
            access_token: session.access_token,
            refresh_token: session.refresh_token,
            expires_in: session.expires_in,
        }),
    ))
}

/// POST /api/auth/login — identifier matches username OR any identity value
/// (emails compared case-insensitively).
pub async fn login(
    State(state): State<AppState>,
    payload: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Json<LoginResponse>, AppError> {
    let Json(req) = payload.map_err(bad_json)?;
    let identifier = req.identifier.trim();

    let row: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT u.id, u.username, u.password_hash FROM users u \
         WHERE u.username = lower($1) \
            OR EXISTS (SELECT 1 FROM auth_identities ai \
                       WHERE ai.user_id = u.id AND lower(ai.value) = lower($1)) \
         LIMIT 1",
    )
    .bind(identifier)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let (user_id, username, password_hash) = row.ok_or(AppError::InvalidCredentials)?;
    // Unknown identifier and wrong password collapse into the same 401 body.
    if !password::verify_password(&req.password, &password_hash) {
        return Err(AppError::InvalidCredentials);
    }

    let session = issue_session(&state, user_id).await.map_err(AppError::internal)?;
    Ok(Json(LoginResponse {
        access_token: session.access_token,
        refresh_token: session.refresh_token,
        expires_in: session.expires_in,
        user_id,
        username,
    }))
}

/// POST /api/auth/refresh — rotates: predecessor revoked, successor issued.
pub async fn refresh(
    State(state): State<AppState>,
    payload: Result<Json<RefreshRequest>, JsonRejection>,
) -> Result<Json<TokenPairResponse>, AppError> {
    let Json(req) = payload.map_err(bad_json)?;
    let presented_hash = tokens::stored_hash(&req.refresh_token)
        .map_err(|_| AppError::InvalidCredentials)?;

    let row: Option<(Uuid, Uuid, OffsetDateTime, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT id, user_id, expires_at, revoked_at FROM refresh_tokens WHERE token_hash = $1",
    )
    .bind(&presented_hash)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::internal)?;

    let (token_id, user_id, expires_at, revoked_at) = row.ok_or(AppError::InvalidCredentials)?;
    if revoked_at.is_some() || expires_at < OffsetDateTime::now_utc() {
        return Err(AppError::InvalidCredentials);
    }

    let (new_refresh_wire, new_refresh_hash) = tokens::generate();
    let new_expires_at = OffsetDateTime::now_utc() + TimeDuration::days(tokens::REFRESH_TTL_DAYS);

    let mut tx = state.pool.begin().await.map_err(AppError::internal)?;
    // Guard against concurrent double-spend of the same refresh token.
    let rotated = sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(token_id)
    .execute(&mut *tx)
    .await
    .map_err(AppError::internal)?;
    if rotated.rows_affected() == 0 {
        return Err(AppError::InvalidCredentials);
    }
    sqlx::query(
        "INSERT INTO refresh_tokens (id, user_id, token_hash, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(new_refresh_hash)
    .bind(new_expires_at)
    .execute(&mut *tx)
    .await
    .map_err(AppError::internal)?;
    tx.commit().await.map_err(AppError::internal)?;

    let access_token = jwt::sign_access(&state.jwt_secret, user_id).map_err(AppError::internal)?;
    Ok(Json(TokenPairResponse {
        access_token,
        refresh_token: new_refresh_wire,
        expires_in: jwt::ACCESS_TTL_SECS,
    }))
}

/// POST /api/auth/ws-ticket — Bearer-authenticated; mints a single-use,
/// 5-minute WebSocket ticket.
pub async fn ws_ticket(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<WsTicketResponse>, AppError> {
    let ticket = ws_ticket::issue(&state, user.0).await.map_err(AppError::internal)?;
    Ok(Json(WsTicketResponse { ticket }))
}
