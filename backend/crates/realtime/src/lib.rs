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
//!
//! # Presence and last-seen
//!
//! A **User** is online while any of their Devices is connected, and offline only
//! when the last one goes — Presence is per User, where a Sync Cursor is per
//! Device (CONTEXT.md). The [`ConnectionRegistry`] counts the Devices, so
//! [`ConnectionRegistry::register`] and [`ConnectionRegistry::unregister`] report
//! the first and the last connection as a [`PresenceChange`]; the hub turns that
//! into a `ServerEvent::Presence` for the Participants of a shared Conversation
//! and nothing else. Reachability itself lives in this process's memory, so a
//! restart empties it and no User can be left stuck online; the last-seen instant
//! is the durable half, checkpointed into `user_presence` by
//! [`LastSeenStore`] under the same batched discipline as the cursors above.
//!
//! # Typing Indicator
//!
//! A Typing Indicator is the protocol's cheapest event and therefore its easiest
//! amplifier, so the **throttle is the feature**: the hub coalesces every signal
//! for one (Conversation, Participant) into at most one fan-out per
//! [`TypingLimits::throttle`], and every indicator expires after
//! [`TypingLimits::ttl`] whether or not a stop arrives. See [`typing`] for the
//! arithmetic and why the state is ephemeral by construction rather than by
//! convention. The fan-out audience is the *other* Participants of the one
//! Conversation — never the sender's whole social graph, never the sender's own
//! Devices — resolved through the chat domain like every other recipient set.
//!
//! A connection that goes silent without closing — a severed network, a killed
//! client with no FIN — is swept once its client heartbeat is overdue: see
//! `ConnectionLiveness` and [`RealtimeHub::dead_connection_timeout`]. A client
//! that never sends a heartbeat is never swept, which is what keeps the sweep
//! additive for a client written against the older contract.

#![forbid(unsafe_code)]

mod connection;
mod cursor;
mod liveness;
mod presence;
mod registry;
mod replay;
mod session;
mod typing;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jiuyue_chat::ChatService;
use sqlx::PgPool;
use thiserror::Error;

pub use connection::serve_connection;
pub use presence::LastSeenStore;
pub use registry::{ConnectionId, ConnectionRegistry, PresenceChange};
pub use replay::{DEFAULT_REPLAY_CAPACITY, ReplayBuffer};
pub use typing::TypingLimits;

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

/// How many Users one presence checkpoint coalesces before it writes without
/// waiting for the next heartbeat.
///
/// The same write-amplification guard as [`CURSOR_CHECKPOINT_BATCH`], on a set
/// that is process-wide rather than per connection: presence changes are one per
/// User transition, so this bounds the pending set on a busy node while a normal
/// heartbeat flush (one statement for every online User) stays the common path.
pub const PRESENCE_CHECKPOINT_BATCH: usize = 512;

/// How many consecutive client heartbeats a silent connection may miss before the
/// server closes it.
///
/// The heartbeat is every [`HEARTBEAT_INTERVAL`], so **a dead connection can hold
/// a User online for at most three beats — 90 s at the production 30 s period**,
/// and in practice at most four (the sweep runs on the heartbeat tick, so the
/// detection lands between the third and fourth). Three rather than one because a
/// single lost beat must not flap a live client (a GC pause, a burst, a brief
/// network hiccup), and it is deliberately close to the client's own 75 s
/// (`2.5 × 30 s`) staleness deadline in ADR-0013 — the two ends of the same link
/// should give up at roughly the same moment.
pub const DEAD_CONNECTION_BEATS: u32 = 3;

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

    /// The audience for a User's presence could not be resolved.
    ///
    /// Presence fan-out asks the chat domain who shares a Conversation with the
    /// User; a failure there degrades the *hint* — the connection itself is
    /// unaffected — but the reason must not be swallowed.
    #[error("could not resolve a presence audience")]
    Chat(#[from] jiuyue_chat::ChatError),

    /// The presence store could not be read or written.
    #[error("the presence store failed")]
    Presence(#[from] sqlx::Error),
}

/// Everything a live connection needs beyond its socket.
///
/// One hub per process: it owns the connection registry, the chat service and the
/// presence store, so a connection can persist a Message, fan it out and record a
/// User's reachability without reaching into either domain's internals. It also
/// carries the three tunables the socket loop reads — the heartbeat period, the
/// replay-buffer capacity and the dead-connection deadline — so tests can pin them
/// without touching global state.
pub struct RealtimeHub {
    registry: ConnectionRegistry,
    chat: Arc<ChatService>,
    presence: presence::PresenceState,
    typing: typing::TypingTracker,
    heartbeat_interval: Duration,
    replay_capacity: usize,
    dead_connection_timeout: Duration,
}

impl RealtimeHub {
    /// Build a hub over the chat service and the presence store, with the
    /// production defaults.
    pub fn new(chat: Arc<ChatService>, pool: PgPool) -> Self {
        Self::with_settings(chat, pool, HEARTBEAT_INTERVAL, DEFAULT_REPLAY_CAPACITY)
    }

    /// Build a hub with an explicit heartbeat period and replay capacity.
    ///
    /// Exists so tests can exercise the heartbeat, the buffer eviction and the
    /// presence sweep with milliseconds instead of seconds, without weakening the
    /// production values. The dead-connection deadline is derived from the
    /// heartbeat ([`DEAD_CONNECTION_BEATS`]), so shortening one shortens the other
    /// and a test never has to wait 90 s to watch a sweep.
    pub fn with_settings(
        chat: Arc<ChatService>,
        pool: PgPool,
        heartbeat_interval: Duration,
        replay_capacity: usize,
    ) -> Self {
        Self {
            registry: ConnectionRegistry::new(),
            chat,
            presence: presence::PresenceState::new(pool),
            typing: typing::TypingTracker::new(TypingLimits::production()),
            heartbeat_interval,
            replay_capacity,
            dead_connection_timeout: heartbeat_interval * DEAD_CONNECTION_BEATS,
        }
    }

    /// Override the Typing Indicator clocks.
    ///
    /// Exists so a test can watch the throttle and the expiry in milliseconds
    /// instead of tens of seconds, without weakening the production values.
    /// Consuming rather than mutating so a hub is fully configured before it is
    /// shared — the same reason the heartbeat and replay capacity are constructor
    /// arguments.
    pub fn with_typing_limits(mut self, limits: TypingLimits) -> Self {
        self.typing = typing::TypingTracker::new(limits);
        self
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

/// Current wall-clock time as milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> Result<i64, RealtimeError> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    i64::try_from(elapsed.as_millis()).map_err(|_| RealtimeError::ClockRange)
}
