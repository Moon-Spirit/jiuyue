//! M14a RTC call signaling: in-memory call registry + `rtc.signal` relay.
//!
//! Voice/video calls are pure WebRTC P2P: **media never touches the server**.
//! The server's only jobs are (1) authorizing that the sender is a member of
//! the target conversation, (2) keeping a transient roster of who is in which
//! call so it can answer `call_busy` and fan `roster`/`ended` events, and
//! (3) relaying opaque SDP/ICE frames between participants' devices.
//!
//! # Registry lifetime
//!
//! [`CallRegistry`] is process-global in-memory state (`AppState::calls`).
//! Entries are created on `invite`, mutated on `accept`/`join`/`leave`, and
//! **dropped** the moment a call ends (`ended`). There is no durable call
//! history. As a safety net a lazily-triggered sweep ([`STALE_CALL_MAX_AGE`])
//! reaps any entry whose last transition is older than 4 hours, so a call
//! whose peers vanished without a `hangup` can never wedge a conversation as
//! permanently `call_busy`. (The sweep runs at the top of every inbound
//! `rtc.signal`, so it is O(calls) on the signaling hot path — calls are
//! few and short-lived.)
//!
//! # Conversation scope
//!
//! Calls are allowed only in `direct` and `group` conversations. `secret`
//! (E2EE) conversations are rejected with `calls_unsupported_in_secret`:
//! WebRTC signaling metadata (participant ids, timing, SDP fingerprints)
//! would leak exactly the relationship metadata secret chats exist to hide.
//!
//! # Wire contract
//!
//! One new frame type, `rtc.signal` (see [`jiuyue_protocol::RtcSignalIn`] /
//! [`RtcSignalOut`]). The registry is authorization + roster only; SDP/ICE
//! payloads pass through byte-for-byte.

use crate::auth::extract::AuthUser;
use crate::state::AppState;
use axum::routing::get;
use axum::{Json, Router};
use jiuyue_protocol::{Frame, PROTOCOL_VERSION, Payload, RtcSignalIn, RtcSignalOut, UserRef};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// A call entry with no transition for this long is reaped by the lazy sweep.
/// 4 hours is far beyond any legitimate voice call and keeps a crashed peer's
/// stale `call_busy` from wedging a conversation forever.
pub const STALE_CALL_MAX_AGE: Duration = Duration::from_secs(4 * 60 * 60);

/// Default STUN server when `JIUYUE_STUN_URLS` is unset.
pub const DEFAULT_STUN_URL: &str = "stun:stun.l.google.com:19302";

// ---------------------------------------------------------------------------
// Call registry
// ---------------------------------------------------------------------------

/// One live call: which conversation it belongs to, who is in it, the
/// negotiated media flavor, and when the entry was created.
#[derive(Debug, Clone)]
pub struct CallState {
    pub conversation_id: i64,
    /// Current participants in join order (initiator first). Never duplicated.
    pub participants: Vec<Uuid>,
    /// `"audio"` (default) or `"audio_video"`; informational for the roster.
    pub media: String,
    pub created_at: Instant,
}

/// Process-global registry of live calls keyed by client-chosen `call_id`.
///
/// All operations acquire the internal [`Mutex`] internally and release it
/// before returning (they never await while holding it), so the type is safe
/// to share across async tasks behind [`std::sync::Arc`].
#[derive(Debug, Default)]
pub struct CallRegistry {
    calls: Mutex<HashMap<Uuid, CallState>>,
}

impl CallRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the map, tolerating a poisoned mutex (registry state is
    /// best-effort and re-derivable, so a panicked task must not wedge calls).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, CallState>> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reaps entries older than `max_age` relative to now; returns how many.
    pub fn sweep_stale(&self, max_age: Duration) -> usize {
        self.sweep_stale_at(Instant::now(), max_age)
    }

    /// [`Self::sweep_stale`] with an injected clock (unit-testable).
    pub fn sweep_stale_at(&self, now: Instant, max_age: Duration) -> usize {
        let mut calls = self.lock();
        let before = calls.len();
        calls.retain(|_, call| now.saturating_duration_since(call.created_at) < max_age);
        before - calls.len()
    }

    /// Atomically claims the conversation's call slot. Returns `false` (and
    /// inserts nothing) when the conversation already has an active call —
    /// the `call_busy` gate.
    pub fn try_start(
        &self,
        call_id: Uuid,
        conversation_id: i64,
        initiator: Uuid,
        media: String,
    ) -> bool {
        let mut calls = self.lock();
        if calls
            .values()
            .any(|call| call.conversation_id == conversation_id)
        {
            return false;
        }
        calls.insert(
            call_id,
            CallState {
                conversation_id,
                participants: vec![initiator],
                media,
                created_at: Instant::now(),
            },
        );
        true
    }

    /// Snapshot of one call (cloned — safe to hold across awaits).
    pub fn get(&self, call_id: Uuid) -> Option<CallState> {
        self.lock().get(&call_id).cloned()
    }

    /// Adds `user_id` (idempotent) and returns the resulting participant list,
    /// or `None` when the call no longer exists.
    pub fn add_participant(&self, call_id: Uuid, user_id: Uuid) -> Option<Vec<Uuid>> {
        let mut calls = self.lock();
        let call = calls.get_mut(&call_id)?;
        if !call.participants.contains(&user_id) {
            call.participants.push(user_id);
        }
        Some(call.participants.clone())
    }

    /// Removes `user_id` (if present) and returns the remaining participants,
    /// or `None` when the call no longer exists.
    pub fn remove_participant(&self, call_id: Uuid, user_id: Uuid) -> Option<Vec<Uuid>> {
        let mut calls = self.lock();
        let call = calls.get_mut(&call_id)?;
        call.participants.retain(|participant| *participant != user_id);
        Some(call.participants.clone())
    }

    /// Drops a call entry, returning its final state when it existed.
    pub fn remove(&self, call_id: Uuid) -> Option<CallState> {
        self.lock().remove(&call_id)
    }
}

// ---------------------------------------------------------------------------
// Rejection taxonomy (mapped to `error` frames by the WS dispatcher)
// ---------------------------------------------------------------------------

/// Why an `rtc.signal` frame was refused. The connection always stays open:
/// signaling mistakes are policy answers, not identity doubt.
#[derive(Debug)]
pub(crate) enum RtcRejection {
    /// 400 — non-member (`not_a_member`, no membership/conversation oracle),
    /// secret conversation (`calls_unsupported_in_secret`), or unknown kind.
    BadRequest(&'static str),
    /// 409 — the conversation already has an active call (`call_busy`).
    Conflict(&'static str),
    /// 404 — call or participant gone (`call_not_found`).
    NotFound(&'static str),
    /// 500 — unexpected internal fault.
    Internal(anyhow::Error),
}

impl From<sqlx::Error> for RtcRejection {
    fn from(err: sqlx::Error) -> Self {
        Self::Internal(err.into())
    }
}

// ---------------------------------------------------------------------------
// Inbound dispatch
// ---------------------------------------------------------------------------

/// Handles one inbound `rtc.signal` frame end to end: membership/kind gate,
/// registry transition, and relay of the resulting frame(s). Returns
/// `Err(RtcRejection)` when the sender should receive an `error` frame.
pub(crate) async fn handle_signal(
    state: &AppState,
    sender_id: Uuid,
    signal: RtcSignalOut,
) -> Result<(), RtcRejection> {
    // Lazy stale-call cleanup on the signaling hot path (see module docs).
    let _ = state.calls.sweep_stale(STALE_CALL_MAX_AGE);

    let conversation_id = signal.signal.conversation_id;
    let call_id = signal.signal.call_id;

    // Membership gate BEFORE any oracle: a non-member (or unknown
    // conversation) always gets the same `not_a_member`, never a
    // conversation-existence signal.
    let conversation_kind = match conversation_kind_for_member(state, conversation_id, sender_id)
        .await?
    {
        Some(kind) => kind,
        None => return Err(RtcRejection::BadRequest("not_a_member")),
    };
    if conversation_kind == "secret" {
        return Err(RtcRejection::BadRequest("calls_unsupported_in_secret"));
    }

    match signal.signal.kind.as_str() {
        "invite" => handle_invite(state, sender_id, conversation_id, call_id, signal).await,
        "accept" => handle_join(state, sender_id, conversation_id, call_id, false).await,
        "join" => handle_join(state, sender_id, conversation_id, call_id, true).await,
        "reject" => {
            handle_reject(state, sender_id, conversation_id, call_id, &conversation_kind, signal)
                .await
        }
        "leave" | "hangup" => {
            handle_leave(state, sender_id, conversation_id, call_id, &conversation_kind).await
        }
        "sdp" | "ice" => handle_direct(state, sender_id, call_id, signal).await,
        _ => Err(RtcRejection::BadRequest("unsupported_rtc_kind")),
    }
}

/// `invite`: claim the conversation's call slot and ring the other members.
async fn handle_invite(
    state: &AppState,
    sender_id: Uuid,
    conversation_id: i64,
    call_id: Uuid,
    signal: RtcSignalOut,
) -> Result<(), RtcRejection> {
    let media = signal
        .signal
        .media
        .clone()
        .unwrap_or_else(|| "audio".to_owned());
    if !state.calls.try_start(call_id, conversation_id, sender_id, media) {
        return Err(RtcRejection::Conflict("call_busy"));
    }
    let from = require_user_ref(state, sender_id).await?;
    let members = conversation_members(state, conversation_id).await?;
    let payload = Payload::RtcSignal(RtcSignalOut::reply(signal.signal, from, None));
    for user_id in members.into_iter().filter(|id| *id != sender_id) {
        relay(state, user_id, &payload);
    }
    Ok(())
}

/// `accept` (DM callee) / `join` (group member): add the sender and broadcast
/// the new roster. `join` additionally relays the join itself to the other
/// participants so they can start mesh negotiation.
async fn handle_join(
    state: &AppState,
    sender_id: Uuid,
    conversation_id: i64,
    call_id: Uuid,
    relay_join: bool,
) -> Result<(), RtcRejection> {
    state
        .calls
        .get(call_id)
        .filter(|call| call.conversation_id == conversation_id)
        .ok_or(RtcRejection::NotFound("call_not_found"))?;
    let participants = state
        .calls
        .add_participant(call_id, sender_id)
        .ok_or(RtcRejection::NotFound("call_not_found"))?;
    let from = require_user_ref(state, sender_id).await?;
    let roster = roster_refs(state, &participants).await?;
    let roster_payload = Payload::RtcSignal(RtcSignalOut {
        signal: signal_with(conversation_id, call_id, "roster"),
        from: Some(from.clone()),
        participants: Some(roster),
    });
    for user_id in &participants {
        relay(state, *user_id, &roster_payload);
    }

    if relay_join {
        let join_payload = Payload::RtcSignal(RtcSignalOut::reply(
            signal_with(conversation_id, call_id, "join"),
            from,
            None,
        ));
        for user_id in participants.iter().filter(|id| **id != sender_id) {
            relay(state, *user_id, &join_payload);
        }
    }
    Ok(())
}

/// `reject`: a DM callee declined the ring. Relays `reject` to the caller and,
/// for a DM (where reject is terminal), drops the call and sends `ended` with
/// the reason so the caller's UI can tear down and a fresh invite can start.
async fn handle_reject(
    state: &AppState,
    sender_id: Uuid,
    conversation_id: i64,
    call_id: Uuid,
    conversation_kind: &str,
    signal: RtcSignalOut,
) -> Result<(), RtcRejection> {
    let call = state
        .calls
        .get(call_id)
        .filter(|call| call.conversation_id == conversation_id)
        .ok_or(RtcRejection::NotFound("call_not_found"))?;
    let reason = signal
        .signal
        .reason
        .clone()
        .unwrap_or_else(|| "declined".to_owned());
    let from = require_user_ref(state, sender_id).await?;
    let recipients: Vec<Uuid> = call
        .participants
        .iter()
        .copied()
        .filter(|id| *id != sender_id)
        .collect();

    let reject_payload = Payload::RtcSignal(RtcSignalOut::reply(
        RtcSignalIn {
            reason: Some(reason.clone()),
            ..signal_with(conversation_id, call_id, "reject")
        },
        from.clone(),
        None,
    ));
    for user_id in &recipients {
        relay(state, *user_id, &reject_payload);
    }

    if conversation_kind == "direct" {
        state.calls.remove(call_id);
        let ended_payload = Payload::RtcSignal(RtcSignalOut {
            signal: RtcSignalIn {
                reason: Some(reason),
                ..signal_with(conversation_id, call_id, "ended")
            },
            from: Some(from),
            participants: Some(Vec::new()),
        });
        for user_id in recipients.into_iter().chain(std::iter::once(sender_id)) {
            relay(state, user_id, &ended_payload);
        }
    }
    Ok(())
}

/// `leave` / `hangup`: remove the sender. A DM that drops to one participant
/// (or a call that drops to zero anywhere) ends; otherwise the shrunk roster
/// is broadcast and the leave is relayed to the remaining participants.
async fn handle_leave(
    state: &AppState,
    sender_id: Uuid,
    conversation_id: i64,
    call_id: Uuid,
    conversation_kind: &str,
) -> Result<(), RtcRejection> {
    state
        .calls
        .get(call_id)
        .filter(|call| call.conversation_id == conversation_id)
        .ok_or(RtcRejection::NotFound("call_not_found"))?;
    let remaining = state
        .calls
        .remove_participant(call_id, sender_id)
        .ok_or(RtcRejection::NotFound("call_not_found"))?;
    let from = require_user_ref(state, sender_id).await?;

    let starts_new_invite = remaining.is_empty()
        || (conversation_kind == "direct" && remaining.len() <= 1);
    if starts_new_invite {
        state.calls.remove(call_id);
        let ended_payload = Payload::RtcSignal(RtcSignalOut {
            signal: signal_with(conversation_id, call_id, "ended"),
            from: Some(from),
            participants: Some(Vec::new()),
        });
        // Tell everyone who was in the call (the remaining peer plus the
        // leaver, whose other devices may still be listening).
        let mut recipients = remaining;
        if !recipients.contains(&sender_id) {
            recipients.push(sender_id);
        }
        for user_id in recipients {
            relay(state, user_id, &ended_payload);
        }
        return Ok(());
    }

    let roster = roster_refs(state, &remaining).await?;
    let roster_payload = Payload::RtcSignal(RtcSignalOut {
        signal: signal_with(conversation_id, call_id, "roster"),
        from: Some(from.clone()),
        participants: Some(roster),
    });
    for user_id in &remaining {
        relay(state, *user_id, &roster_payload);
    }
    let relay_payload = Payload::RtcSignal(RtcSignalOut::reply(
        signal_with(conversation_id, call_id, "leave"),
        from,
        None,
    ));
    for user_id in &remaining {
        relay(state, *user_id, &relay_payload);
    }
    Ok(())
}

/// `sdp` / `ice`: relay an opaque negotiation frame to exactly one other
/// participant's devices. Sender and target must both be in the call.
async fn handle_direct(
    state: &AppState,
    sender_id: Uuid,
    call_id: Uuid,
    signal: RtcSignalOut,
) -> Result<(), RtcRejection> {
    let conversation_id = signal.signal.conversation_id;
    let call = state
        .calls
        .get(call_id)
        .filter(|call| call.conversation_id == conversation_id)
        .ok_or(RtcRejection::NotFound("call_not_found"))?;
    if !call.participants.contains(&sender_id) {
        return Err(RtcRejection::NotFound("call_not_found"));
    }
    let to_user_id = signal
        .signal
        .to_user_id
        .filter(|target| call.participants.contains(target))
        .ok_or(RtcRejection::NotFound("call_not_found"))?;

    let from = require_user_ref(state, sender_id).await?;
    let payload = Payload::RtcSignal(RtcSignalOut {
        signal: signal.signal,
        from: Some(from),
        participants: None,
    });
    relay(state, to_user_id, &payload);
    Ok(())
}

/// Serializes and fire-and-forget relays one payload to every live device of
/// `to_user` (drop-lag registry policy — signaling is best-effort).
fn relay(state: &AppState, to_user: Uuid, payload: &Payload) {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: payload.clone(),
    };
    let wire = crate::ws::serialize_frame(&frame);
    let delivered = state.registry.deliver_to(to_user, &wire);
    tracing::debug!(%to_user, delivered, "rtc.signal relayed");
}

/// Builds an empty outbound signal of `kind`; callers fill the fields they
/// need (all optionals are null-absent on the wire).
fn signal_with(conversation_id: i64, call_id: Uuid, kind: &str) -> RtcSignalIn {
    RtcSignalIn {
        conversation_id,
        call_id,
        kind: kind.to_owned(),
        to_user_id: None,
        media: None,
        reason: None,
        sdp_type: None,
        sdp: None,
        candidate: None,
        sdp_mid: None,
        sdp_mline_index: None,
    }
}

// ---------------------------------------------------------------------------
// DB lookups
// ---------------------------------------------------------------------------

/// Returns the conversation `kind` when `user_id` is a member, else `None`.
/// The JOIN makes membership and existence indistinguishable (anti-probing).
async fn conversation_kind_for_member(
    state: &AppState,
    conversation_id: i64,
    user_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT c.kind FROM conversations c \
         JOIN conversation_members cm \
           ON cm.conversation_id = c.id AND cm.user_id = $2 \
         WHERE c.id = $1",
    )
    .bind(conversation_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
}

/// All member user ids of a conversation.
async fn conversation_members(
    state: &AppState,
    conversation_id: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT user_id FROM conversation_members WHERE conversation_id = $1")
        .bind(conversation_id)
        .fetch_all(&state.pool)
        .await
}

/// Resolves one user's public RTC identity; `None` when the user is gone.
async fn fetch_user_ref(state: &AppState, user_id: Uuid) -> Result<Option<UserRef>, sqlx::Error> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT username, display_name FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(row.map(|(username, display_name)| UserRef {
        user_id,
        display_name: crate::profile::effective_display_name(&display_name, &username),
        username,
    }))
}

/// [`fetch_user_ref`] promoted to a hard error (the sender must exist).
async fn require_user_ref(state: &AppState, user_id: Uuid) -> Result<UserRef, RtcRejection> {
    fetch_user_ref(state, user_id).await?.ok_or_else(|| {
        RtcRejection::Internal(anyhow::anyhow!("rtc participant {user_id} vanished"))
    })
}

/// Resolves a roster in the given participant order, skipping vanished users.
async fn roster_refs(state: &AppState, participants: &[Uuid]) -> Result<Vec<UserRef>, RtcRejection> {
    if participants.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(Uuid, String, String)> =
        sqlx::query_as("SELECT id, username, display_name FROM users WHERE id = ANY($1)")
            .bind(participants)
            .fetch_all(&state.pool)
            .await?;
    let by_id: HashMap<Uuid, UserRef> = rows
        .into_iter()
        .map(|(user_id, username, display_name)| {
            let display_name = crate::profile::effective_display_name(&display_name, &username);
            (
                user_id,
                UserRef {
                    user_id,
                    username,
                    display_name,
                },
            )
        })
        .collect();
    Ok(participants
        .iter()
        .filter_map(|user_id| by_id.get(user_id).cloned())
        .collect())
}

// ---------------------------------------------------------------------------
// ICE config (`GET /api/rtc/config`)
// ---------------------------------------------------------------------------

/// One entry of the WebRTC `iceServers` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Response body of `GET /api/rtc/config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RtcConfigResponse {
    pub ice_servers: Vec<IceServer>,
}

/// RTC routes nested under `/api/rtc` (Bearer-authenticated).
pub fn router() -> Router<AppState> {
    Router::new().route("/config", get(config))
}

/// `GET /api/rtc/config` — returns the ICE server list the client feeds to
/// `RTCPeerConnection`. STUN comes from `JIUYUE_STUN_URLS`; TURN (for peers
/// behind symmetric NAT) is added when `JIUYUE_TURN_URLS` is set, optionally
/// with `JIUYUE_TURN_USERNAME` / `JIUYUE_TURN_CREDENTIAL`.
pub async fn config(_user: AuthUser) -> Json<RtcConfigResponse> {
    Json(RtcConfigResponse {
        ice_servers: ice_servers_from_env(|name| std::env::var(name).ok()),
    })
}

/// Pure env-driven ICE builder (unit-testable): `getter` resolves a variable
/// name to its value, so tests inject a fake environment without touching
/// process-global env.
pub fn ice_servers_from_env(getter: impl Fn(&str) -> Option<String>) -> Vec<IceServer> {
    let stun_urls = split_urls(getter("JIUYUE_STUN_URLS").as_deref())
        .unwrap_or_else(|| vec![DEFAULT_STUN_URL.to_owned()]);
    let mut servers = vec![IceServer {
        urls: stun_urls,
        username: None,
        credential: None,
    }];
    if let Some(turn_urls) = split_urls(getter("JIUYUE_TURN_URLS").as_deref()) {
        servers.push(IceServer {
            urls: turn_urls,
            username: non_empty(getter("JIUYUE_TURN_USERNAME")),
            credential: non_empty(getter("JIUYUE_TURN_CREDENTIAL")),
        });
    }
    servers
}

/// Splits a comma-separated env value, trimming blanks; `None` when unset or
/// effectively empty.
fn split_urls(raw: Option<&str>) -> Option<Vec<String>> {
    let urls: Vec<String> = raw?
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    (!urls.is_empty()).then_some(urls)
}

/// Trims a value and collapses blank → `None`.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_env(
        pairs: &[(&str, &str)],
    ) -> impl Fn(&str) -> Option<String> + 'static {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn ice_config_defaults_to_google_stun_when_env_absent() {
        let servers = ice_servers_from_env(fake_env(&[]));
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, vec![DEFAULT_STUN_URL.to_owned()]);
        assert!(servers[0].username.is_none());
        assert!(servers[0].credential.is_none());
    }

    #[test]
    fn ice_config_reads_comma_separated_stun_override() {
        let servers = ice_servers_from_env(fake_env(&[(
            "JIUYUE_STUN_URLS",
            "stun:a.example:3478, stun:b.example:3478 ",
        )]));
        assert_eq!(
            servers[0].urls,
            vec!["stun:a.example:3478", "stun:b.example:3478"]
        );
        assert_eq!(servers.len(), 1, "no TURN without JIUYUE_TURN_URLS");
    }

    #[test]
    fn ice_config_emits_turn_entry_with_credentials_when_configured() {
        let servers = ice_servers_from_env(fake_env(&[
            ("JIUYUE_TURN_URLS", "turn:turn.example:3478"),
            ("JIUYUE_TURN_USERNAME", "alice"),
            ("JIUYUE_TURN_CREDENTIAL", "s3cret"),
        ]));
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].urls, vec!["turn:turn.example:3478"]);
        assert_eq!(servers[1].username.as_deref(), Some("alice"));
        assert_eq!(servers[1].credential.as_deref(), Some("s3cret"));
    }

    #[test]
    fn ice_config_blank_overrides_fall_back_to_defaults() {
        let servers = ice_servers_from_env(fake_env(&[
            ("JIUYUE_STUN_URLS", " , "),
            ("JIUYUE_TURN_URLS", "  "),
        ]));
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, vec![DEFAULT_STUN_URL.to_owned()]);
    }

    #[test]
    fn try_start_is_busy_until_the_call_is_removed() {
        let registry = CallRegistry::new();
        let conversation = 7;
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let initiator = Uuid::now_v7();

        assert!(registry.try_start(first, conversation, initiator, "audio".to_owned()));
        assert!(
            !registry.try_start(second, conversation, initiator, "audio".to_owned()),
            "a second call in the same conversation is busy"
        );
        assert!(registry.remove(first).is_some());
        assert!(
            registry.try_start(second, conversation, initiator, "audio".to_owned()),
            "cleanup must free the conversation"
        );
    }

    #[test]
    fn participant_ops_are_idempotent_and_ordered() {
        let registry = CallRegistry::new();
        let call_id = Uuid::now_v7();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert!(registry.try_start(call_id, 1, a, "audio".to_owned()));

        assert_eq!(
            registry.add_participant(call_id, b).as_deref(),
            Some([a, b].as_slice())
        );
        assert_eq!(
            registry.add_participant(call_id, b).as_deref(),
            Some([a, b].as_slice()),
            "duplicate join does not duplicate"
        );
        assert_eq!(
            registry.remove_participant(call_id, a).as_deref(),
            Some([b].as_slice())
        );
        assert_eq!(
            registry.remove_participant(call_id, a).as_deref(),
            Some([b].as_slice()),
            "removing a non-participant is a no-op"
        );
    }

    #[test]
    fn stale_sweep_reaps_only_entries_past_max_age() {
        let registry = CallRegistry::new();
        let fresh = Uuid::now_v7();
        let stale = Uuid::now_v7();
        let initiator = Uuid::now_v7();
        assert!(registry.try_start(fresh, 1, initiator, "audio".to_owned()));
        assert!(registry.try_start(stale, 2, initiator, "audio".to_owned()));

        // Backdate the stale entry without underflowing `Instant`.
        let now = Instant::now();
        {
            let mut calls = registry.lock();
            let call = calls.get_mut(&stale).expect("stale entry");
            call.created_at = now
                .checked_sub(STALE_CALL_MAX_AGE + Duration::from_secs(60))
                .unwrap_or(now - Duration::from_secs(1));
        }

        let reaped = registry.sweep_stale_at(now, STALE_CALL_MAX_AGE);
        assert_eq!(reaped, 1);
        assert!(registry.get(fresh).is_some());
        assert!(registry.get(stale).is_none());
    }
}
