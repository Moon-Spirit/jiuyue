//! Events carried inside envelopes.
//!
//! Both directions use the same adjacently tagged representation
//! (`#[serde(tag = "t", content = "d")]`): the wire is `{"t": "<Variant>", "d": ...}`.
//! Keeping the tag and the payload in their own fields means a new variant is a
//! purely additive change — old clients see an unknown `t` and skip it.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::chat::{ConversationCreated, MessageAck, MessageRejected, NewMessage, SendMessage};
use crate::group::MembershipChanged;
use crate::presence::Presence;
use crate::read::{MarkRead, ReadMarker, ReadReceipt};
use crate::sync::{Resume, Resync, SyncCursor, SyncState};

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
    /// A Conversation this connection now participates in was created, **or** its
    /// own summary changed in a way every Participant must see live — today, a
    /// Group announcement edit.
    ///
    /// Deliberately one shape rather than a second event: a client already upserts
    /// a Conversation on creation, so an updated summary rides the same reducer,
    /// and the announcement is a new optional field an older client ignores
    /// (ADR-0003's additive rule).
    ConversationCreated(ConversationCreated),
    /// A Group Conversation's membership changed: someone joined, left or was
    /// removed, a Role changed, ownership was transferred, or the group was
    /// dissolved.
    ///
    /// Delivered to the Participants who should see the change, and **only** to
    /// them. A Participant who left or was removed is not in the Group's message
    /// fan-out any more, but is told about their own exit so their client can drop
    /// the Conversation — see [`crate::group`].
    MembershipChanged(MembershipChanged),
    /// The server's answer to [`ClientEvent::Resume`]: whether the connection's
    /// missed envelopes were replayed, or whether the client must repair
    /// Conversations from its cursors (ADR-0003).
    Resync(Resync),
    /// The Device's persisted per-Conversation Sync Cursors, pushed once on connect
    /// when any are stored. The client repairs forward from these positions, which
    /// is what lets a Device that was away — a closed laptop, a new process — learn
    /// exactly what it missed instead of re-reading whole Conversations.
    SyncState(SyncState),
    /// The owning User's **private** Read Marker advanced (CONTEXT.md: 已读标记).
    ///
    /// Sent **only** to the reader's own Devices — never to another Participant.
    /// It is what clears the Unread Count on the account's other Devices when one
    /// Device reads. See [`crate::read`] for why this is not the same event as
    /// [`ServerEvent::ReadReceipt`].
    ReadMarker(ReadMarker),
    /// A Participant's **public** Read Receipt advanced (CONTEXT.md: 已读回执).
    ///
    /// Sent **only** to the *other* Participants, and only from the public
    /// [`ReadReceipt`] position — the reader's private Read Marker has no path to
    /// this event.
    ReadReceipt(ReadReceipt),
    /// A User's reachability changed (CONTEXT.md: Presence).
    ///
    /// Sent **only** to Participants of a Conversation shared with that User — the
    /// people with a reason to care — never to every connected client. The
    /// transition is per **User**, not per Device: a second Device connecting while
    /// the first is already live produces no event at all, and [`Presence::status`]
    /// only turns `offline` when the User's *last* Device goes.
    Presence(Presence),
    /// A Participant started or stopped composing in a Conversation
    /// (CONTEXT.md: Typing Indicator).
    ///
    /// **Ephemeral by construction.** The event carries no Message id and no
    /// Sequence Number, it is never written to a table, and it cannot arrive after
    /// a reconnect: it rides the per-connection `s` (ADR-0003), and a reconnect is
    /// a new connection whose replay buffer starts empty, so a position from the
    /// old connection is answered `unavailable` rather than replayed.
    ///
    /// Sent **only** to the *other* Participants of [`Typing::conversation_id`].
    /// The sender's own Devices are deliberately excluded — seeing your own typing
    /// echoed on your phone is a bug — and the payload carries no Message text, so
    /// message content can never travel on this unpersisted, unmoderated channel.
    /// The receiver must clear the indicator on its own after a bounded interval,
    /// because a client that closes mid-sentence never sends a stop.
    Typing(Typing),
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
    /// Advance this Device's Sync Cursor for one Conversation.
    ///
    /// Best-effort and monotonic: the server keeps the highest value it is told
    /// (never a rewind), coalesces reports in memory, and checkpoints them in
    /// batches. A report the server never receives costs the Device a re-fetch of
    /// the tail on its next connect — harmless under at-least-once delivery.
    SyncCursor(SyncCursor),
    /// Declare how far the User has read in a Conversation (CONTEXT.md: 已读标记).
    ///
    /// One action with two separate effects: the private Read Marker advances
    /// (clearing the Unread Count and echoing to the caller's other Devices) and
    /// the public Read Receipt advances (broadcast to the other Participants).
    /// Monotonic — a replayed or out-of-order report can never rewind either.
    MarkRead(MarkRead),
    /// "I am composing in this Conversation" (CONTEXT.md: Typing Indicator).
    ///
    /// Best-effort and deliberately cheap: the server coalesces repeats in memory,
    /// so a client may emit one signal per keystroke and still cost at most one
    /// fan-out per throttle window. A signal naming a Conversation the caller does
    /// not participate in is dropped, and the server never stores anything for it.
    Typing(TypingSignal),
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

/// The state a Typing Indicator signal carries (CONTEXT.md: 正在输入).
///
/// Two states, not a boolean, for the same reason [`crate::PresenceStatus`] is an
/// enum: an `idle` refinement can be added later as a new variant without touching
/// the shape of every existing payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TypingState {
    /// The Participant is composing; the other Participants show the indicator.
    Started,
    /// The Participant stopped composing (or sent); the indicator clears now.
    Stopped,
}

/// `ClientEvent::Typing` payload: "I am / am not composing in this Conversation".
///
/// The **who** is not in the payload: it is the authenticated User the connection
/// belongs to, so a client cannot claim another Participant is typing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TypingSignal {
    /// ULID of the Conversation the signal applies to. The caller must be a
    /// Participant; anything else is dropped without a fan-out.
    pub conversation_id: String,
    /// Whether the caller started or stopped composing.
    pub state: TypingState,
}

/// `ServerEvent::Typing` payload: **who** is composing, and **where**.
///
/// That is the whole payload — two ids and a state. There is deliberately no
/// Message text field: this event is neither persisted nor moderated, so a
/// content preview here would route around the Cloud Conversation moderation
/// boundary (CONTEXT.md). The receiver orders these by arrival on its single
/// ordered connection; the server emits them in a total per-Conversation order,
/// so a stop can never overtake a later start for the same Participant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Typing {
    /// ULID of the Conversation the indicator belongs to.
    pub conversation_id: String,
    /// ULID of the Participant who is (or is no longer) composing.
    pub user_id: String,
    /// Whether that Participant started or stopped composing.
    pub state: TypingState,
}

#[cfg(test)]
mod tests {
    use super::{ClientEvent, ServerEvent, Typing, TypingSignal, TypingState};
    use crate::envelope::{ClientEnvelope, ServerEnvelope};

    #[test]
    fn a_typing_signal_uses_the_additive_adjacent_tag_and_stays_minimal() {
        let envelope = ClientEnvelope::new(ClientEvent::Typing(TypingSignal {
            conversation_id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            state: TypingState::Started,
        }));

        let wire = serde_json::to_value(&envelope).expect("a client envelope must serialise");

        assert_eq!(wire["e"]["t"], "Typing");
        assert_eq!(wire["e"]["d"]["state"], "started");
        assert_eq!(
            wire["e"]["d"]["conversation_id"],
            "01JABC1234567890ABCDEFGHJ1"
        );
        assert!(
            wire["e"]["d"].get("body").is_none(),
            "a typing signal must never carry Message text"
        );
    }

    #[test]
    fn a_server_typing_event_names_who_and_where_and_nothing_else() {
        let envelope = ServerEnvelope::new(
            4,
            1_750_000_000_000,
            ServerEvent::Typing(Typing {
                conversation_id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
                user_id: "01JABC1234567890ABCDEFGHJ2".to_owned(),
                state: TypingState::Stopped,
            }),
        );

        let wire = serde_json::to_value(&envelope).expect("a server envelope must serialise");

        assert_eq!(wire["e"]["t"], "Typing");
        assert_eq!(wire["e"]["d"]["user_id"], "01JABC1234567890ABCDEFGHJ2");
        assert_eq!(
            wire["e"]["d"]["conversation_id"],
            "01JABC1234567890ABCDEFGHJ1"
        );
        assert_eq!(wire["e"]["d"]["state"], "stopped");
        assert_eq!(
            wire["e"]["d"].as_object().map(serde_json::Map::len),
            Some(3),
            "the payload is who, where and the state — nothing else"
        );
    }

    #[test]
    fn a_typing_state_round_trips_through_json() {
        for state in [TypingState::Started, TypingState::Stopped] {
            let text = serde_json::to_string(&state).expect("a state must serialise");
            let decoded: TypingState =
                serde_json::from_str(&text).expect("a state must deserialise");
            assert_eq!(decoded, state);
        }
    }
}
