//! jiuyue realtime gateway.
//!
//! This crate owns the WebSocket transport: the upgrade handshake configuration,
//! framing limits, per-connection sequence assignment, a bounded send path, and
//! the [`ConnectionRegistry`] that fans an event out to every live Device of a
//! User. It is mounted by `jiuyue-server` as one route and stays free of HTTP
//! routing.
//!
//! Keeping realtime out of `jiuyue-server` (rather than a `ws.rs` module inside
//! it) matches the module split in the product spec 鈥?the realtime gateway is its
//! own domain 鈥?and lets the transport evolve independently of HTTP routing.
//!
//! # Authentication
//!
//! A socket is bound to a User by the caller: `jiuyue-server` authenticates the
//! access token **before** upgrading and hands [`serve_connection`] the resolved
//! `user_id`. There is no path that serves an unauthenticated socket.
//!
//! # Limits
//!
//! `tokio-tungstenite` defaults to 16 MiB frames and 64 MiB messages. On the
//! 2 vCPU / 2 GB production box a handful of connections could exhaust memory, so
//! both are pinned to deliberately small values ([`MAX_FRAME_SIZE`],
//! [`MAX_MESSAGE_SIZE`]). Two bounded queues guard the send path: the control
//! queue the registry fans into, and the envelope queue the writer drains. A slow
//! client applies backpressure rather than growing memory without limit.
//!
//! # Two sequences, never confused
//!
//! `s` on the envelope is the **connection** sequence (gap detection and replay,
//! ADR-0003). A Message's `seq` is the **conversation** sequence, allocated by
//! `jiuyue-chat`. This module owns the former and merely transports the latter.

#![forbid(unsafe_code)]

mod registry;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use jiuyue_chat::ChatService;
use jiuyue_contract::{
    ClientEnvelope, ClientEvent, MessageAck, MessageRejected, NewMessage, Ping, ServerEnvelope,
    ServerEvent,
};
use thiserror::Error;
use tokio::sync::mpsc;

pub use registry::{ConnectionId, ConnectionRegistry};

/// Hard cap on a single WebSocket frame (64 KiB).
///
/// Every envelope the contract sends today is a small JSON object, so this is
/// generous while keeping a hostile client from allocating megabytes per frame.
pub const MAX_FRAME_SIZE: usize = 64 * 1024;

/// Hard cap on a reassembled WebSocket message (1 MiB).
///
/// Messages may be split across frames; the reassembled size is capped
/// independently so fragmentation cannot bypass the frame limit.
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

/// Bound on the per-connection outbound envelope queue, in envelopes.
///
/// This is the writer's backpressure point: when it is full the connection waits
/// to hand an envelope over rather than buffering without limit.
pub const SEND_QUEUE_CAPACITY: usize = 64;

/// Bound on the per-connection control queue, in events.
///
/// The registry fans out through this queue with `try_send` only, so its size is
/// the per-client memory ceiling for undelivered fan-out.
pub const CONTROL_QUEUE_CAPACITY: usize = 64;

/// Interval between server-initiated heartbeats.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Failures raised while serving a realtime connection.
#[derive(Debug, Error)]
pub enum RealtimeError {
    /// The system clock could not be read relative to the Unix epoch.
    #[error("failed to read the system clock")]
    Clock(#[from] std::time::SystemTimeError),

    /// The system clock does not fit the contract's millisecond timestamp.
    #[error("the system clock is outside the representable timestamp range")]
    ClockRange,

    /// The peer closed the connection while a send was pending.
    #[error("the realtime connection was closed")]
    Closed,
}

/// Everything a live connection needs beyond its socket.
///
/// One hub per process: it owns the connection registry and the chat service, so a
/// connection can persist a Message and fan it out without reaching into either
/// domain's internals.
pub struct RealtimeHub {
    registry: ConnectionRegistry,
    chat: Arc<ChatService>,
}

impl RealtimeHub {
    /// Build a hub over the chat service.
    pub fn new(chat: Arc<ChatService>) -> Self {
        Self {
            registry: ConnectionRegistry::new(),
            chat,
        }
    }

    /// The live-connection registry, for fan-out from outside the socket path.
    pub fn registry(&self) -> &ConnectionRegistry {
        &self.registry
    }

    /// The chat service this hub serves.
    pub fn chat(&self) -> &ChatService {
        &self.chat
    }
}

/// Serve one authenticated connection, logging (not panicking) on failure.
///
/// `user_id` must come from a verified access token; this function never sees the
/// token itself.
pub async fn serve_connection(socket: WebSocket, user_id: String, hub: Arc<RealtimeHub>) {
    if let Err(error) = run_connection(socket, user_id, hub).await {
        tracing::debug!(%error, "realtime connection ended");
    }
}

/// Assign the connection sequence, push the opening heartbeat, then pump frames
/// and fanned-out events until the socket closes.
async fn run_connection(
    socket: WebSocket,
    user_id: String,
    hub: Arc<RealtimeHub>,
) -> Result<(), RealtimeError> {
    let (mut sink, mut stream) = socket.split();
    let (envelope_tx, mut envelope_rx) = mpsc::channel::<ServerEnvelope>(SEND_QUEUE_CAPACITY);

    // Writer task: drains the bounded queue onto the socket and closes the sink
    // when the connection ends. Holding the sink here leaves the read side free.
    let writer = tokio::spawn(async move {
        while let Some(envelope) = envelope_rx.recv().await {
            let text = match serde_json::to_string(&envelope) {
                Ok(text) => text,
                Err(error) => {
                    tracing::error!(%error, "failed to encode a server envelope");
                    continue;
                }
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // The registry holds the only control sender: fan-out reaches this connection
    // through the same bounded queue as any other Device of this User.
    let (control_tx, mut control_rx) = mpsc::channel::<ServerEvent>(CONTROL_QUEUE_CAPACITY);
    let connection_id = hub.registry.register(user_id.clone(), control_tx).await;

    let mut sequence = 0_u64;
    enqueue_ping(&envelope_tx, &mut sequence).await?;

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    // The first interval tick is immediate; the opening heartbeat already fired.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if enqueue_ping(&envelope_tx, &mut sequence).await.is_err() {
                    break;
                }
            }
            incoming = control_rx.recv() => match incoming {
                Some(event) => {
                    if enqueue(&envelope_tx, &mut sequence, event).await.is_err() {
                        break;
                    }
                }
                // The registry dropped the sender: this connection is over.
                None => break,
            },
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    if handle_client_frame(
                        text.as_str(),
                        &user_id,
                        &hub,
                        &envelope_tx,
                        &mut sequence,
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::debug!(%error, "realtime receive error");
                    break;
                }
            },
        }
    }

    hub.registry.unregister(&user_id, connection_id).await;

    // Dropping the sender ends the writer task; awaiting it lets the close frame
    // flush before the connection is torn down.
    drop(envelope_tx);
    let _ = writer.await;

    Ok(())
}

/// Handle one decoded client frame.
///
/// An unparseable frame is logged and ignored rather than dropping the connection
/// 鈥?a client bug must not cost the user their session. A `SendMessage` is
/// persisted inline: a connection processes its own sends in order, and the
/// database call is bounded by the pool's acquire timeout, so one user's send
/// cannot stall another's.
async fn handle_client_frame(
    text: &str,
    user_id: &str,
    hub: &RealtimeHub,
    envelope_tx: &mpsc::Sender<ServerEnvelope>,
    sequence: &mut u64,
) -> Result<(), RealtimeError> {
    let envelope = match serde_json::from_str::<ClientEnvelope>(text) {
        Ok(envelope) => envelope,
        Err(error) => {
            tracing::debug!(%error, "ignoring an undecodable client frame");
            return Ok(());
        }
    };

    match envelope.event() {
        ClientEvent::Ping(ping) => {
            tracing::trace!(seq = ping.seq, time_ms = ping.time_ms, "client heartbeat");
        }
        ClientEvent::SendMessage(send) => {
            let reply = match hub.chat.send_message(user_id, send.clone()).await {
                Ok(sent) => {
                    // Fan out only when this call actually wrote the Message. A
                    // replayed Client Message ID (or the loser of a race between
                    // two identical sends) must be a no-op on the wire: everyone
                    // still gets exactly one NewMessage for the Message, and the
                    // sender alone still gets its ack 鈥?which may be the only
                    // reason it retried in the first place.
                    if sent.created {
                        hub.registry
                            .deliver(
                                &sent.participants,
                                &ServerEvent::NewMessage(NewMessage {
                                    message: sent.message.clone(),
                                }),
                            )
                            .await;
                    }

                    ServerEvent::MessageAck(MessageAck {
                        client_msg_id: send.client_msg_id.clone(),
                        message: sent.message,
                    })
                }
                Err(error) => {
                    tracing::debug!(%error, "rejected a message send");
                    ServerEvent::MessageRejected(MessageRejected {
                        client_msg_id: send.client_msg_id.clone(),
                        code: error.code(),
                        message: error.message().to_owned(),
                    })
                }
            };

            enqueue(envelope_tx, sequence, reply).await?;
        }
    }

    Ok(())
}

/// Increment the per-connection sequence and enqueue an event as an envelope.
async fn enqueue(
    envelope_tx: &mpsc::Sender<ServerEnvelope>,
    sequence: &mut u64,
    event: ServerEvent,
) -> Result<(), RealtimeError> {
    *sequence += 1;

    let envelope = ServerEnvelope::new(*sequence, now_ms()?, event);
    envelope_tx
        .send(envelope)
        .await
        .map_err(|_| RealtimeError::Closed)
}

/// Increment the per-connection sequence and enqueue a heartbeat envelope whose
/// payload carries the same sequence the envelope does.
async fn enqueue_ping(
    envelope_tx: &mpsc::Sender<ServerEnvelope>,
    sequence: &mut u64,
) -> Result<(), RealtimeError> {
    *sequence += 1;

    let time_ms = now_ms()?;
    let envelope = ServerEnvelope::new(
        *sequence,
        time_ms,
        ServerEvent::Ping(Ping {
            seq: *sequence,
            time_ms,
        }),
    );

    envelope_tx
        .send(envelope)
        .await
        .map_err(|_| RealtimeError::Closed)
}

/// Current wall-clock time as milliseconds since the Unix epoch.
fn now_ms() -> Result<i64, RealtimeError> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    i64::try_from(elapsed.as_millis()).map_err(|_| RealtimeError::ClockRange)
}
