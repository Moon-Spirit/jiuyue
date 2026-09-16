//! Chat wire types: Conversations and Messages, over both REST and the socket.
//!
//! These types are the single source of truth for the chat contract, exactly as
//! [`crate::auth`] is for identity and [`crate::envelope`] is for the socket frame.
//! `jiuyue-chat`, `jiuyue-realtime` and `jiuyue-server` produce and consume them,
//! and `ts-rs` exports them to `frontend/src/generated`, so the frontend never
//! re-declares a server shape.
//!
//! Shape rules the rest of the stack relies on:
//!
//! - Numbers that are conceptually 64-bit (`seq`, `*_at_ms`) are annotated
//!   `#[ts(type = "number")]`. Without it `ts-rs` emits `bigint`, which
//!   `JSON.parse` never produces.
//! - [`MessageView::client_msg_id`] is echoed back on both the acknowledgement
//!   and the new-message event. It is the sender's idempotency key, and it is what
//!   lets the sending client reconcile the optimistic bubble it rendered before
//!   the server had even seen the message.
//! - The socket variants are additive: a new [`crate::ServerEvent`] variant is an
//!   older client's unknown tag, which it skips (ADR-0003). Greeting a peer with a
//!   new Conversation is therefore a new event rather than a change to an old one.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::auth::ErrorCode;
use crate::group::GroupSummary;
use crate::read::ReadReceipt;

/// Hard cap on a Message body, in Unicode scalar values.
///
/// The server enforces it and the client mirrors it so the limit is felt before a
/// round trip. It matches the `messages_body_length` check in the schema.
pub const MAX_MESSAGE_BODY_CHARS: usize = 4000;

/// Hard cap on a Client Message ID, in bytes.
pub const MAX_CLIENT_MSG_ID_BYTES: usize = 128;

/// Default number of Messages in a history page when the request omits `limit`.
pub const DEFAULT_MESSAGE_PAGE_SIZE: i64 = 50;

/// Hard cap on a history page.
///
/// A client must not be able to ask for a whole Conversation in one request: an
/// unbounded page is a memory bomb on the 2 vCPU / 2 GB production box. A
/// `limit` above this is clamped, never honoured.
pub const MAX_MESSAGE_PAGE_SIZE: i64 = 100;

/// Kind of Conversation (CONTEXT.md: Conversation).
///
/// Serialised lowercase because the value is stored verbatim in
/// `conversations.kind`; `group` is part of the vocabulary from day one even
/// though only `direct` can be created until the group ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum ConversationKind {
    /// Exactly two Participants.
    Direct,
    /// Three or more Participants, with Roles.
    Group,
}

/// The publicly visible part of the *other* Participant of a Direct Conversation.
///
/// Deliberately not [`crate::UserProfile`]: a Conversation peer must not leak the
/// email address, which the profile carries for the account owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PeerSummary {
    /// ULID of the other User.
    pub id: String,
    /// `@handle` of the other User.
    pub username: String,
    /// Display name of the other User.
    pub display_name: String,
    /// Avatar URL of the other User, when one has been set.
    pub avatar_url: Option<String>,
}

/// One Conversation as the conversation list renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConversationSummary {
    /// ULID of the Conversation.
    pub id: String,
    /// Direct or Group.
    pub kind: ConversationKind,
    /// The other Participant of a Direct Conversation; absent for a Group, which
    /// names no single peer.
    pub peer: Option<PeerSummary>,
    /// The Group-specific part of the summary (title, member count and the
    /// caller's own Role); absent for a Direct Conversation.
    ///
    /// Optional on the wire as well as in Rust so a Group field can be added
    /// without rejecting a Direct-shaped payload from an older peer.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub group: Option<GroupSummary>,
    /// The caller's Unread Count (CONTEXT.md: 未读数) in this Conversation.
    ///
    /// Computed server-side from the caller's private Read Marker and maintained
    /// incrementally, never by scanning `messages` here: the Conversation list is
    /// the hottest read path. It is the **caller's own** count — another
    /// Participant's is never exposed.
    #[ts(type = "number")]
    pub unread_count: i64,
    /// Creation time, milliseconds since the Unix epoch.
    #[ts(type = "number")]
    pub created_at_ms: i64,
}

/// `POST /conversations/direct` body.
///
/// The peer is named by `@handle` rather than by ULID: a user knows a handle, and
/// the contacts module that would give them an id picker is a later ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateDirectConversationRequest {
    /// `@handle` of the other Participant, matched case-insensitively.
    pub peer_username: String,
}

/// One Message as the message list renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageView {
    /// ULID of the Message (the global Message ID).
    pub id: String,
    /// ULID of the Conversation the Message belongs to.
    pub conversation_id: String,
    /// Per-Conversation Sequence Number; the ordering authority (ADR-0003).
    #[ts(type = "number")]
    pub seq: i64,
    /// ULID of the sending User.
    pub sender_id: String,
    /// The sender's Client Message ID, echoed so an optimistic bubble can match.
    pub client_msg_id: String,
    /// Message text.
    pub body: String,
    /// Server-assigned creation time, milliseconds since the Unix epoch.
    #[ts(type = "number")]
    pub created_at_ms: i64,
}

/// `GET /conversations` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConversationList {
    /// The caller's Conversations, newest first.
    pub conversations: Vec<ConversationSummary>,
}

/// Query parameters of `GET /conversations/{id}/messages`.
///
/// The cursor is the Conversation's Sequence Number, the ordering authority
/// (ADR-0003). Two directions share this one contract:
///
/// - **Backwards** (`before`, exclusive): the `limit` Messages with the highest
///   `seq` strictly below `before`. Omit it for the newest page, then pass the
///   response's `next_before` to load the page before it.
/// - **Forwards** (`after`, exclusive): the `limit` oldest Messages with
///   `seq > after`, used to repair a Conversation after a reconnect or a detected
///   gap. Pass the response's `next_after` to continue.
///
/// `before` and `after` are mutually exclusive; supplying both is rejected.
///
/// Cursor-on-`seq` is what makes page boundaries correct under concurrent sends.
/// `LIMIT/OFFSET` shifts by one every time an older row appears, so a client
/// paging through it loses or repeats a Message; a `seq` cursor cannot, because
/// it is anchored to a value that does not move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessagePageQuery {
    /// Exclusive backward cursor: return Messages with `seq < before`.
    ///
    /// Omit to read the most recent page. A `before` outside the Conversation's
    /// range (below `1`, or above its newest Message) is not an error: it yields
    /// an empty page or the most recent one.
    #[ts(type = "number | null")]
    pub before: Option<i64>,
    /// Exclusive forward cursor: return the oldest Messages with `seq > after`.
    ///
    /// This is the repair direction (ADR-0003 / the reconnect ticket): a client
    /// that holds everything up to `after` pulls exactly the Messages it missed,
    /// oldest first, until `has_more` is false.
    #[ts(type = "number | null")]
    pub after: Option<i64>,
    /// Page size, clamped to `[1, MAX_MESSAGE_PAGE_SIZE]`; defaults to
    /// `DEFAULT_MESSAGE_PAGE_SIZE` when omitted.
    #[ts(type = "number | null")]
    pub limit: Option<i64>,
}

/// `GET /conversations/{id}/messages` response: one page of history.
///
/// Ordering is explicit and deterministic: `messages` is always ascending by
/// `seq` (oldest first), whatever page was requested. Walked backwards,
/// concatenating page N+1 (fetched with `before = next_before`) in front of page N
/// reconstructs the whole history with no duplicates and no gaps. Walked forwards,
/// concatenating each next page (fetched with `after = next_after`) does the same
/// in the repair direction.
///
/// `has_more` means "another page exists **in the direction that was asked for**":
/// older Messages for a `before` page, newer Messages for an `after` page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageList {
    /// The page's Messages, oldest first (ascending `seq`), so the list renders
    /// top-to-bottom. Empty when the cursor points past the end in that direction.
    pub messages: Vec<MessageView>,
    /// The cursor for the next older page: pass this as `before` to fetch it.
    /// `null` when there is nothing older, or when this was a forward page.
    #[ts(type = "number | null")]
    pub next_before: Option<i64>,
    /// The cursor for the next newer page: pass this as `after` to fetch it.
    /// `null` when there is nothing newer, or when this was a backward page.
    #[ts(type = "number | null")]
    pub next_after: Option<i64>,
    /// Whether another page exists beyond this one, in the requested direction.
    pub has_more: bool,
    /// The **other** Participants' public Read Receipts, so the page renders its
    /// "read" indicators on first paint rather than only after a live event
    /// arrives. The caller's own private Read Marker is never part of this list:
    /// a receipt is public, a marker is not (see [`crate::read`]).
    pub read_receipts: Vec<ReadReceipt>,
}

/// `ClientEvent::SendMessage` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SendMessage {
    /// ULID of the Conversation to post into.
    pub conversation_id: String,
    /// The sender's idempotency key for this send. Reusing it is how a retry
    /// avoids a duplicate Message.
    pub client_msg_id: String,
    /// Message text.
    pub body: String,
}

/// `ServerEvent::MessageAck` payload.
///
/// Sent to the connection that submitted the send, and only that connection: it
/// is the reconciliation signal for the optimistic bubble.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageAck {
    /// Echo of [`SendMessage::client_msg_id`], the correlation key.
    pub client_msg_id: String,
    /// The stored Message, with its real id, Sequence Number and timestamp.
    pub message: MessageView,
}

/// `ServerEvent::NewMessage` payload.
///
/// Fanned out to every connected session of every Participant, including the
/// sender's other Devices. A client that also received the acknowledgement must
/// de-duplicate by [`MessageView::id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewMessage {
    /// The stored Message.
    pub message: MessageView,
}

/// `ServerEvent::MessageRejected` payload.
///
/// The sender's optimistic bubble can be marked failed and retried; the retry
/// reuses the same `client_msg_id`, so a rejection never becomes a duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageRejected {
    /// Echo of the rejected [`SendMessage::client_msg_id`].
    pub client_msg_id: String,
    /// Machine-readable reason, from the shared [`ErrorCode`] vocabulary.
    pub code: ErrorCode,
    /// Human-readable reason (Chinese); a display fallback, not a contract.
    pub message: String,
}

/// `ServerEvent::ConversationCreated` payload.
///
/// Pushed to a Participant's live sessions when a Conversation is created, so the
/// peer's conversation list updates without a refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConversationCreated {
    /// The Conversation the caller now participates in.
    pub conversation: ConversationSummary,
}
