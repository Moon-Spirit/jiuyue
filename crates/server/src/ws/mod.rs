//! Ticket-gated WebSocket endpoint: `GET /ws?ticket=<one-time>&platform=<str>`.
//!
//! Connection lifecycle:
//!
//! 1. **Upgrade auth** — the one-time ticket is consumed atomically (Redis
//!    GETDEL semantics) *before* the upgrade commits; invalid, expired or
//!    replayed tickets reject the HTTP upgrade with `401`.
//! 2. **Device registration** — every accepted connection inserts a `devices`
//!    row (platform from the query string, default `web`) and registers its
//!    outbound channel in the [`ConnRegistry`].
//! 3. **Version gate** — the first frame (and any later frame) whose envelope
//!    `v != 1` earns an `error { code: "bad_request" }` frame and an
//!    immediate close.
//! 4. **Frame loop** — `msg.send` runs the persist-then-ack pipeline
//!    (transactional seq allocation + idempotent insert, see
//!    [`process_msg_send`]); `sync.req` replays everything past the caller's
//!    per-conversation delivery cursors into one `sync.res` (see
//!    [`process_sync_req`]); unknown frame types answer `unknown_type`
//!    *without* closing (forward compat).
//! 5. **Heartbeat** — the server pings every [`HeartbeatConfig::ping_every`];
//!    a peer that sends nothing back within [`HeartbeatConfig::idle_timeout`]
//!    of a ping is evicted from the registry (ghost-connection cleanup).
//! 6. **Teardown** — the device is unregistered from the registry so fanout
//!    stops targeting it.

pub mod registry;

// Canonical paths for sibling modules (`state`, tests).
pub use registry::{ConnRegistry, FrameTx, OutboundFrame, OUTBOUND_CHANNEL_CAPACITY};

use crate::auth::ws_ticket;
use crate::error::AppError;
use crate::push::{PushEnvelope, PushKind, PushPlatform};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use jiuyue_protocol::{
    E2eeMsg, ErrorCode, ErrorPayload, Frame, MsgAck, MsgNew, MsgRecall, MsgRecalled, MsgSend,
    Payload, ProfileUpdated, ReadReceipt, ReadUpdate, SyncMessage, SyncReq, SyncRes, Typing,
    TypingState, PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

/// How long teardown waits for the writer to drain before abandoning it.
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Server-initiated heartbeat policy for one WebSocket connection.
///
/// The server pings every [`Self::ping_every`]; if the peer sends nothing
/// back (a pong *or any other frame*) within [`Self::idle_timeout`] of that
/// ping, the connection is presumed dead and evicted from the registry.
///
/// Both intervals are parsed **once** from `JIUYUE_WS_PING_SECS` /
/// `JIUYUE_WS_TIMEOUT_SECS` when the [`AppState`] is built (defaults 30/60),
/// so deployments tune them via env and tests shrink them by assigning the
/// public [`AppState::heartbeat`] field before building the router — no
/// process-global env mutation needed.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatConfig {
    pub ping_every: Duration,
    pub idle_timeout: Duration,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(60),
        }
    }
}

impl HeartbeatConfig {
    /// Reads the intervals from the environment; missing, non-numeric or
    /// zero values fall back to the 30s/60s defaults (a zero ping period
    /// would panic tokio's interval timer).
    pub fn from_env() -> Self {
        Self::from_env_values(
            std::env::var("JIUYUE_WS_PING_SECS").ok(),
            std::env::var("JIUYUE_WS_TIMEOUT_SECS").ok(),
        )
    }

    fn from_env_values(ping_secs: Option<String>, timeout_secs: Option<String>) -> Self {
        fn parse_secs(raw: Option<String>) -> Option<u64> {
            let secs = raw?.trim().parse::<u64>().ok()?;
            (secs > 0).then_some(secs)
        }
        let fallback = Self::default();
        Self {
            ping_every: parse_secs(ping_secs)
                .map_or(fallback.ping_every, Duration::from_secs),
            idle_timeout: parse_secs(timeout_secs)
                .map_or(fallback.idle_timeout, Duration::from_secs),
        }
    }
}

/// `GET /ws` — consumes the one-time ticket, then upgrades.
///
/// Any ticket failure (missing, unknown, expired, already consumed) collapses
/// into a plain `401` upgrade rejection; no socket is ever established for an
/// unauthenticated peer.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(ticket) = params.get("ticket").map(String::as_str) else {
        return AppError::InvalidCredentials.into_response();
    };
    let user_id = match ws_ticket::consume(&state, ticket).await {
        Ok(user_id) => user_id,
        // Invalid/expired/single-used tickets are all generic 401s.
        Err(_) => return AppError::InvalidCredentials.into_response(),
    };
    let platform = params
        .get("platform")
        .cloned()
        .unwrap_or_else(|| "web".to_owned());
    let device_id = match register_device(&state, user_id, &platform).await {
        Ok(device_id) => device_id,
        Err(err) => return AppError::internal(err).into_response(),
    };
    // M7 XP economy: the first WS connection of a UTC day completes the
    // "daily login" — grant the +20 bonus once per UTC day. Fire-and-forget
    // and best-effort: an XP outage must never block the connection upgrade.
    {
        let pool = state.pool.clone();
        tokio::spawn(async move {
            if let Err(err) = crate::profile::grant_daily_login_bonus(&pool, user_id).await {
                tracing::warn!(%user_id, error = %format!("{err:#}"), "daily login bonus failed");
            }
        });
    }
    tracing::info!(%user_id, %device_id, platform = %platform, "websocket connected");
    ws.on_upgrade(move |socket| run_connection(state, socket, user_id, device_id))
}

/// Inserts the per-connection device row (`last_seen_at` pinned to now).
///
/// Interpretation note: each accepted WS connection registers its own device
/// session row — the devices table has no natural key for a true upsert, and
/// per-connection rows keep the registry's `(user_id, device_id)` keys unique
/// even when one user connects several times on the same platform.
async fn register_device(
    state: &AppState,
    user_id: Uuid,
    platform: &str,
) -> anyhow::Result<Uuid> {
    let device_id = Uuid::now_v7();
    sqlx::query("INSERT INTO devices (id, user_id, platform, last_seen_at) VALUES ($1, $2, $3, now())")
        .bind(device_id)
        .bind(user_id)
        .bind(platform)
        .execute(&state.pool)
        .await?;
    Ok(device_id)
}

/// Whether a raw first frame carries envelope version 1.
fn envelope_version_is_supported(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| value.get("v")?.as_u64())
        .is_some_and(|v| v == u64::from(PROTOCOL_VERSION))
}

/// Serializes a server frame; our payloads are pure serde types, so failure
/// is an invariant breach that degrades to a logged placeholder, never a panic.
fn serialize_frame(frame: &Frame) -> OutboundFrame {
    Arc::new(serde_json::to_string(frame).unwrap_or_else(|err| {
        tracing::error!(%err, "frame serialization failed; substituting empty object");
        "{}".to_owned()
    }))
}

fn error_frame(code: ErrorCode, message: impl Into<String>) -> Frame {
    Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::Error(ErrorPayload {
            code,
            message: message.into(),
            retryable: false,
        }),
    }
}

/// Fire-and-forget relay for friend-system frames (M5): pushes one
/// server-to-client payload to every live device of `to_user` through the
/// connection registry (same drop-lag policy as typing).
///
/// These frames are NEVER persisted and NEVER replayed by `sync.req` —
/// authoritative friend state lives in the `/api/friends` REST surface, so
/// delivery is best-effort by design. Called from REST handlers after their
/// transaction commits; a fully offline recipient simply misses the live
/// notice and discovers the change on the next REST read.
pub(crate) fn relay_friend_payload(state: &AppState, to_user: Uuid, payload: Payload) {
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload,
    };
    let wire = serialize_frame(&frame);
    let delivered = state.registry.deliver_to(to_user, &wire);
    tracing::debug!(%to_user, delivered, "friend frame relayed");
}

/// Fire-and-forget `profile.updated` fan-out after a profile PATCH commits.
///
/// Recipients: every ONLINE user sharing at least one conversation with the
/// editor (conversation members minus self, deduplicated, registry-filtered).
/// Offline peers miss the live notice and catch the new avatar/name on their
/// next conversation listing / sync — the profile surface is REST-authoritative.
pub(crate) async fn relay_profile_updated(
    state: &AppState,
    editor: Uuid,
    display_name: &str,
    avatar: &str,
) {
    let Ok(partners) = sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT cm2.user_id
           FROM conversation_members cm1
           JOIN conversation_members cm2 ON cm2.conversation_id = cm1.conversation_id
          WHERE cm1.user_id = $1 AND cm2.user_id <> $1",
    )
    .bind(editor)
    .fetch_all(&state.pool)
    .await
    else {
        return;
    };
    if partners.is_empty() {
        return;
    }
    let frame = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::ProfileUpdated(ProfileUpdated {
            user_id: editor,
            display_name: display_name.to_owned(),
            avatar: avatar.to_owned(),
        }),
    };
    let wire = serialize_frame(&frame);
    for partner in partners {
        let delivered = state.registry.deliver_to(partner, &wire);
        tracing::debug!(editor = %editor, %partner, delivered, "profile.updated relayed");
    }
}

/// What the frame loop should do after handling one frame.
enum Flow {
    Continue,
    Close,
}

/// Post-upgrade session: a writer task drains the outbound queue (live
/// frames + heartbeat pings) while this task reads and dispatches inbound
/// frames, arming the liveness timer between pings.
async fn run_connection(state: AppState, socket: WebSocket, user_id: Uuid, device_id: Uuid) {
    let heartbeat = state.heartbeat;
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundFrame>(OUTBOUND_CHANNEL_CAPACITY);
    // Control lane for server-initiated pings: separate from the frame queue
    // so live traffic can never delay a liveness probe.
    let (ping_tx, mut ping_rx) = mpsc::channel::<()>(1);
    state.registry.register(user_id, device_id, out_tx.clone());

    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(frame) => {
                        if sink.send(Message::Text((*frame).clone().into())).await.is_err() {
                            break;
                        }
                    }
                    // All frame producers dropped: teardown began.
                    None => break,
                },
                maybe_ping = ping_rx.recv() => {
                    // Heartbeat probe due; the reader only queues these when
                    // it wants a liveness answer. A closed control lane means
                    // teardown began — stop writing.
                    if maybe_ping.is_none() {
                        break;
                    }
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                },
            }
        }
        // Channel closed: end the socket with a proper close frame.
        let _ = sink.send(Message::Close(None)).await;
    });

    let mut version_checked = false;
    // Armed when a ping goes out, cleared by ANY inbound frame; eviction
    // fires when it stays armed past `idle_timeout`.
    let mut awaiting_pong_since: Option<tokio::time::Instant> = None;
    let mut heartbeat_evicted = false;
    let mut heartbeat_timer = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat.ping_every,
        heartbeat.ping_every,
    );

    loop {
        tokio::select! {
            _ = heartbeat_timer.tick() => match awaiting_pong_since {
                Some(since) if since.elapsed() >= heartbeat.idle_timeout => {
                    tracing::info!(%user_id, %device_id, "websocket heartbeat timeout; evicting");
                    heartbeat_evicted = true;
                    break;
                }
                // Ping already outstanding and not yet timed out: hold the
                // line — re-pinging would push the deadline out forever.
                Some(_) => {}
                None => {
                    // try_send: one queued ping is as good as another; a
                    // failure means the writer (and thus the socket) is gone
                    // and the read half will notice on its own.
                    if ping_tx.try_send(()).is_ok() {
                        awaiting_pong_since = Some(tokio::time::Instant::now());
                    }
                }
            },
            msg = stream.next() => {
                let text = match msg {
                    None => break,
                    Some(Ok(msg)) => {
                        // Any inbound frame proves liveness, pong or not.
                        awaiting_pong_since = None;
                        match msg {
                            Message::Text(text) => text,
                            // Text-only protocol; control frames other than Close are ignored.
                            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => continue,
                            Message::Close(_) => break,
                        }
                    }
                    Some(Err(err)) => {
                        tracing::debug!(%user_id, %device_id, error = %err, "websocket read failed");
                        break;
                    }
                };

                if !version_checked {
                    version_checked = true;
                    if !envelope_version_is_supported(&text) {
                        let _ = out_tx
                            .send(serialize_frame(&error_frame(
                                ErrorCode::BadRequest,
                                "unsupported protocol version",
                            )))
                            .await;
                        break;
                    }
                }

                // Buffer through `Value` so `Frame::try_from` surfaces the typed
                // `FrameError` variants (serde_json's Deserialize would erase them).
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(value) => match Frame::try_from(value) {
                        Ok(frame) => {
                            if let Flow::Close = handle_frame(
                                &state,
                                &out_tx,
                                user_id,
                                device_id,
                                frame.payload,
                            )
                            .await
                            {
                                break;
                            }
                        }
                        Err(jiuyue_protocol::FrameError::UnsupportedVersion(version)) => {
                            let _ = out_tx
                                .send(serialize_frame(&error_frame(
                                    ErrorCode::BadRequest,
                                    format!("unsupported envelope version {version}"),
                                )))
                                .await;
                            break;
                        }
                        Err(err) => {
                            // Malformed payload of a known type: stay connected (lenient),
                            // unlike version mismatches which are fatal.
                            let _ = out_tx
                                .send(serialize_frame(&error_frame(
                                    ErrorCode::BadRequest,
                                    format!("malformed frame: {err}"),
                                )))
                                .await;
                        }
                    },
                    Err(err) => {
                        let _ = out_tx
                            .send(serialize_frame(&error_frame(
                                ErrorCode::BadRequest,
                                format!("frame is not valid JSON: {err}"),
                            )))
                            .await;
                    }
                }
            }
        }
    }

    state.registry.unregister(user_id, device_id);
    // Dropping `out_tx` lets the writer observe `out_rx == None` *after* it
    // has flushed every queued frame (FIFO), so a pending error frame is
    // always written before the close. `ping_tx` deliberately stays alive
    // until scope end: dropping it here too would make the writer's select
    // race between "drain remaining frames" and "control lane closed", and
    // losing that race would truncate queued frames ahead of the close.
    drop(out_tx);
    if heartbeat_evicted {
        spawn_touch_device_last_seen(&state, device_id);
    }
    let _ = tokio::time::timeout(WRITER_DRAIN_TIMEOUT, writer).await;
    tracing::info!(%user_id, %device_id, "websocket disconnected");
}

async fn handle_frame(
    state: &AppState,
    out_tx: &FrameTx,
    sender_id: Uuid,
    sender_device_id: Uuid,
    payload: Payload,
) -> Flow {
    match payload {
        Payload::MsgSend(send) => match process_msg_send(state, sender_id, &send).await {
            Ok(outcome) => {
                let ack = Frame {
                    v: PROTOCOL_VERSION,
                    payload: Payload::MsgAck(MsgAck {
                        client_msg_id: send.client_msg_id,
                        message_id: outcome.message_id,
                        seq: outcome.seq,
                        duplicate: outcome.duplicate,
                    }),
                };
                let _ = out_tx.send(serialize_frame(&ack)).await;
                if !outcome.duplicate {
                    fanout_msg_new(state, sender_id, sender_device_id, &send.body, &outcome).await;
                    // M7 XP economy: fresh sends earn +10 XP per full 100
                    // characters (daily cap 200; secret chats excluded inside
                    // the award fn). The character COUNT is the only thing
                    // that leaves this frame — never the body. Best-effort
                    // post-commit spawn: an XP outage must never fail the
                    // already-acked send path.
                    spawn_message_xp(state, sender_id, send.conversation_id, &send.body);
                }
                Flow::Continue
            }
            Err(SendRejection::BadRequest(reason)) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(ErrorCode::BadRequest, reason)))
                    .await;
                Flow::Continue
            }
            Err(SendRejection::Unauthorized(reason)) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(ErrorCode::Unauthorized, reason)))
                    .await;
                // Authorization failures kill the connection: the ticket
                // proved who you are, but you reached for someone else's
                // conversation — do not leave the socket open for probing.
                Flow::Close
            }
            Err(SendRejection::Internal(err)) => {
                tracing::error!(%sender_id, error = %format!("{err:#}"), "msg.send failed");
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Internal,
                        "internal error",
                    )))
                    .await;
                Flow::Continue
            }
        },
        // M3 secret chats: `e2ee.msg` reuses the msg.send persist-then-ack
        // pipeline shape (membership gate → seq tx → idempotent insert →
        // identical ACK) with TWO deliberate asymmetries vs plaintext sends:
        //
        // 1. AT REST: `body_enc` stores the RAW UTF-8 BYTES of the
        //    client-produced ciphertext string verbatim (`kind='e2ee'`,
        //    `key_id='e2ee:<message_type>'`) — the server holds no key
        //    material and MUST NOT be able to decrypt it.
        // 2. ON THE WIRE: peers receive the SAME `e2ee.msg` frame mirrored
        //    back rather than a `msg.new`, because msg.new's frozen shape
        //    carries plaintext fields the server cannot produce. `sync.req`
        //    replays stored rows as `SyncMessage::Encrypted(e2ee.msg)`
        //    entries for the same reason (recalled tombstones excepted —
        //    they render as empty-body msg.new tombstones like plaintext).
        Payload::E2eeMsg(send) => match process_e2ee_send(state, sender_id, &send).await {
            Ok(outcome) => {
                let ack = Frame {
                    v: PROTOCOL_VERSION,
                    payload: Payload::MsgAck(MsgAck {
                        client_msg_id: send.client_msg_id,
                        message_id: outcome.message_id,
                        seq: outcome.seq,
                        duplicate: outcome.duplicate,
                    }),
                };
                let _ = out_tx.send(serialize_frame(&ack)).await;
                if !outcome.duplicate {
                    fanout_e2ee(state, sender_id, sender_device_id, &send, &outcome).await;
                }
                Flow::Continue
            }
            Err(SendRejection::BadRequest(reason)) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(ErrorCode::BadRequest, reason)))
                    .await;
                Flow::Continue
            }
            Err(SendRejection::Unauthorized(reason)) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(ErrorCode::Unauthorized, reason)))
                    .await;
                // Same policy as msg.send: reaching for a foreign
                // conversation kills the connection.
                Flow::Close
            }
            Err(SendRejection::Internal(err)) => {
                tracing::error!(%sender_id, error = %format!("{err:#}"), "e2ee.msg failed");
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Internal,
                        "internal error",
                    )))
                    .await;
                Flow::Continue
            }
        },
        // The protocol crate maps unknown `t` values into Error/UnknownType —
        // honor that mapping by answering without closing (forward compat).
        Payload::Error(payload) => {
            let _ = out_tx
                .send(serialize_frame(&error_frame(
                    payload.code,
                    "frame type not accepted from client",
                )))
                .await;
            Flow::Continue
        }
        // Catch-up replay past the caller's delivery cursors (ticket 06).
        Payload::SyncReq(req) => match process_sync_req(state, sender_id, &req).await {
            Ok(res) => {
                let _ = out_tx
                    .send(serialize_frame(&Frame {
                        v: PROTOCOL_VERSION,
                        payload: Payload::SyncRes(res),
                    }))
                    .await;
                Flow::Continue
            }
            Err(err) => {
                tracing::error!(%sender_id, error = %format!("{err:#}"), "sync.req failed");
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Internal,
                        "internal error",
                    )))
                    .await;
                Flow::Continue
            }
        },
        // WS transport authenticates via the upgrade ticket; the in-band
        // handshake frames are reserved for future milestones.
        Payload::AuthTicketReq {} => {
            let _ = out_tx
                .send(serialize_frame(&error_frame(
                    ErrorCode::BadRequest,
                    "transport already authenticated via upgrade ticket",
                )))
                .await;
            Flow::Continue
        }
        // M2 read receipts: persist the reader's cursor, echo a receipt to
        // the OTHER members only (the reader already knows it read).
        Payload::ReadUpdate(req) => match process_read_update(state, sender_id, &req).await {
            Ok(Some(last_read_seq)) => {
                fanout_read_receipt(state, sender_id, req.conversation_id, last_read_seq).await;
                Flow::Continue
            }
            // Raced with membership removal: nothing to echo.
            Ok(None) => Flow::Continue,
            Err(MemberRejection::NotMember) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Unauthorized,
                        "sender is not a member of this conversation",
                    )))
                    .await;
                Flow::Close
            }
            Err(MemberRejection::Internal(err)) => {
                tracing::error!(%sender_id, error = %format!("{err:#}"), "read.update failed");
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Internal,
                        "internal error",
                    )))
                    .await;
                Flow::Continue
            }
        },
        // M2 typing: pure relay, never persisted. The server stamps the
        // authenticated sender into `user_id` — a client-supplied value is
        // never trusted.
        Payload::Typing(typing) => match relay_typing(state, sender_id, &typing).await {
            Ok(()) => Flow::Continue,
            Err(MemberRejection::NotMember) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Unauthorized,
                        "sender is not a member of this conversation",
                    )))
                    .await;
                Flow::Close
            }
            Err(MemberRejection::Internal(err)) => {
                tracing::error!(%sender_id, error = %format!("{err:#}"), "typing relay failed");
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Internal,
                        "internal error",
                    )))
                    .await;
                Flow::Continue
            }
        },
        // M2 recall: sender-only + time-windowed via domain RecallPolicy,
        // tombstone-guarded via MessageStateMachine; broadcast to everyone.
        Payload::MsgRecall(req) => match process_msg_recall(state, sender_id, &req).await {
            Ok(()) => {
                broadcast_msg_recalled(state, req.conversation_id, req.message_id).await;
                Flow::Continue
            }
            Err(RecallRejection::NotMember) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Unauthorized,
                        "sender is not a member of this conversation",
                    )))
                    .await;
                Flow::Close
            }
            // Action-level denial: refuse without closing — unlike a foreign
            // conversation probe, recalling someone else's message inside
            // your own conversation is a policy answer, not identity doubt.
            Err(RecallRejection::NotSender) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Unauthorized,
                        "only the sender may recall a message",
                    )))
                    .await;
                Flow::Continue
            }
            Err(RecallRejection::NotFound) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::NotFound,
                        "message not found in this conversation",
                    )))
                    .await;
                Flow::Continue
            }
            Err(RecallRejection::Conflict(reason)) => {
                let _ = out_tx
                    .send(serialize_frame(&error_frame(ErrorCode::Conflict, reason)))
                    .await;
                Flow::Continue
            }
            Err(RecallRejection::Internal(err)) => {
                tracing::error!(%sender_id, error = %format!("{err:#}"), "msg.recall failed");
                let _ = out_tx
                    .send(serialize_frame(&error_frame(
                        ErrorCode::Internal,
                        "internal error",
                    )))
                    .await;
                Flow::Continue
            }
        },
        // Server-to-client types arriving client→server are protocol misuse.
        Payload::MsgAck(_)
        | Payload::MsgNew(_)
        | Payload::AuthTicketRes(_)
        | Payload::SyncRes(_)
        | Payload::ReadReceipt(_)
        | Payload::MsgRecalled(_)
        | Payload::FriendRequested(_)
        | Payload::FriendAccepted(_)
        | Payload::ProfileUpdated(_) => {
            let _ = out_tx
                .send(serialize_frame(&error_frame(
                    ErrorCode::BadRequest,
                    "frame type is server-to-client",
                )))
                .await;
            Flow::Continue
        }
    }
}

/// Outcome of the persist-then-ack pipeline for one `msg.send`.
struct SendOutcome {
    message_id: Uuid,
    conversation_id: i64,
    seq: i64,
    sent_at_rfc3339: String,
    duplicate: bool,
    /// Resolved reply-quote metadata for the fresh insert (duplicates never
    /// refanout, so they carry none).
    reply: Option<ReplyMeta>,
}

/// Server-resolved quote metadata carried on `msg.new` so receivers can
/// render the quote without an extra fetch.
struct ReplyMeta {
    message_id: Uuid,
    sender_id: Uuid,
    /// First [`REPLY_PREVIEW_MAX_CHARS`] chars of the quoted body (plaintext).
    body_preview: Option<String>,
}

/// Reply quotes show at most this many plaintext characters.
const REPLY_PREVIEW_MAX_CHARS: usize = 80;

enum SendRejection {
    BadRequest(&'static str),
    Unauthorized(&'static str),
    Internal(anyhow::Error),
}

impl From<sqlx::Error> for SendRejection {
    fn from(err: sqlx::Error) -> Self {
        Self::Internal(err.into())
    }
}

impl From<anyhow::Error> for SendRejection {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}

impl From<time::error::Format> for SendRejection {
    fn from(err: time::error::Format) -> Self {
        Self::Internal(anyhow::anyhow!(err))
    }
}

/// Persist-then-ack pipeline for one `msg.send` frame.
///
/// One transaction:
///
/// 1. `UPDATE conversations SET last_seq = last_seq + 1 ... RETURNING` —
///    row-locked, strictly monotonic per-conversation seq (Telegram pts).
/// 2. `INSERT INTO messages ... ON CONFLICT (conversation_id, client_msg_id)
///    DO NOTHING RETURNING id, seq` — idempotency gate.
/// 3. Fresh insert → COMMIT → fresh ACK. Conflict → ROLLBACK (returns the
///    burned seq; gaps are legal but pointless here) → read the winner's
///    committed row → duplicate ACK carrying the original `(id, seq)`.
///
/// The ACK is only ever produced *after* the commit lands (persist-then-ack):
/// a client that saw an ACK will always find the row in the database.
async fn process_msg_send(
    state: &AppState,
    sender_id: Uuid,
    send: &MsgSend,
) -> Result<SendOutcome, SendRejection> {
    // Membership authorization before touching the sequence counter.
    // (`::int8` so the scalar decodes as i64 — a bare `1` is INT4.)
    let member: Option<i64> = sqlx::query_scalar(
        "SELECT 1::int8 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(send.conversation_id)
    .bind(sender_id)
    .fetch_optional(&state.pool)
    .await?;
    if member.is_none() {
        return Err(SendRejection::Unauthorized(
            "sender is not a member of this conversation",
        ));
    }

    // Reply target validation: must exist AND belong to the same
    // conversation. Resolved once here (inside the tx) and reused for the
    // quote metadata after a fresh insert.
    let mut tx = state.pool.begin().await?;
    let reply_target: Option<(Uuid, Uuid, Vec<u8>, Option<OffsetDateTime>)> =
        match send.reply_to {
            Some(reply_to) => {
                let row: Option<(Uuid, Uuid, Vec<u8>, Option<OffsetDateTime>)> =
                    sqlx::query_as(
                        "SELECT id, sender_id, body_enc, recalled_at FROM messages \
                         WHERE id = $1 AND conversation_id = $2",
                    )
                    .bind(reply_to)
                    .bind(send.conversation_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                if row.is_none() {
                    let _ = tx.rollback().await;
                    return Err(SendRejection::BadRequest(
                        "reply_to must reference an existing message in the same conversation",
                    ));
                }
                row
            }
            None => None,
        };

    // Encrypt at rest before the heavy transaction work (no DB time spent in crypto).
    let body_enc = state.cipher.encrypt(&send.body)?;
    let allocated_seq: Option<i64> =
        sqlx::query_scalar("UPDATE conversations SET last_seq = last_seq + 1 WHERE id = $1 RETURNING last_seq")
            .bind(send.conversation_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(seq) = allocated_seq else {
        // Conversation vanished between the membership check and now (or the
        // id never existed): same authorization answer either way.
        let _ = tx.rollback().await;
        return Err(SendRejection::Unauthorized("conversation does not exist"));
    };

    let candidate_id = Uuid::now_v7();
    // DECISION (forwarding): forward is a pure CLIENT-side compose of a new
    // msg.send whose body is prefixed "[转发] " — there is deliberately no
    // wire field on msg.send to set forwarded_from_username, so the column
    // is always NULL on this insert. The schema column and the null-absent
    // `forwarded_from_username` field on msg.new are reserved for a future
    // server-side attribution path.
    let forwarded_from_username: Option<String> = None;
    let fresh: Option<(Uuid, i64, OffsetDateTime)> = sqlx::query_as(
        "INSERT INTO messages (id, conversation_id, seq, sender_id, client_msg_id, key_id, body_enc, reply_to, forwarded_from_username) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (conversation_id, client_msg_id) DO NOTHING \
         RETURNING id, seq, sent_at",
    )
    .bind(candidate_id)
    .bind(send.conversation_id)
    .bind(seq)
    .bind(sender_id)
    .bind(send.client_msg_id)
    .bind(state.cipher.key_id())
    .bind(&body_enc)
    .bind(send.reply_to)
    .bind(&forwarded_from_username)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((message_id, seq, sent_at)) = fresh {
        // Resolve the quote metadata while the reply row is still in scope:
        // decrypt the target body and cut an 80-char plaintext preview. A
        // RECALLED target never contributes its content ("never sent
        // again") — only its id/sender ride along.
        let reply = reply_target.map(
            |(reply_id, reply_sender_id, reply_body_enc, recalled_at)| {
                let body_preview = if recalled_at.is_some() {
                    None
                } else {
                    match state.cipher.decrypt(&reply_body_enc) {
                        Ok(plaintext) => {
                            Some(truncate_chars(&plaintext, REPLY_PREVIEW_MAX_CHARS))
                        }
                        Err(err) => {
                            tracing::warn!(%reply_id, error = %err, "reply preview decrypt failed");
                            None
                        }
                    }
                };
                ReplyMeta {
                    message_id: reply_id,
                    sender_id: reply_sender_id,
                    body_preview,
                }
            },
        );
        tx.commit().await?;
        return Ok(SendOutcome {
            message_id,
            conversation_id: send.conversation_id,
            seq,
            sent_at_rfc3339: sent_at.format(&Rfc3339)?,
            duplicate: false,
            reply,
        });
    }

    // Duplicate delivery: give back the just-burned seq, then read the row
    // the winning transaction committed (it must exist — the conflict proved it).
    tx.rollback().await?;
    let (message_id, seq, sent_at): (Uuid, i64, OffsetDateTime) = sqlx::query_as(
        "SELECT id, seq, sent_at FROM messages WHERE conversation_id = $1 AND client_msg_id = $2",
    )
    .bind(send.conversation_id)
    .bind(send.client_msg_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(SendOutcome {
        message_id,
        conversation_id: send.conversation_id,
        seq,
        sent_at_rfc3339: sent_at.format(&Rfc3339)?,
        duplicate: true,
        reply: None,
    })
}

/// Pushes `msg.new` to every member's live devices and fires the
/// fire-and-forget Redis notify used later for multi-instance fanout.
///
/// Fanout set (ticket 06): **all** conversation members — including the
/// sender's *other* live devices (multi-device self-echo) — except the exact
/// sending `(user_id, device_id)`, which already got its authoritative
/// `msg.ack`. Every accepted delivery advances that user's persisted
/// `last_delivered_seq` cursor (fire-and-forget).
async fn fanout_msg_new(
    state: &AppState,
    sender_id: Uuid,
    sender_device_id: Uuid,
    plaintext_body: &str,
    outcome: &SendOutcome,
) {
    let members: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT user_id FROM conversation_members WHERE conversation_id = $1",
    )
    .bind(outcome.conversation_id)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                conversation_id = outcome.conversation_id,
                error = %err,
                "fanout member lookup failed"
            );
            Vec::new()
        }
    };

    let msg_new = Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgNew(MsgNew {
            message_id: outcome.message_id,
            conversation_id: outcome.conversation_id,
            seq: outcome.seq,
            sender_id,
            body: plaintext_body.to_owned(),
            sent_at: outcome.sent_at_rfc3339.clone(),
            reply_to_message_id: outcome.reply.as_ref().map(|r| r.message_id),
            reply_to_sender_id: outcome.reply.as_ref().map(|r| r.sender_id),
            reply_to_body_preview: outcome
                .reply
                .as_ref()
                .and_then(|r| r.body_preview.clone()),
            // Always None today (see the DECISION comment in process_msg_send).
            forwarded_from_username: None,
            recalled: false,
        }),
    };
    let wire = serialize_frame(&msg_new);
    // Offline members are resolved BEFORE the fanout loop consumes `members`:
    // a member with zero registry entries received nothing live and is the
    // offline-push candidate set (M4).
    let offline_members: Vec<Uuid> = members
        .iter()
        .copied()
        .filter(|user_id| *user_id != sender_id)
        .filter(|user_id| state.registry.user_device_count(*user_id) == 0)
        .collect();
    for user_id in members {
        let delivered = if user_id == sender_id {
            state
                .registry
                .deliver_to_excluding(user_id, sender_device_id, &wire)
        } else {
            state.registry.deliver_to(user_id, &wire)
        };
        tracing::debug!(%user_id, delivered, conversation_id = outcome.conversation_id, "fanout");
        if delivered > 0 {
            spawn_advance_delivered_cursor(state, outcome.conversation_id, user_id, outcome.seq);
        }
    }

    // M4 offline-push hook: one fire-and-forget envelope per offline device.
    // Plaintext preview is allowed here because this path only serves normal
    // chats — secret chats (`fanout_e2ee`) never reach this module.
    if !offline_members.is_empty() {
        spawn_dispatch_offline_push(
            state,
            outcome.conversation_id,
            plaintext_body,
            offline_members,
        );
    }

    // Fire-and-forget pub/sub notify: single-instance delivery rides the
    // registry above; this hook exists so a later multi-instance deployment
    // can subscribe per node. Payload is only the message id (pointer).
    // Spawned + fully error-logged: a Redis outage must never panic or slow
    // the send path.
    let mut conn = state.redis.clone();
    let channel = format!("conv:{}", outcome.conversation_id);
    let payload = outcome.message_id.to_string();
    tokio::spawn(async move {
        match redis::cmd("PUBLISH")
            .arg(&channel)
            .arg(&payload)
            .query_async::<i64>(&mut conn)
            .await
        {
            Ok(receivers) => tracing::debug!(channel = %channel, receivers, "redis publish"),
            Err(err) => tracing::debug!(channel = %channel, error = %err, "redis publish failed"),
        }
    });
}

/// Fire-and-forget offline-push dispatch (M4): resolves every device row of
/// the offline members that carries a non-null `push_token`, then delivers
/// one [`PushEnvelope`] per device through the region-aware
/// [`crate::push::PushService`]. Spawned + fully error-logged: a push outage
/// must never panic or slow the send path. `web` devices and rows without a
/// token are skipped by design.
fn spawn_dispatch_offline_push(
    state: &AppState,
    conversation_id: i64,
    plaintext_body: &str,
    offline_users: Vec<Uuid>,
) {
    let pool = state.pool.clone();
    let push = Arc::clone(&state.push);
    let preview = truncate_chars(plaintext_body, crate::push::PREVIEW_MAX_CHARS);
    tokio::spawn(async move {
        let rows: Vec<(Uuid, Uuid, String, String)> = match sqlx::query_as(
            "SELECT id, user_id, platform, push_token FROM devices \
             WHERE user_id = ANY($1) AND push_token IS NOT NULL",
        )
        .bind(&offline_users)
        .fetch_all(&pool)
        .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(
                    conversation_id,
                    error = %err,
                    "offline-push device lookup failed"
                );
                return;
            }
        };
        for (device_id, user_id, platform, push_token) in rows {
            // `web` and unrecognized platforms never receive pushes.
            let Some(platform) = PushPlatform::parse(&platform) else {
                continue;
            };
            let envelope = PushEnvelope::new(
                user_id,
                device_id,
                conversation_id,
                &preview,
                PushKind::Message,
            );
            if let Err(err) = push.deliver(platform, &push_token, envelope).await {
                tracing::warn!(
                    %user_id,
                    %device_id,
                    %platform,
                    conversation_id,
                    error = %err,
                    "offline push delivery failed"
                );
            }
        }
    });
}

/// Outcome of the persist-then-ack pipeline for one `e2ee.msg`.
struct E2eeSendOutcome {
    message_id: Uuid,
    conversation_id: i64,
    seq: i64,
    duplicate: bool,
}

/// Persist-then-ack pipeline for one `e2ee.msg` frame — mirrors
/// [`process_msg_send`] minus everything a ciphertext cannot have (no
/// server-side encryption, no reply-quote resolution). See the dispatch-site
/// comment in [`handle_frame`] for the at-rest/wire asymmetry decisions.
async fn process_e2ee_send(
    state: &AppState,
    sender_id: Uuid,
    send: &E2eeMsg,
) -> Result<E2eeSendOutcome, SendRejection> {
    // Membership authorization before touching the sequence counter
    // (`::int8` so the scalar decodes as i64 — same convention as msg.send).
    let member: Option<i64> = sqlx::query_scalar(
        "SELECT 1::int8 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(send.conversation_id)
    .bind(sender_id)
    .fetch_optional(&state.pool)
    .await?;
    if member.is_none() {
        return Err(SendRejection::Unauthorized(
            "sender is not a member of this conversation",
        ));
    }

    let mut tx = state.pool.begin().await?;
    let allocated_seq: Option<i64> =
        sqlx::query_scalar("UPDATE conversations SET last_seq = last_seq + 1 WHERE id = $1 RETURNING last_seq")
            .bind(send.conversation_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(seq) = allocated_seq else {
        let _ = tx.rollback().await;
        return Err(SendRejection::Unauthorized("conversation does not exist"));
    };

    let candidate_id = Uuid::now_v7();
    // Ciphertext-at-rest decision: store the ciphertext STRING BYTES
    // verbatim (valid UTF-8 by construction — it arrived as a JSON string).
    // The olm message type rides in `key_id` (`e2ee:<type>`), the only
    // metadata column that stays untouched by the verbatim-bytes rule.
    let body_enc = send.ciphertext.as_bytes().to_vec();
    let key_id = format!("e2ee:{}", send.message_type);
    let fresh: Option<(Uuid, i64)> = sqlx::query_as(
        "INSERT INTO messages (id, conversation_id, seq, sender_id, client_msg_id, key_id, body_enc, kind) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'e2ee') \
         ON CONFLICT (conversation_id, client_msg_id) DO NOTHING \
         RETURNING id, seq",
    )
    .bind(candidate_id)
    .bind(send.conversation_id)
    .bind(seq)
    .bind(sender_id)
    .bind(send.client_msg_id)
    .bind(&key_id)
    .bind(&body_enc)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((message_id, seq)) = fresh {
        tx.commit().await?;
        return Ok(E2eeSendOutcome {
            message_id,
            conversation_id: send.conversation_id,
            seq,
            duplicate: false,
        });
    }

    // Duplicate delivery: give back the just-burned seq, then read the row
    // the winning transaction committed (identical to the msg.send path).
    tx.rollback().await?;
    let (message_id, seq): (Uuid, i64) = sqlx::query_as(
        "SELECT id, seq FROM messages WHERE conversation_id = $1 AND client_msg_id = $2",
    )
    .bind(send.conversation_id)
    .bind(send.client_msg_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(E2eeSendOutcome {
        message_id,
        conversation_id: send.conversation_id,
        seq,
        duplicate: true,
    })
}

/// Pushes the mirrored `e2ee.msg` frame to every member's live devices with
/// the SAME fanout set as `msg.new` (all members incl. the sender's OTHER
/// devices, minus the exact sending device which already got its ACK),
/// advances delivered cursors and fires the Redis notify hook. The
/// ciphertext passes through byte-for-byte; the server never inspects it.
async fn fanout_e2ee(
    state: &AppState,
    sender_id: Uuid,
    sender_device_id: Uuid,
    send: &E2eeMsg,
    outcome: &E2eeSendOutcome,
) {
    let members: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT user_id FROM conversation_members WHERE conversation_id = $1",
    )
    .bind(outcome.conversation_id)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                conversation_id = outcome.conversation_id,
                error = %err,
                "e2ee fanout member lookup failed"
            );
            Vec::new()
        }
    };

    let frame = serialize_frame(&Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::E2eeMsg(E2eeMsg {
            conversation_id: outcome.conversation_id,
            client_msg_id: send.client_msg_id,
            ciphertext: send.ciphertext.clone(),
            message_type: send.message_type,
        }),
    });
    for user_id in members {
        let delivered = if user_id == sender_id {
            state
                .registry
                .deliver_to_excluding(user_id, sender_device_id, &frame)
        } else {
            state.registry.deliver_to(user_id, &frame)
        };
        tracing::debug!(
            %user_id,
            delivered,
            conversation_id = outcome.conversation_id,
            "e2ee fanout"
        );
        if delivered > 0 {
            spawn_advance_delivered_cursor(state, outcome.conversation_id, user_id, outcome.seq);
        }
    }

    // Fire-and-forget pub/sub notify on the shared `conv:{id}` channel —
    // pointer payload (message id) only, never key or ciphertext material.
    let mut conn = state.redis.clone();
    let channel = format!("conv:{}", outcome.conversation_id);
    let payload = outcome.message_id.to_string();
    tokio::spawn(async move {
        match redis::cmd("PUBLISH")
            .arg(&channel)
            .arg(&payload)
            .query_async::<i64>(&mut conn)
            .await
        {
            Ok(receivers) => tracing::debug!(channel = %channel, receivers, "redis publish"),
            Err(err) => tracing::debug!(channel = %channel, error = %err, "redis publish failed"),
        }
    });
}

/// Fire-and-forget delivery-cursor advance: once at least one of the user's
/// devices accepted the live frame, `last_delivered_seq` moves forward
/// monotonically (`GREATEST`). Failures are logged, never fatal — the cursor
/// is a recovery hint, and a stale one only means extra rows replayed by the
/// next `sync.req`.
fn spawn_advance_delivered_cursor(
    state: &AppState,
    conversation_id: i64,
    user_id: Uuid,
    seq: i64,
) {
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let result = sqlx::query(
            "UPDATE conversation_members \
             SET last_delivered_seq = GREATEST(last_delivered_seq, $3) \
             WHERE conversation_id = $1 AND user_id = $2",
        )
        .bind(conversation_id)
        .bind(user_id)
        .bind(seq)
        .execute(&pool)
        .await;
        if let Err(err) = result {
            tracing::warn!(
                %conversation_id,
                %user_id,
                seq,
                error = %err,
                "delivery cursor update failed"
            );
        }
    });
}

/// Best-effort `devices.last_seen_at` refresh when the heartbeat evicts a
/// silent connection; failures are logged and otherwise ignored.
fn spawn_touch_device_last_seen(state: &AppState, device_id: Uuid) {
    let pool = state.pool.clone();
    tokio::spawn(async move {
        if let Err(err) = sqlx::query("UPDATE devices SET last_seen_at = now() WHERE id = $1")
            .bind(device_id)
            .execute(&pool)
            .await
        {
            tracing::warn!(%device_id, error = %err, "device last_seen_at touch failed");
        }
    });
}

/// Fire-and-forget messaging-XP award (M7). Spawned AFTER the send committed
/// and acked, so any failure here degrades to a logged no-op — the send path
/// is never held hostage by the XP ledger. Only the character COUNT is passed
/// (never the body); secret-chat messages earn 0 inside the award fn.
fn spawn_message_xp(state: &AppState, sender_id: Uuid, conversation_id: i64, body: &str) {
    let pool = state.pool.clone();
    let char_count = body.chars().count();
    tokio::spawn(async move {
        let result =
            crate::profile::award_message_xp_for_chars(&pool, sender_id, conversation_id, char_count)
                .await;
        if let Err(err) = result {
            tracing::warn!(
                %sender_id,
                %conversation_id,
                error = %format!("{err:#}"),
                "message xp award failed"
            );
        }
    });
}

/// Cuts a plaintext string to at most `max` chars (char-boundary safe).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Rejection type for member-gated ephemeral frames (`read.update`, `typing`).
enum MemberRejection {
    NotMember,
    Internal(anyhow::Error),
}

impl From<sqlx::Error> for MemberRejection {
    fn from(err: sqlx::Error) -> Self {
        Self::Internal(err.into())
    }
}

/// Read-cursor pipeline for one `read.update`: persist
/// `last_read_seq = GREATEST(persisted, requested)` and report the effective
/// value so the receipt echo never regresses. Returns `None` when membership
/// vanished between the gate and the write (raced removal — nothing to echo).
async fn process_read_update(
    state: &AppState,
    reader_id: Uuid,
    req: &ReadUpdate,
) -> Result<Option<i64>, MemberRejection> {
    let member: Option<i64> = sqlx::query_scalar(
        "SELECT 1::int8 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(req.conversation_id)
    .bind(reader_id)
    .fetch_optional(&state.pool)
    .await?;
    if member.is_none() {
        return Err(MemberRejection::NotMember);
    }

    let effective: Option<i64> = sqlx::query_scalar(
        "UPDATE conversation_members \
         SET last_read_seq = GREATEST(last_read_seq, $3) \
         WHERE conversation_id = $1 AND user_id = $2 \
         RETURNING last_read_seq",
    )
    .bind(req.conversation_id)
    .bind(reader_id)
    .bind(req.last_read_seq)
    .fetch_optional(&state.pool)
    .await?;
    Ok(effective)
}

/// Pushes `read.receipt` to every OTHER member's live devices (never back to
/// the reader). No Redis hook here by design: read cursors are durable state
/// recovered through sync, unlike typing which is ephemeral.
async fn fanout_read_receipt(
    state: &AppState,
    reader_id: Uuid,
    conversation_id: i64,
    last_read_seq: i64,
) {
    let members: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT user_id FROM conversation_members WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(conversation_id, error = %err, "receipt member lookup failed");
            return;
        }
    };
    let frame = serialize_frame(&Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::ReadReceipt(ReadReceipt {
            conversation_id,
            user_id: reader_id,
            last_read_seq,
        }),
    });
    for user_id in members.into_iter().filter(|id| *id != reader_id) {
        let delivered = state.registry.deliver_to(user_id, &frame);
        tracing::debug!(%user_id, delivered, conversation_id, "read.receipt fanout");
    }
}

/// Typing relay: member-gated, registry-only delivery to OTHER members
/// (never echoed to the typer), plus a fire-and-forget Redis publish on
/// `conv:{id}` so a future multi-instance deployment can bridge relays.
/// Nothing is persisted anywhere — typing is strictly ephemeral.
async fn relay_typing(state: &AppState, sender_id: Uuid, typing: &Typing) -> Result<(), MemberRejection> {
    let member: Option<i64> = sqlx::query_scalar(
        "SELECT 1::int8 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(typing.conversation_id)
    .bind(sender_id)
    .fetch_optional(&state.pool)
    .await?;
    if member.is_none() {
        return Err(MemberRejection::NotMember);
    }

    let frame = serialize_frame(&Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::Typing(Typing {
            conversation_id: typing.conversation_id,
            state: typing.state,
            user_id: Some(sender_id),
        }),
    });

    let members: Vec<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM conversation_members WHERE conversation_id = $1",
    )
    .bind(typing.conversation_id)
    .fetch_all(&state.pool)
    .await?;
    for user_id in members.into_iter().filter(|id| *id != sender_id) {
        let delivered = state.registry.deliver_to(user_id, &frame);
        tracing::debug!(%user_id, delivered, conversation_id = typing.conversation_id, "typing relay");
    }

    // Fire-and-forget pub/sub hook (same channel convention as msg.new).
    // Payload is a small self-describing envelope; spawned + error-logged so
    // a Redis outage can never slow or break the relay.
    let mut conn = state.redis.clone();
    let channel = format!("conv:{}", typing.conversation_id);
    let payload = serde_json::json!({
        "kind": "typing",
        "user_id": sender_id,
        "state": match typing.state {
            TypingState::Start => "start",
            TypingState::Stop => "stop",
        },
    })
    .to_string();
    tokio::spawn(async move {
        match redis::cmd("PUBLISH")
            .arg(&channel)
            .arg(&payload)
            .query_async::<i64>(&mut conn)
            .await
        {
            Ok(receivers) => tracing::debug!(channel = %channel, receivers, "redis publish"),
            Err(err) => tracing::debug!(channel = %channel, error = %err, "redis publish failed"),
        }
    });
    Ok(())
}

/// Rejection taxonomy for one `msg.recall`.
enum RecallRejection {
    /// Requester is not in the conversation at all (connection closes).
    NotMember,
    /// Member, but not the author of the message (refused, socket stays).
    NotSender,
    /// No such message in this conversation.
    NotFound,
    /// Window expired or already recalled (tombstone is terminal).
    Conflict(&'static str),
    Internal(anyhow::Error),
}

impl From<sqlx::Error> for RecallRejection {
    fn from(err: sqlx::Error) -> Self {
        Self::Internal(err.into())
    }
}

/// Recall pipeline for one `msg.recall`, composing BOTH domain guards:
///
/// 1. [`jiuyue_domain::RecallPolicy`] — identity first ("only the sender"),
///    then the injected-clock window check (`now_utc()` vs `sent_at`,
///    default 120s inclusive).
/// 2. [`jiuyue_domain::MessageStateMachine`] — a tombstone is terminal, so
///    an already-recalled message rejects the second recall as an illegal
///    transition (mapped to `conflict`). Non-recalled messages are all
///    representable as `Sent` here because `Recall` is legal from
///    Sent/Delivered/Read alike.
///
/// The final UPDATE re-checks `(id, sender, recalled_at IS NULL)` so a race
/// between two concurrent recalls still lands exactly one tombstone; losing
/// that race surfaces as `conflict`.
async fn process_msg_recall(
    state: &AppState,
    requester_id: Uuid,
    req: &MsgRecall,
) -> Result<(), RecallRejection> {
    use jiuyue_domain::{MessageStateMachine, MessageStatus, RecallPolicy, TransitionEvent};

    let member: Option<i64> = sqlx::query_scalar(
        "SELECT 1::int8 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
    )
    .bind(req.conversation_id)
    .bind(requester_id)
    .fetch_optional(&state.pool)
    .await?;
    if member.is_none() {
        return Err(RecallRejection::NotMember);
    }

    let row: Option<(Uuid, OffsetDateTime, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT sender_id, sent_at, recalled_at FROM messages \
         WHERE id = $1 AND conversation_id = $2",
    )
    .bind(req.message_id)
    .bind(req.conversation_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((sender_id, sent_at, recalled_at)) = row else {
        return Err(RecallRejection::NotFound);
    };

    // Guard 1: policy (identity + window), clock injected at this instant.
    RecallPolicy::default()
        .can_recall(requester_id, sender_id, sent_at, OffsetDateTime::now_utc())
        .map_err(|err| match err {
            jiuyue_domain::RecallError::NotSender => RecallRejection::NotSender,
            jiuyue_domain::RecallError::WindowExpired => {
                RecallRejection::Conflict("recall window expired")
            }
        })?;

    // Guard 2: state machine (tombstone is terminal → double recall dies here).
    let mut machine = MessageStateMachine::from(if recalled_at.is_some() {
        MessageStatus::Recalled
    } else {
        MessageStatus::Sent
    });
    machine
        .transition(TransitionEvent::Recall)
        .map_err(|_| RecallRejection::Conflict("message already recalled"))?;

    let updated = sqlx::query(
        "UPDATE messages SET recalled_at = now() \
         WHERE id = $1 AND sender_id = $2 AND recalled_at IS NULL",
    )
    .bind(req.message_id)
    .bind(sender_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(RecallRejection::Conflict("message already recalled"));
    }
    tracing::info!(
        conversation_id = req.conversation_id,
        message_id = %req.message_id,
        requester_id = %requester_id,
        "message recalled"
    );
    Ok(())
}

/// Broadcasts `msg.recalled` to ALL members including the requester (the
/// broadcast doubles as the sender's confirmation; there is no separate ack
/// frame for recalls).
async fn broadcast_msg_recalled(state: &AppState, conversation_id: i64, message_id: Uuid) {
    let members: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT user_id FROM conversation_members WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(conversation_id, error = %err, "recalled member lookup failed");
            return;
        }
    };
    let frame = serialize_frame(&Frame {
        v: PROTOCOL_VERSION,
        payload: Payload::MsgRecalled(MsgRecalled {
            conversation_id,
            message_id,
        }),
    });
    for user_id in members {
        let delivered = state.registry.deliver_to(user_id, &frame);
        tracing::debug!(%user_id, delivered, conversation_id, "msg.recalled broadcast");
    }
}

/// Per-cursor replay cap for one `sync.req`.
const SYNC_BATCH_LIMIT: i64 = 200;

/// One tombstone-aware sync row: the message columns plus the LEFT-JOINed
/// reply-target columns (`r.sender_id`, `r.body_enc`, `r.recalled_at`) and
/// the trailing `m.client_msg_id` / `m.key_id` / `m.kind` triple used to
/// route secret-chat rows into `SyncMessage::Encrypted` entries.
type SyncMessageRow = (
    Uuid,
    i64,
    i64,
    Uuid,
    OffsetDateTime,
    Vec<u8>,
    Option<OffsetDateTime>,
    Option<Uuid>,
    Option<String>,
    Option<Uuid>,
    Option<Vec<u8>>,
    Option<OffsetDateTime>,
    Uuid,
    String,
    String,
);

const SYNC_MESSAGE_QUERY: &str = "SELECT m.id, m.conversation_id, m.seq, m.sender_id, m.sent_at, \
     m.body_enc, m.recalled_at, m.reply_to, m.forwarded_from_username, \
     r.sender_id, r.body_enc, r.recalled_at, \
     m.client_msg_id, m.key_id, m.kind \
     FROM messages m \
     LEFT JOIN messages r ON r.id = m.reply_to \
     WHERE m.conversation_id = $1 AND m.seq > $2 ORDER BY m.seq ASC LIMIT $3";

/// Catch-up pipeline for one `sync.req`: replay every message past each
/// cursor into ONE `sync.res`.
///
/// Wire-shape decision (documented): the frozen protocol type
/// [`SyncRes`] carries `conversation_id` on every message entry, so a single
/// aggregated response represents multiple conversations cleanly; entries
/// are ordered by `(conversation_id, seq)` across the whole request. Each
/// cursor fetches at most [`SYNC_BATCH_LIMIT`] rows (`seq > cursor`, oldest
/// first); `complete` is the AND over cursors of "fewer than the limit
/// matched", i.e. false only when some conversation has more backlog than
/// one batch can carry.
///
/// Authorization policy (deliberate): cursors pointing at conversations the
/// caller is not a member of are silently skipped — no rows, no error — so
/// `sync.req` is never an oracle for conversation existence or membership.
async fn process_sync_req(
    state: &AppState,
    user_id: Uuid,
    req: &SyncReq,
) -> anyhow::Result<SyncRes> {
    let mut messages = Vec::new();
    let mut complete = true;

    for cursor in &req.cursors {
        // Membership gate before touching message rows (`::int8` so the
        // scalar decodes as i64 — same convention as the send path).
        let member: Option<i64> = sqlx::query_scalar(
            "SELECT 1::int8 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2",
        )
        .bind(cursor.conversation_id)
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await?;
        if member.is_none() {
            continue; // silent skip: anti-probing (see doc comment)
        }

        // Tombstone-aware fetch: recalled rows keep body_enc at rest for
        // audit but it is NEVER decrypted or sent again — the wire entry
        // carries an empty body plus `recalled: true`. The lateral join
        // resolves reply-quote metadata in the same round trip; a RECALLED
        // quote target contributes no preview (content "never sent again").
        let rows: Vec<SyncMessageRow> = sqlx::query_as(SYNC_MESSAGE_QUERY)
        .bind(cursor.conversation_id)
        .bind(cursor.last_delivered_seq)
        .bind(SYNC_BATCH_LIMIT)
        .fetch_all(&state.pool)
        .await?;

        complete &= rows.len() < usize::try_from(SYNC_BATCH_LIMIT).unwrap_or(usize::MAX);
        for (
            message_id,
            conversation_id,
            seq,
            sender_id,
            sent_at,
            body_enc,
            recalled_at,
            reply_to,
            forwarded_from_username,
            reply_sender_id,
            reply_body_enc,
            reply_recalled_at,
            client_msg_id,
            key_id,
            kind,
        ) in rows
        {
            let recalled = recalled_at.is_some();
            let sent_at_rfc3339 = sent_at.format(&Rfc3339)?;

            // Secret-chat rows (kind='e2ee', see the e2ee.msg dispatch-site
            // decision) bypass decryption entirely — the server cannot. They
            // replay as `SyncMessage::Encrypted` entries carrying the stored
            // ciphertext VERBATIM; the olm message type is recovered from the
            // `key_id` marker (`e2ee:<type>`). Recalled tombstones seal their
            // content forever, so they render as empty-body msg.new
            // tombstones instead — consistent with plaintext recall.
            if kind == "e2ee" {
                let entry = if recalled {
                    SyncMessage::Plain(MsgNew {
                        message_id,
                        conversation_id,
                        seq,
                        sender_id,
                        body: String::new(),
                        sent_at: sent_at_rfc3339,
                        reply_to_message_id: reply_to,
                        reply_to_sender_id: None,
                        reply_to_body_preview: None,
                        forwarded_from_username,
                        recalled: true,
                    })
                } else {
                    SyncMessage::Encrypted(E2eeMsg {
                        conversation_id,
                        client_msg_id,
                        ciphertext: String::from_utf8_lossy(&body_enc).into_owned(),
                        message_type: e2ee_message_type_from_key_id(&key_id),
                    })
                };
                messages.push((conversation_id, seq, entry));
                continue;
            }

            // Decrypt at-rest bodies before they leave the server — except
            // tombstones, whose content stays sealed forever. A row we
            // cannot decrypt is an internal fault surfaced as a generic
            // error to the client (never plaintext, never a panic).
            let body = if recalled {
                String::new()
            } else {
                state.cipher.decrypt(&body_enc)?
            };
            let mut reply_to_sender_id = None;
            let mut reply_to_body_preview = None;
            if let (Some(_reply_id), Some(r_sender), Some(r_body), r_recalled) =
                (reply_to, reply_sender_id, reply_body_enc, reply_recalled_at)
            {
                reply_to_sender_id = Some(r_sender);
                if r_recalled.is_none()
                    && let Ok(plaintext) = state.cipher.decrypt(&r_body)
                {
                    reply_to_body_preview =
                        Some(truncate_chars(&plaintext, REPLY_PREVIEW_MAX_CHARS));
                }
            }
            messages.push((
                conversation_id,
                seq,
                SyncMessage::Plain(MsgNew {
                    message_id,
                    conversation_id,
                    seq,
                    sender_id,
                    body,
                    sent_at: sent_at_rfc3339,
                    // Quote metadata survives recall: it describes the QUOTED
                    // message, not the tombstone's own (sealed) content.
                    reply_to_message_id: reply_to,
                    reply_to_sender_id,
                    reply_to_body_preview,
                    forwarded_from_username,
                    recalled,
                }),
            ));
        }
    }

    messages.sort_by_key(|(conversation_id, seq, _)| (*conversation_id, *seq));
    Ok(SyncRes {
        messages: messages
            .into_iter()
            .map(|(_, _, entry)| entry)
            .collect(),
        complete,
    })
}

/// Recovers the olm message type stored in `key_id` (`e2ee:<type>`); falls
/// back to `1` (normal) on malformed markers so replays stay deliverable.
fn e2ee_message_type_from_key_id(key_id: &str) -> i32 {
    key_id
        .strip_prefix("e2ee:")
        .and_then(|raw| raw.parse::<i32>().ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod heartbeat_config_tests {
    use super::*;

    #[test]
    fn defaults_are_thirty_and_sixty_seconds() {
        let cfg = HeartbeatConfig::default();
        assert_eq!(cfg.ping_every, Duration::from_secs(30));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(60));

        let parsed = HeartbeatConfig::from_env_values(None, None);
        assert_eq!(parsed.ping_every, cfg.ping_every);
        assert_eq!(parsed.idle_timeout, cfg.idle_timeout);
    }

    #[test]
    fn valid_env_values_override_defaults() {
        let cfg = HeartbeatConfig::from_env_values(Some("1".into()), Some("2".into()));
        assert_eq!(cfg.ping_every, Duration::from_secs(1));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(2));

        let padded = HeartbeatConfig::from_env_values(Some(" 5 ".into()), Some("10".into()));
        assert_eq!(padded.ping_every, Duration::from_secs(5));
        assert_eq!(padded.idle_timeout, Duration::from_secs(10));
    }

    #[test]
    fn invalid_or_zero_env_values_fall_back_to_defaults() {
        for ping in ["abc", "0", "-3", ""] {
            let cfg = HeartbeatConfig::from_env_values(Some(ping.to_owned()), Some("45".into()));
            assert_eq!(cfg.ping_every, Duration::from_secs(30), "ping {ping:?} must fall back");
            assert_eq!(cfg.idle_timeout, Duration::from_secs(45));
        }
        for timeout in ["not-a-number", "0"] {
            let cfg = HeartbeatConfig::from_env_values(Some("7".into()), Some(timeout.to_owned()));
            assert_eq!(cfg.ping_every, Duration::from_secs(7));
            assert_eq!(
                cfg.idle_timeout,
                Duration::from_secs(60),
                "timeout {timeout:?} must fall back"
            );
        }
    }
}
