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
//!
//! # Groups and the fan-out seam
//!
//! A Group is the same Conversation aggregate with Roles on its Participants
//! ([`permission`] is the one authority on who may do what). Two things keep the
//! eventual read-fanout of ADR-0005 from rewriting this crate:
//!
//! - **The domain returns recipient sets, never delivery.** [`SentMessage`]
//!   carries the Participants of the moment, and [`MembershipUpdate`] carries
//!   per-recipient [`MembershipNotice`]s. `jiuyue-chat` never touches the realtime
//!   registry, so "who" is a domain answer and "how" is the gateway's problem.
//! - **The gateway has one delivery primitive.** `jiuyue-realtime`'s
//!   `ConnectionRegistry::deliver` takes a list of User ids and fans out to their
//!   Devices. Changing small-group write-fanout into large-group read-fanout is a
//!   change to what that call is handed, not to the rules that produced the list.
//! - **The recipient set is also a relationship query.** `presence_audience`
//!   answers "who shares a Conversation with this User" for presence fan-out;
//!   unlike `SentMessage` it carries no payload at all, so the transport decides
//!   what (if anything) to say and issue #27 can narrow who is allowed to hear it
//!   without touching this crate's rules.
//!
//! The recipient sets are also where membership rules become observable: a
//! Participant who left or was removed is absent from the Group's
//! [`SentMessage::participants`], so they stop receiving Group Messages — enforced
//! by the fan-out, not by client-side politeness. They are told about their own
//! exit through [`MembershipNotice`] so their client drops the Conversation.

#![forbid(unsafe_code)]

mod error;
pub mod permission;
mod repository;
mod service;

pub use error::ChatError;
pub use permission::Capability;
pub use repository::ChatRepository;
pub use service::{
    ChatService, ConversationNotice, MAX_SYNC_CURSORS, MembershipNotice, MembershipUpdate,
    OpenedConversation, ReadState, ReadStateUpdate, SentMessage,
};
