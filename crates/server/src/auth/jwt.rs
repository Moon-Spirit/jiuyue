//! HS256 access tokens: claims `{sub: user_id, exp}` with a 15-minute TTL.

use anyhow::Context;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct AccessClaims {
    pub sub: String,
    pub exp: i64,
}

pub const ACCESS_TTL_SECS: i64 = 15 * 60;

pub fn sign_access(secret: &str, user_id: uuid::Uuid) -> anyhow::Result<String> {
    let exp = time::OffsetDateTime::now_utc().unix_timestamp() + ACCESS_TTL_SECS;
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &AccessClaims {
            sub: user_id.to_string(),
            exp,
        },
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .context("failed to sign access token")?;
    Ok(token)
}

/// Verifies signature + expiry; returns the authenticated user id.
pub fn verify_access(secret: &str, token: &str) -> anyhow::Result<uuid::Uuid> {
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    let data = jsonwebtoken::decode::<AccessClaims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .context("invalid access token")?;
    data.claims
        .sub
        .parse::<uuid::Uuid>()
        .context("bad sub claim")
}
