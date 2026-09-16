//! jiuyue chat domain: Conversations and Messages.
//!
//! This crate owns the `conversations`, `conversation_members` and `messages`
//! tables (ADR-0011), the atomic Sequence Number allocator that makes a
//! Conversation's ordering authoritative (ADR-0003), and the idempotent send that
//! lets a client retry without duplicating a Message.
//!
//! # What lives here
//!
//! | Type                         | Role                                                     |
//! | ---------------------------- | -------------------------------------------------------- |
//! | [`ChatService`]              | Open, list, read and send — the whole domain surface      |
//! | [`ChatError`]                | Failures, each with a stable wire [`ErrorCode`]           |
//! | [`OpenedConversation`]       | A Direct Conversation plus the Participants to notify     |
//! | [`SentMessage`]              | A stored Message plus the Participants to fan out to      |
//!
//! # What this crate does not do
//!
//! It does not know about HTTP or WebSockets: it returns data and lets
//! `jiuyue-server` and `jiuyue-realtime` deliver it. It does not paginate history
//! (ticket #10), repair reconnects (ticket #11), or model Groups, attachments,
//! reactions or read state (later tickets). The schema and the service are shaped
//! so those can be added without replacing this module.
//!
//! # Retry safety
//!
//! A send carries a Client Message ID (CONTEXT.md). `(sender_id, client_msg_id)`
//! is UNIQUE, so replaying a send returns the original Message rather than writing
//! a second one, and the acknowledgement the sender receives is the same both
//! times. See [`ChatService::send_message`].

#![forbid(unsafe_code)]

mod error;
mod repository;
mod service;

pub use error::ChatError;
pub use repository::ChatRepository;
pub use service::{
    ChatService, ConversationNotice, OpenedConversation, RECENT_MESSAGE_LIMIT, SentMessage,
};
