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

/// Hard cap on a Message body, in Unicode scalar values.
///
/// The server enforces it and the client mirrors it so the limit is felt before a
/// round trip. It matches the `messages_body_length` check in the schema.
pub const MAX_MESSAGE_BODY_CHARS: usize = 4000;

/// Hard cap on a Client Message ID, in bytes.
pub const MAX_CLIENT_MSG_ID_BYTES: usize = 128;

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
    /// The other Participant of a Direct Conversation; absent for a Group (whose
    /// summary is a later ticket).
    pub peer: Option<PeerSummary>,
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

/// `GET /conversations/{id}/messages` response.
///
/// A bounded window (the most recent messages), not a page: cursor pagination is
/// ticket #10 and deliberately absent here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageList {
    /// The returned Messages, oldest first so the list renders top-to-bottom.
    pub messages: Vec<MessageView>,
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
