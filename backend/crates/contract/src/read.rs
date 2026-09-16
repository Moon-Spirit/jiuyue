//! Read state: the **Read Marker** (private) and the **Read Receipt** (public).
//!
//! CONTEXT.md draws a hard line between two positions that travel the same road:
//!
//! - The **Read Marker** is a User's *private* position in a Conversation — "I
//!   have read up to here". It drives the Unread Count and is **never shown to
//!   anyone else**. It is the axis [`ReadMarker`] describes.
//! - The **Read Receipt** is a Participant's *public* acknowledgement, shown to
//!   the other Participants. It is the axis [`ReadReceipt`] describes.
//!
//! They are separate on the wire as well as in storage, and this module is where
//! that separation is stated once:
//!
//! - [`ReadMarker`] and [`ReadReceipt`] are different types with **disjoint field
//!   sets**. A private marker is the only thing that carries
//!   [`ReadMarker::unread_count`]; a public receipt is the only thing that names
//!   [`ReadReceipt::reader_id`]. There is no conversion between them, so one can
//!   never be constructed from the other.
//! - They travel as two different [`crate::ServerEvent`] variants
//!   (`ReadMarker` / `ReadReceipt`), so a client that receives one has not
//!   received the other.
//! - The server sends [`ReadMarker`] **only** to the owning User's own Devices
//!   and [`ReadReceipt`] **only** to the *other* Participants. The recipient sets
//!   are disjoint, so a peer's private marker has no path to another User's
//!   socket.
//!
//! These types are deliberately positions (a Sequence Number), never a
//! per-Message record: a receipt for a thousand Messages is still one row and one
//! frame.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// `ClientEvent::MarkRead` payload: "this User has read up to `last_read_seq`".
///
/// One client action, two server-side effects that stay separate: it advances the
/// caller's private **Read Marker** (which clears the Unread Count and is echoed
/// to the caller's other Devices) and the caller's public **Read Receipt** (which
/// is broadcast to the other Participants). The value is a Sequence Number
/// (ADR-0003), and the server clamps it to the newest Message that actually
/// exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MarkRead {
    /// ULID of the Conversation the User has been reading.
    pub conversation_id: String,
    /// Highest Sequence Number the User has read.
    #[ts(type = "number")]
    pub last_read_seq: i64,
}

/// `ServerEvent::ReadMarker` payload — the caller's **private** position.
///
/// Delivered **only** to the owning User's Devices, never to another Participant;
/// that is what lets a second Device of the same account clear its badge without
/// the peer learning anything. Carries the recomputed [`Self::unread_count`] so
/// every Device converges on the server's value rather than on its own guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ReadMarker {
    /// ULID of the Conversation the marker belongs to.
    pub conversation_id: String,
    /// Highest Sequence Number the User has read (the Read Marker).
    #[ts(type = "number")]
    pub last_read_seq: i64,
    /// The User's Unread Count after the marker advanced.
    #[ts(type = "number")]
    pub unread_count: i64,
}

/// `ServerEvent::ReadReceipt` payload — a Participant's **public** acknowledgement.
///
/// Delivered **only** to the *other* Participants. It names the reader, because a
/// receipt is meaningless without knowing whose it is, and it carries a position
/// rather than a per-Message record. It deliberately has no unread field: the
/// Unread Count is a User's private business, not part of a receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ReadReceipt {
    /// ULID of the Conversation the receipt is for.
    pub conversation_id: String,
    /// ULID of the Participant who read; a receipt is always attributable.
    pub reader_id: String,
    /// Highest Sequence Number that Participant has publicly acknowledged.
    #[ts(type = "number")]
    pub last_read_seq: i64,
}

#[cfg(test)]
mod tests {
    use super::{ReadMarker, ReadReceipt};
    use crate::events::ServerEvent;

    #[test]
    fn a_receipt_carries_no_private_read_state() {
        let receipt = ReadReceipt {
            conversation_id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            reader_id: "01JABC1234567890ABCDEFGHJ2".to_owned(),
            last_read_seq: 7,
        };

        let wire = serde_json::to_value(&receipt).expect("a receipt must serialise");

        assert_eq!(wire["reader_id"], "01JABC1234567890ABCDEFGHJ2");
        assert_eq!(wire["last_read_seq"], 7);
        assert!(
            wire.get("unread_count").is_none(),
            "the Unread Count is the reader's private business and must not ride a receipt"
        );
    }

    #[test]
    fn a_marker_names_no_reader_and_carries_the_unread_count() {
        let marker = ReadMarker {
            conversation_id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            last_read_seq: 7,
            unread_count: 0,
        };

        let wire = serde_json::to_value(&marker).expect("a marker must serialise");

        assert_eq!(wire["unread_count"], 0);
        assert!(
            wire.get("reader_id").is_none(),
            "a private marker is not attributable to a peer and names no reader"
        );
    }

    #[test]
    fn the_two_read_events_are_distinguishable_on_the_wire_by_their_tag() {
        let marker = ServerEvent::ReadMarker(ReadMarker {
            conversation_id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            last_read_seq: 3,
            unread_count: 0,
        });
        let receipt = ServerEvent::ReadReceipt(ReadReceipt {
            conversation_id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            reader_id: "01JABC1234567890ABCDEFGHJ2".to_owned(),
            last_read_seq: 3,
        });

        let marker = serde_json::to_value(&marker).expect("the event must serialise");
        let receipt = serde_json::to_value(&receipt).expect("the event must serialise");

        assert_eq!(marker["t"], "ReadMarker");
        assert_eq!(receipt["t"], "ReadReceipt");
        assert!(
            marker["d"].get("reader_id").is_none(),
            "a marker event never carries a peer-facing field"
        );
        assert!(
            receipt["d"].get("unread_count").is_none(),
            "a receipt event never carries the private unread count"
        );
    }
}
