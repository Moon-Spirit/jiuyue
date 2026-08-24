//! One-time WebSocket tickets: Redis key `ws_ticket:{ticket}` -> user_id,
//! TTL 300s. Redemption atomically deletes the key on read, so a ticket can
//! only ever be consumed once.

use crate::error::AppError;
use crate::state::AppState;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use uuid::Uuid;

pub const TICKET_TTL_SECS: u64 = 300;

fn key(ticket: &str) -> String {
    format!("ws_ticket:{ticket}")
}

/// Mints a fresh single-use ticket bound to `user_id`.
pub async fn issue(state: &AppState, user_id: Uuid) -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let ticket = URL_SAFE_NO_PAD.encode(bytes);
    let mut conn = state.redis.clone();
    redis::cmd("SETEX")
        .arg(key(&ticket))
        .arg(TICKET_TTL_SECS)
        .arg(user_id.to_string())
        .query_async::<()>(&mut conn)
        .await?;
    Ok(ticket)
}

/// Atomically consumes a ticket via WATCH/GET/MULTI/DEL/EXEC (GETDEL semantics
/// without requiring Redis >= 6.2): exactly one contender observes the value and
/// its DEL commits; every later attempt finds an empty key. Second redemption
/// and expired/unknown tickets collapse into `InvalidCredentials`.
pub async fn consume(state: &AppState, ticket: &str) -> Result<Uuid, AppError> {
    let key = key(ticket);
    let mut conn = state.redis.clone();

    let mut held: Option<String> = None;
    loop {
        redis::cmd("WATCH")
            .arg(&key)
            .query_async::<()>(&mut conn)
            .await
            .map_err(AppError::internal)?;
        let value: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .map_err(AppError::internal)?;
        let user_id = match value {
            Some(user_id) => user_id,
            None => {
                // Nothing to consume; clear the WATCH before leaving.
                redis::cmd("UNWATCH")
                    .query_async::<()>(&mut conn)
                    .await
                    .map_err(AppError::internal)?;
                break;
            }
        };
        // EXEC yields Nil when the watched key changed since WATCH, i.e. another
        // consumer won the race; `Option<()>` maps that to `None` and we retry.
        let committed: Option<()> = redis::pipe()
            .atomic()
            .del(key.as_str())
            .ignore()
            .query_async(&mut conn)
            .await
            .map_err(AppError::internal)?;
        if committed.is_some() {
            held = Some(user_id);
            break;
        }
    }

    let user_id =
        held.ok_or(AppError::InvalidCredentials)?.parse::<Uuid>().map_err(|err| {
            AppError::Internal(anyhow::anyhow!("ws_ticket held malformed user id: {err}"))
        })?;
    Ok(user_id)
}
