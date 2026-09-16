//! REST handler for presence.
//!
//! `GET /presence?user_ids=a,b,c` answers the presence of the named Users — the
//! half of presence the socket cannot cover. A live `ServerEvent::Presence` only
//! arrives *when a User's reachability changes*, so a client that has just opened
//! a Conversation needs one read to render the current state without waiting for
//! the next change. That is all this route is.
//!
//! # Visibility
//!
//! The answer is **filtered, not merely looked up**: only a User who shares a
//! Conversation with the caller appears, because presence visibility already is a
//! domain decision (see `jiuyue_chat::ChatService::presence_audience`). A requested
//! User outside that audience is omitted rather than reported offline, so this
//! endpoint cannot be used to probe strangers, and issue #27 narrows the same seam
//! rather than adding a second rule.
//!
//! `user_ids` is a comma-separated list — the shape a query string carries without
//! repeated keys. It is parsed (split, trimmed, de-duplicated) at this boundary,
//! bounded by [`MAX_PRESENCE_QUERY`], and handed to the service as typed ids; no
//! raw query string travels any further.

use std::collections::HashSet;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use jiuyue_contract::{
    ErrorCode, FieldError, FieldErrorCode, MAX_PRESENCE_QUERY, PresenceList, PresenceQuery,
};

use super::api::{ApiError, bearer_token};
use crate::state::AppState;

/// The `/presence` routes.
pub fn router() -> Router<AppState> {
    Router::new().route("/presence", get(list_presence))
}

/// `GET /presence?user_ids=…` — the presence of the requested, visible Users.
///
/// Request order is preserved and each requested User appears at most once.
/// Returns `503` when the instance runs without a realtime subsystem, exactly like
/// the other domain routes, rather than making the route disappear.
async fn list_presence(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<PresenceQuery>,
) -> Result<Json<PresenceList>, ApiError> {
    let session = authenticate(&state, &headers).await?;
    let requested = parse_user_ids(query.user_ids.as_deref())?;

    let presences = state
        .realtime()?
        .presence_for(&session.user_id, &requested)
        .await
        .map_err(ApiError::from)?;

    Ok(Json(PresenceList { presences }))
}

/// Parse the comma-separated `user_ids` parameter into the ids to ask about.
///
/// Blank entries are dropped (a trailing comma is a client convenience, not an
/// error), duplicates collapse to the first occurrence so the answer stays in
/// request order, and the total is bounded. An absent or effectively empty list is
/// refused rather than answered with an empty list, because "ask about nobody" is
/// a client bug that would otherwise look like "everybody is invisible".
fn parse_user_ids(raw: Option<&str>) -> Result<Vec<String>, ApiError> {
    let raw = raw.ok_or_else(|| {
        validation(
            "user_ids",
            FieldErrorCode::Required,
            "请提供要查询的用户标识",
        )
    })?;

    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for candidate in raw.split(',') {
        let id = candidate.trim();
        if id.is_empty() || !seen.insert(id.to_owned()) {
            continue;
        }
        ids.push(id.to_owned());
    }

    if ids.is_empty() {
        return Err(validation(
            "user_ids",
            FieldErrorCode::Required,
            "请提供要查询的用户标识",
        ));
    }

    if ids.len() > MAX_PRESENCE_QUERY {
        return Err(validation(
            "user_ids",
            FieldErrorCode::TooLong,
            &format!("一次最多查询 {MAX_PRESENCE_QUERY} 位用户"),
        ));
    }

    Ok(ids)
}

/// A `422` with one field-level reason.
fn validation(field: &str, code: FieldErrorCode, message: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::ValidationFailed,
        "请求参数无效",
        vec![FieldError {
            field: field.to_owned(),
            code,
            message: message.to_owned(),
        }],
    )
}

/// Resolve an authorization header to the caller, or fail with `401`.
async fn authenticate(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<jiuyue_auth::AuthenticatedSession, ApiError> {
    let token = bearer_token(headers)?;
    state
        .auth()?
        .authenticate(&token)
        .await
        .map_err(ApiError::from)
}

#[cfg(test)]
mod tests {
    use super::parse_user_ids;
    use jiuyue_contract::MAX_PRESENCE_QUERY;

    #[test]
    fn ids_are_split_trimmed_and_deduplicated_in_request_order() {
        let ids: Vec<String> = parse_user_ids(Some(" a, b ,a,,c ")).unwrap_or_default();

        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn an_absent_or_empty_list_is_refused() {
        assert!(parse_user_ids(None).is_err());
        assert!(parse_user_ids(Some("  ,  ")).is_err());
    }

    #[test]
    fn more_than_the_bound_is_refused_rather_than_truncated() {
        let wide: Vec<String> = (0..MAX_PRESENCE_QUERY)
            .map(|index| format!("u{index}"))
            .collect();
        assert!(
            parse_user_ids(Some(&wide.join(","))).is_ok(),
            "exactly the bound must parse"
        );

        let wider: Vec<String> = (0..=MAX_PRESENCE_QUERY)
            .map(|index| format!("u{index}"))
            .collect();
        assert!(
            parse_user_ids(Some(&wider.join(","))).is_err(),
            "one past the bound must be refused"
        );
    }
}
