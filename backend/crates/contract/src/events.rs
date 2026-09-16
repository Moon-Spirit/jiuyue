//! Events carried inside envelopes.
//!
//! Both directions use the same adjacently tagged representation
//! (`#[serde(tag = "t", content = "d")]`): the wire is `{"t": "<Variant>", "d": ...}`.
//! Keeping the tag and the payload in their own fields means a new variant is a
//! purely additive change — old clients see an unknown `t` and skip it.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::chat::{ConversationCreated, MessageAck, MessageRejected, NewMessage, SendMessage};
use crate::sync::{Resume, Resync};

/// Events the server pushes to a client.
///
/// Variants are additive. A client that does not recognise a `t` skips the event
/// rather than failing, which is what lets the server grow the vocabulary without
/// a protocol version bump (ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "t", content = "d")]
#[ts(export)]
pub enum ServerEvent {
    /// Connection-level heartbeat. Sent once when the connection opens and
    /// periodically afterwards.
    Ping(Ping),
    /// A Message this connection submitted has been stored.
    MessageAck(MessageAck),
    /// A Message was stored in a Conversation this connection participates in.
    /// Fanned out to every Participant, including the sender's other Devices.
    NewMessage(NewMessage),
    /// A Message this connection submitted was refused and was not stored.
    MessageRejected(MessageRejected),
    /// A Conversation this connection now participates in was created.
    ConversationCreated(ConversationCreated),
    /// The server's answer to [`ClientEvent::Resume`]: whether the connection's
    /// missed envelopes were replayed, or whether the client must repair
    /// Conversations from its cursors (ADR-0003).
    Resync(Resync),
}

/// Events a client sends to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "t", content = "d")]
#[ts(export)]
pub enum ClientEvent {
    /// Client-side heartbeat / latency probe.
    Ping(Ping),
    /// Post a text Message into a Conversation.
    SendMessage(SendMessage),
    /// Wake-up handshake after (re)connecting or noticing a gap in `s`.
    Resume(Resume),
}

/// Heartbeat payload shared by both directions.
///
/// It carries the originator's connection sequence and wall-clock time so a peer
/// can detect staleness, estimate clock skew and verify ordering from the event
/// alone, without consulting transport metadata. This mirrors the Mattermost
/// reliable-WebSocket ping event referenced by ADR-0003.
///
/// A **server** heartbeat also carries `connection_id`, the process-unique
/// identity of the connection that sent it. The client echoes that id back in its
/// [`crate::sync::Resume`] handshake, which is what lets the server tell "resume
/// within this connection" (replayable) apart from "resume a position from a
/// previous connection" (unprovable — repair Conversations). A **client**
/// heartbeat leaves it `null`, because a client does not name a connection the
/// server did not assign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Ping {
    /// Connection sequence this heartbeat was sent with.
    #[ts(type = "number")]
    pub seq: u64,
    /// Originator wall-clock time, milliseconds since the Unix epoch.
    #[ts(type = "number")]
    pub time_ms: i64,
    /// Identity of the connection this server heartbeat belongs to; `null` on a
    /// client heartbeat.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub connection_id: Option<u64>,
}
