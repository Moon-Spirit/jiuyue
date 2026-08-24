//! Refresh tokens: 48 random bytes, base64url-encoded on the wire, stored as
//! SHA-256 digest (`bytea`). Rotation revokes the predecessor.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub const REFRESH_TTL_DAYS: i64 = 30;
pub const REFRESH_TTL_SECS: i64 = REFRESH_TTL_DAYS * 24 * 3600;

/// Generates a new opaque token: `(wire_token_base64url, sha256_digest_bytes)`.
pub fn generate() -> (String, Vec<u8>) {
    let mut bytes = [0u8; 48];
    rand::rng().fill_bytes(&mut bytes);
    let wire = URL_SAFE_NO_PAD.encode(bytes);
    (wire, sha256_of_decoded(&bytes))
}

fn sha256_of_decoded(decoded: &[u8]) -> Vec<u8> {
    Sha256::digest(decoded).to_vec()
}

/// Decodes a wire token and returns its stored-form digest. Fails on any
/// non-canonical input, which callers map to a generic 401.
pub fn stored_hash(wire_token: &str) -> anyhow::Result<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD
        .decode(wire_token)
        .map_err(|_| anyhow::anyhow!("malformed refresh token"))?;
    if decoded.len() != 48 {
        anyhow::bail!("malformed refresh token");
    }
    Ok(sha256_of_decoded(&decoded))
}
