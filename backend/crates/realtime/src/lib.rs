//! jiuyue realtime gateway.
//!
//! This crate owns the WebSocket transport only: the upgrade handshake, framing
//! limits, per-connection sequence assignment and a bounded send queue. It knows
//! nothing about application state or persistence, so the HTTP server can mount
//! it as one stateless route and stay thin.
//!
//! Keeping realtime out of `jiuyue-server` (rather than a `ws.rs` module inside
//! it) matches the module split in the product spec — the realtime gateway is its
//! own domain — and lets the transport evolve independently of HTTP routing.
//!
//! # Limits
//!
//! `tokio-tungstenite` defaults to 16 MiB frames and 64 MiB messages. On the
//! 2 vCPU / 2 GB production box a handful of connections could exhaust memory,
//! so both are pinned to deliberately small values ([`MAX_FRAME_SIZE`],
//! [`MAX_MESSAGE_SIZE`]). The send path is a bounded `mpsc` queue, so a slow
//! client applies backpressure instead of growing the buffer without limit.

#![forbid(unsafe_code)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use jiuyue_contract::{ClientEnvelope, ClientEvent, Ping, ServerEnvelope, ServerEvent};
use thiserror::Error;
use tokio::sync::mpsc;

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

/// Bound on the per-connection send queue, in envelopes.
///
/// The queue is the backpressure point: when it is full the producer waits
/// rather than buffering without limit.
pub const SEND_QUEUE_CAPACITY: usize = 64;

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

/// Axum handler for `GET /ws`.
///
/// Configures the connection limits before handing the socket to
/// [`run_connection`]. Axum performs the HTTP upgrade in a background task.
pub async fn ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.max_frame_size(MAX_FRAME_SIZE)
        .max_message_size(MAX_MESSAGE_SIZE)
        .write_buffer_size(MAX_FRAME_SIZE)
        .max_write_buffer_size(MAX_FRAME_SIZE * 2)
        .on_upgrade(serve)
}

/// Serve one connection, logging (not panicking) on failure.
async fn serve(socket: WebSocket) {
    if let Err(error) = run_connection(socket).await {
        tracing::debug!(%error, "realtime connection ended");
    }
}

/// Assign the connection sequence, push the opening heartbeat, then pump frames
/// until the socket closes.
async fn run_connection(socket: WebSocket) -> Result<(), RealtimeError> {
    let (mut sink, mut stream) = socket.split();
    let (sender, mut receiver) = mpsc::channel::<ServerEnvelope>(SEND_QUEUE_CAPACITY);

    // Writer task: drains the bounded queue onto the socket and closes the sink
    // when the connection ends. Holding the sink here leaves the read side free.
    let writer = tokio::spawn(async move {
        while let Some(envelope) = receiver.recv().await {
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

    let mut sequence = 0_u64;
    send_heartbeat(&sender, &mut sequence).await?;

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    // The first interval tick is immediate; the opening heartbeat already fired.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if send_heartbeat(&sender, &mut sequence).await.is_err() {
                    break;
                }
            }
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => handle_client_frame(text.as_str()),
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::debug!(%error, "realtime receive error");
                    break;
                }
            },
        }
    }

    // Dropping the sender ends the writer task; awaiting it lets the close frame
    // flush before the connection is torn down.
    drop(sender);
    let _ = writer.await;

    Ok(())
}

/// Increment the per-connection sequence and enqueue a heartbeat envelope.
async fn send_heartbeat(
    sender: &mpsc::Sender<ServerEnvelope>,
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

    sender
        .send(envelope)
        .await
        .map_err(|_| RealtimeError::Closed)
}

/// Handle one decoded client frame. Client events carry no server-side effect
/// yet, so an unknown or malformed frame is logged and ignored rather than
/// dropping the connection.
fn handle_client_frame(text: &str) {
    match serde_json::from_str::<ClientEnvelope>(text) {
        Ok(envelope) => match envelope.event() {
            ClientEvent::Ping(ping) => {
                tracing::trace!(seq = ping.seq, time_ms = ping.time_ms, "client heartbeat");
            }
        },
        Err(error) => tracing::debug!(%error, "ignoring an undecodable client frame"),
    }
}

/// Current wall-clock time as milliseconds since the Unix epoch.
fn now_ms() -> Result<i64, RealtimeError> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    i64::try_from(elapsed.as_millis()).map_err(|_| RealtimeError::ClockRange)
}
