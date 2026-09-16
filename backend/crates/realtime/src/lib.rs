//! jiuyue realtime gateway.
//!
//! This crate owns the WebSocket transport: the upgrade handshake configuration,
//! framing limits, per-connection sequence assignment, a bounded send path, the
//! connection-level replay buffer, and the [`ConnectionRegistry`] that fans an
//! event out to every live Device of a User. It is mounted by `jiuyue-server` as
//! one route and stays free of HTTP routing.
//!
//! Keeping realtime out of `jiuyue-server` (rather than a `ws.rs` module inside
//! it) matches the module split in the product spec — the realtime gateway is its
//! own domain — and lets the transport evolve independently of HTTP routing.
//!
//! # Module layout
//!
//! - `registry` — user to live connections, and the bounded fan-out path.
//! - `replay` — the bounded per-connection replay buffer.
//! - `connection` — one socket's lifecycle, sequencing and resume handshake.
//! - this module — the hub, the framing limits and the shared error vocabulary.
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
//!
//! # Reconnection and repair (the "wake up and re-sync" handshake)
//!
//! A half-open TCP connection looks identical to a healthy one without periodic
//! traffic, so the connection emits a `ServerEvent::Ping` every
//! [`RealtimeHub::heartbeat_interval`] and the client reconnects when one is
//! overdue. When a client (re)connects, or notices a hole in `s`, it sends
//! `ClientEvent::Resume` naming the highest `s` it consumed. The server answers
//! with `ServerEvent::Resync`, and the reason is one of:
//!
//! - `ResyncReason::Fresh` — a brand-new connection; nothing was missed here.
//! - `ResyncReason::Replayed` — the missed envelopes were still in this
//!   connection's bounded [`ReplayBuffer`] and were re-sent before the answer.
//! - `ResyncReason::Unavailable` — the buffer cannot fill the hole (a new
//!   connection, or an evicted position). The client repairs each Conversation it
//!   holds by pulling everything after its own **conversation** cursor over REST.
//!
//! Delivery is therefore **at-least-once**: a replay may re-send an envelope, so
//! clients must key on Message ID and apply duplicates harmlessly. That is
//! deliberate — exactly-once is not attempted, and duplicates are the price of
//! never losing a Message.
//!
//! # Per-Device sync cursors
//!
//! The handshake above repairs the **connection**; it does not remember what a
//! **Device** had consumed before the process or the laptop went away. That state
//! is a Sync Cursor (CONTEXT.md), scoped to the connection's [`Device`] and
//! persisted by `jiuyue-chat`:
//!
//! - On connect, when the Device has stored cursors, the connection pushes
//!   `ServerEvent::SyncState` right after the opening heartbeat. It carries
//!   **positions, never Messages**, so the frame is bounded by the Device's
//!   Conversation count; the missed Messages are pulled with the existing forward
//!   walk, one bounded page at a time.
//! - The client reports progress with `ClientEvent::SyncCursor`. The connection
//!   coalesces those reports in memory (one entry per Conversation, highest wins)
//!   and checkpoints them on the heartbeat, on teardown, and when the pending set
//!   crosses [`CURSOR_CHECKPOINT_BATCH`] — never one write per Message. A crash
//!   between checkpoints loses at most one heartbeat interval, which the Device
//!   re-fetches harmlessly.

#![forbid(unsafe_code)]

mod connection;
mod cursor;
mod registry;
mod replay;
mod session;

use std::sync::Arc;
use std::time::Duration;

use jiuyue_chat::ChatService;
use thiserror::Error;

pub use connection::serve_connection;
pub use registry::{ConnectionId, ConnectionRegistry};
pub use replay::{DEFAULT_REPLAY_CAPACITY, ReplayBuffer};

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
/// the per-client memory ceiling for undelivered fan-out. A full queue drops the
/// event, which is safe because the connection-level replay buffer and the
/// Conversation cursor repair both exist to recover it.
pub const CONTROL_QUEUE_CAPACITY: usize = 64;

/// Default interval between server-initiated heartbeats.
///
/// The opening heartbeat fires on connect; this is the period of the ones after
/// it. It is the wall-clock evidence a client uses to notice a half-open socket.
/// It doubles as the Sync Cursor checkpoint interval: a connection flushes the
/// cursors it has coalesced on every beat, so a process death loses at most one
/// interval of progress and the Device re-fetches that tail idempotently.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// How many distinct Conversation cursors one connection coalesces in memory
/// before it checkpoints them without waiting for the next heartbeat.
///
/// The coalescing is the write-amplification guard: a client reporting after every
/// Message still produces **one** database statement per flush, because the map is
/// keyed by Conversation and keeps only the highest position. This bound keeps
/// that map from growing without limit on a Device that touches many Conversations
/// between two heartbeats.
pub const CURSOR_CHECKPOINT_BATCH: usize = 64;

/// The authenticated Device a connection belongs to (CONTEXT.md: Device).
///
/// A Device is a `sessions` row — a logged-in client instance — and it is the
/// scope a **Sync Cursor** lives in (CONTEXT.md: 同步游标按 Device). The socket
/// needs both halves: `user_id` for fan-out to every Device of the account, and
/// `session_id` to persist and restore *this* Device's own position. Passing them
/// as one value keeps the connection entry point at three arguments and makes
/// "which Device is this" explicit rather than positional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// ULID of the authenticated User.
    pub user_id: String,
    /// ULID of the `sessions` row the access token proved.
    pub session_id: String,
}

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
/// domain's internals. It also carries the two tunables the socket loop reads —
/// the heartbeat period and the replay-buffer capacity — so tests can pin them
/// without touching global state.
pub struct RealtimeHub {
    registry: ConnectionRegistry,
    chat: Arc<ChatService>,
    heartbeat_interval: Duration,
    replay_capacity: usize,
}

impl RealtimeHub {
    /// Build a hub over the chat service, with the production defaults.
    pub fn new(chat: Arc<ChatService>) -> Self {
        Self {
            registry: ConnectionRegistry::new(),
            chat,
            heartbeat_interval: HEARTBEAT_INTERVAL,
            replay_capacity: DEFAULT_REPLAY_CAPACITY,
        }
    }

    /// Build a hub with an explicit heartbeat period and replay capacity.
    ///
    /// Exists so tests can exercise the heartbeat and buffer eviction with
    /// milliseconds instead of seconds, without weakening the production values.
    pub fn with_settings(
        chat: Arc<ChatService>,
        heartbeat_interval: Duration,
        replay_capacity: usize,
    ) -> Self {
        Self {
            registry: ConnectionRegistry::new(),
            chat,
            heartbeat_interval,
            replay_capacity,
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

    /// Period between server-initiated heartbeats.
    pub fn heartbeat_interval(&self) -> Duration {
        self.heartbeat_interval
    }

    /// How many recent envelopes each connection retains for replay.
    pub fn replay_capacity(&self) -> usize {
        self.replay_capacity
    }
}
