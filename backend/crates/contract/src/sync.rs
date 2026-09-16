//! Connection-level resynchronisation: the "wake up and re-sync" handshake.
//!
//! ADR-0003 gives every connection a monotonic sequence (`s` on the envelope) so
//! a client can tell that it missed an envelope — a gap in `s` is detectable from
//! the frame alone. These two types make the *reaction* to that gap explicit on
//! the wire instead of an implicit client policy:
//!
//! - [`Resume`] is the client's wake-up handshake, sent once a socket opens (and
//!   again if a live socket notices a hole). It names the highest `s` the client
//!   actually consumed.
//! - [`Resync`] is the server's answer. It says whether the server could replay
//!   the missed range from its bounded buffer ([`ResyncReason::Replayed`]) or
//!   whether the client must repair at the Conversation layer instead
//!   ([`ResyncReason::Unavailable`]).
//!
//! This is a **connection**-level contract and carries no Message ordering: the
//! authority for what a Conversation contains is still the per-Conversation
//! Sequence Number, repaired over REST (see [`crate::chat::MessagePageQuery`]).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Client's wake-up handshake after (re)connecting or after noticing a gap.
///
/// `last_seq` is the highest per-connection sequence `s` the client has actually
/// consumed, or `0` when it has consumed none (a first connection).
/// `connection_id` is the [`crate::Ping::connection_id`] of the connection that
/// sequence belongs to, echoed back when the client knows it.
///
/// A server that still holds `last_seq + 1` in its bounded replay buffer **and**
/// recognises `connection_id` as its own answers with [`ResyncReason::Replayed`].
/// A position with no matching connection id (a fresh socket, a reconnect, or a
/// client that has not yet seen a heartbeat) is [`ResyncReason::Unavailable`]:
/// the client repairs each Conversation from its own cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Resume {
    /// Highest connection sequence the client consumed; `0` when none.
    #[ts(type = "number")]
    pub last_seq: u64,
    /// The connection `last_seq` belongs to, when the client knows it; `null` on a
    /// first connect or before the first heartbeat has been seen.
    #[ts(type = "number | null")]
    pub connection_id: Option<u64>,
}

/// Why the server considers a connection resynchronised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ResyncReason {
    /// A brand-new connection, or `last_seq == 0`: nothing was missed at the
    /// connection layer, so no Conversation repair is implied by this answer.
    Fresh,
    /// The requested range was still buffered and has been replayed ahead of this
    /// event; the client is caught up at the connection layer.
    Replayed,
    /// The position is no longer buffered (the connection is new, or the bounded
    /// buffer evicted it). The client must repair each Conversation it holds by
    /// pulling everything after its own cursor.
    Unavailable,
}

/// The server's explicit answer to [`Resume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Resync {
    /// What the server did with the requested position.
    pub reason: ResyncReason,
    /// How many envelopes were replayed before this event; `0` unless the reason
    /// is [`ResyncReason::Replayed`].
    #[ts(type = "number")]
    pub replayed: u64,
}

/// One Device's consumed position in one Conversation (CONTEXT.md: **Sync Cursor**).
///
/// The cursor is scoped to a **Device**, not to a connection and not to a User:
/// `sessions` is the Device, and this pair is `(session, conversation)`. It is the
/// Conversation's Sequence Number (ADR-0003) — the one ordering authority.
///
/// `last_seq` is a **safe promise**: nothing at or below it still needs sending. A
/// client must therefore never report a value with a hole below it, or a later
/// resume would skip that hole silently. Everything after it is what a returning
/// Device still has to pull, using the existing forward walk (`after` on
/// [`crate::MessagePageQuery`]) rather than a parallel path. The client advances it
/// as it applies Messages; the server coalesces those reports and checkpoints them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SyncCursor {
    /// ULID of the Conversation the position is in.
    pub conversation_id: String,
    /// Highest Sequence Number this Device has consumed in that Conversation.
    #[ts(type = "number")]
    pub last_seq: i64,
}

/// `ServerEvent::SyncState` payload: the Device's stored Sync Cursors.
///
/// Sent once per connection, immediately after the opening heartbeat, **only when
/// the Device has at least one stored cursor** — a Device with no history needs no
/// hint and keeps the pre-cursor behaviour (load the newest page). This is the
/// "resume/sync response" of the delivery protocol: it carries **positions, never
/// Messages**, so its size is bounded by the Device's Conversation count and a
/// long absence is always repaired in bounded pages over the forward walk. A
/// client that receives it repairs each listed Conversation forward from
/// [`SyncCursor::last_seq`], and must apply duplicates idempotently by Message ID:
/// at-least-once delivery means a repair may re-send a Message the Device already
/// holds (ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SyncState {
    /// Every non-empty cursor the Device has stored, most recently advanced first.
    pub cursors: Vec<SyncCursor>,
}
