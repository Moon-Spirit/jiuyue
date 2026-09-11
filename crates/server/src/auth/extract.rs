//! `Authorization: Bearer <access-token>` extractor.

use crate::auth::jwt;
use crate::error::AppError;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use uuid::Uuid;

pub struct AuthUser(pub Uuid);

impl FromRequestParts<crate::state::AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &crate::state::AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(AppError::InvalidCredentials)?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or(AppError::InvalidCredentials)?;
        let user_id = jwt::verify_access(&state.jwt_secret, token)
            .map_err(|_| AppError::InvalidCredentials)?;
        Ok(AuthUser(user_id))
    }
}
