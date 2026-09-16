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
//! `jiuyue-server` and `jiuyue-realtime` deliver it. It paginates history
//! backwards on the Sequence Number cursor (ticket #11), maintains read state
//! (ticket #16) and repairs reconnects (tickets #12/#13), but does not model
//! Groups, attachments or reactions (later tickets). The schema and the service
//! are shaped so those can be added without replacing this module.
//!
//! # Read state: two positions, never one
//!
//! The Read Marker (a User's private position) and the Read Receipt (a
//! Participant's public acknowledgement) are stored in two columns, carried by
//! two event types and read by two different queries — see
//! [`jiuyue_contract::read`]. The one place they meet is [`ReadStateUpdate`],
//! which deliberately hands back both so the realtime layer can fan them out to
//! their disjoint recipient sets.
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
    ChatService, ConversationNotice, MAX_SYNC_CURSORS, OpenedConversation, ReadState,
    ReadStateUpdate, SentMessage,
};
