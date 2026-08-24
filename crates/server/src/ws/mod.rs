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
//!    [`process_msg_send`]); unknown frame types answer `unknown_type`
//!    *without* closing (forward compat); unimplemented known types
//!    (`sync.req`, ticket 06) answer a graceful error without closing.
//! 5. **Teardown** — the device is unregistered from the registry so fanout
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
    ErrorCode, ErrorPayload, Frame, MsgAck, MsgNew, MsgSend, Payload, PROTOCOL_VERSION,
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

/// Post-upgrade session: writer task drains the outbound queue while this
/// task reads and dispatches inbound frames.
async fn run_connection(state: AppState, socket: WebSocket, user_id: Uuid, device_id: Uuid) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<OutboundFrame>(OUTBOUND_CHANNEL_CAPACITY);
    state.registry.register(user_id, device_id, out_tx.clone());

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(Message::Text((*frame).clone().into())).await.is_err() {
                break;
            }
        }
        // Channel closed: end the socket with a proper close frame.
        let _ = sink.send(Message::Close(None)).await;
    });

    let mut version_checked = false;
    while let Some(msg) = stream.next().await {
        let text = match msg {
            Ok(Message::Text(text)) => text,
            // Text-only protocol; control frames other than Close are ignored.
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_)) => continue,
            Ok(Message::Close(_)) => break,
            Err(err) => {
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
                    if let Flow::Close =
                        handle_frame(&state, &out_tx, user_id, frame.payload).await
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

    state.registry.unregister(user_id, device_id);
    drop(out_tx);
    let _ = tokio::time::timeout(WRITER_DRAIN_TIMEOUT, writer).await;
    tracing::info!(%user_id, %device_id, "websocket disconnected");
}

async fn handle_frame(state: &AppState, out_tx: &FrameTx, sender_id: Uuid, payload: Payload) -> Flow {
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
                    fanout_msg_new(state, sender_id, &send.body, &outcome).await;
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
        // Known but unimplemented until ticket 06: graceful, non-fatal.
        Payload::SyncReq(_) => {
            let _ = out_tx
                .send(serialize_frame(&error_frame(
                    ErrorCode::BadRequest,
                    "sync.req is not implemented in this milestone",
                )))
                .await;
            Flow::Continue
        }
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

/// Pushes `msg.new` to every *other* member's live connections and fires the
/// fire-and-forget Redis notify used later for multi-instance fanout.
async fn fanout_msg_new(state: &AppState, sender_id: Uuid, plaintext_body: &str, outcome: &SendOutcome) {
    let others: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT user_id FROM conversation_members WHERE conversation_id = $1 AND user_id <> $2",
    )
    .bind(outcome.conversation_id)
    .bind(sender_id)
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
    for user_id in others {
        let delivered = state.registry.deliver_to(user_id, &wire);
        tracing::debug!(%user_id, delivered, conversation_id = outcome.conversation_id, "fanout");
    }

    // Fire-and-forget pub/sub notify: single-instance delivery rides the
    // registry above; this hook exists so a later multi-instance deployment
    // can subscribe per node. Payload is only the message id (pointer).
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
