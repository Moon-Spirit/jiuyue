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
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use jiuyue_protocol::{
    ErrorCode, ErrorPayload, Frame, MsgAck, MsgNew, MsgSend, Payload, SyncReq, SyncRes,
    PROTOCOL_VERSION,
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
                }
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
        // Server-to-client types arriving client→server are protocol misuse.
        Payload::MsgAck(_) | Payload::MsgNew(_) | Payload::AuthTicketRes(_) | Payload::SyncRes(_) => {
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
}

enum SendRejection {
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

    // Encrypt at rest before opening the transaction (no DB time spent in crypto).
    let body_enc = state.cipher.encrypt(&send.body)?;

    let mut tx = state.pool.begin().await?;
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
    let fresh: Option<(Uuid, i64, OffsetDateTime)> = sqlx::query_as(
        "INSERT INTO messages (id, conversation_id, seq, sender_id, client_msg_id, key_id, body_enc) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
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
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((message_id, seq, sent_at)) = fresh {
        tx.commit().await?;
        return Ok(SendOutcome {
            message_id,
            conversation_id: send.conversation_id,
            seq,
            sent_at_rfc3339: sent_at.format(&Rfc3339)?,
            duplicate: false,
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
        }),
    };
    let wire = serialize_frame(&msg_new);
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

/// Per-cursor replay cap for one `sync.req`.
const SYNC_BATCH_LIMIT: i64 = 200;

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

        let rows: Vec<(Uuid, i64, i64, Uuid, OffsetDateTime, Vec<u8>)> = sqlx::query_as(
            "SELECT id, conversation_id, seq, sender_id, sent_at, body_enc FROM messages \
             WHERE conversation_id = $1 AND seq > $2 ORDER BY seq ASC LIMIT $3",
        )
        .bind(cursor.conversation_id)
        .bind(cursor.last_delivered_seq)
        .bind(SYNC_BATCH_LIMIT)
        .fetch_all(&state.pool)
        .await?;

        complete &= rows.len() < usize::try_from(SYNC_BATCH_LIMIT).unwrap_or(usize::MAX);
        for (message_id, conversation_id, seq, sender_id, sent_at, body_enc) in rows {
            // Decrypt at-rest bodies before they leave the server; a row we
            // cannot decrypt is an internal fault surfaced as a generic
            // error to the client (never plaintext, never a panic).
            let body = state.cipher.decrypt(&body_enc)?;
            messages.push(MsgNew {
                message_id,
                conversation_id,
                seq,
                sender_id,
                body,
                sent_at: sent_at.format(&Rfc3339)?,
            });
        }
    }

    messages.sort_by_key(|m| (m.conversation_id, m.seq));
    Ok(SyncRes { messages, complete })
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
