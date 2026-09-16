//! Presence (CONTEXT.md: 在线状态) — who is reachable, and when they last were.
//!
//! Presence has two halves, and this module carries both of them on the wire:
//!
//! - **Live**: a [`Presence`] rides [`crate::ServerEvent::Presence`] to the
//!   Participants of a shared Conversation the moment a User's reachability
//!   changes. The change is always a **User** transition — going online is the
//!   *first* of a User's Devices connecting and going offline is the *last* one
//!   disconnecting (CONTEXT.md draws exactly this Device-vs-User line) — so a
//!   phone and a laptop never flicker each other offline.
//! - **On demand**: [`PresenceList`] is the REST read model, so a client that has
//!   just opened a Conversation can render the current state without waiting for
//!   the next change.
//!
//! # Visibility is a decision, not a fan-out
//!
//! Both halves answer "who may see this User's presence" through the same rule:
//! **only Participants of a shared Conversation**. That rule lives in one seam
//! (`jiuyue_chat::ChatService::presence_audience`), so the "who can see my
//! presence" settings of issue #27 become a *filter applied at that seam* rather
//! than a rewrite of every call site. Nothing here fans out unconditionally.
//!
//! # Last seen
//!
//! [`Presence::last_seen_ms`] is `null` while a User is online (the present is not
//! a past instant) and carries the instant they stopped being reachable when they
//! are offline. It is the one durable half of presence: reachability itself is
//! in-process state, so a restart makes everybody offline by construction, while
//! the last-seen instant survives in PostgreSQL.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A User's reachability (CONTEXT.md: Presence).
///
/// Two states, not three: `away` in CONTEXT.md is an *idle* refinement of online
/// (reachable, just quiet) and is deliberately not produced yet — adding the
/// variant later is an additive contract change, which is the whole reason the
/// variants are a string-tagged enum rather than a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PresenceStatus {
    /// At least one of the User's Devices is connected.
    Online,
    /// No Device of the User is connected.
    Offline,
}

/// One User's presence, as the server reports it.
///
/// Sent as [`crate::ServerEvent::Presence`] on a change, and listed by
/// `GET /presence` for a client that needs the current state now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Presence {
    /// ULID of the User whose presence this is.
    pub user_id: String,
    /// Whether the User is reachable right now.
    pub status: PresenceStatus,
    /// When the User was last reachable, milliseconds since the Unix epoch.
    ///
    /// `null` in two cases, both meaning "no past instant to show": while the
    /// User is online, and for a User who has never been seen offline yet.
    #[ts(type = "number | null")]
    pub last_seen_ms: Option<i64>,
}

/// Query parameters of `GET /presence`.
///
/// `user_ids` is a comma-separated list of ULIDs — the shape a query string can
/// carry without repeated keys. The handler splits, trims and de-duplicates it and
/// refuses more than [`MAX_PRESENCE_QUERY`] entries, so one request can never ask
/// the server to materialise an unbounded answer on the 2 vCPU / 2 GB box.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PresenceQuery {
    /// Comma-separated ULIDs to ask about; required and non-empty.
    pub user_ids: Option<String>,
}

/// `GET /presence` response: one entry per **visible** requested User.
///
/// A requested User who does not share a Conversation with the caller is omitted
/// rather than reported offline — the list is the visibility-filtered answer, not
/// a directory. Order follows the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PresenceList {
    /// The visible Users' presence, in request order.
    pub presences: Vec<Presence>,
}

/// Upper bound on how many Users one `GET /presence` may ask about.
///
/// Sized like the other list bounds in this contract: generous for a screenful of
/// participants (a Direct peer, a group's member list) while keeping the point
/// lookup's `= ANY($1)` array, and the response, bounded.
pub const MAX_PRESENCE_QUERY: usize = 200;

#[cfg(test)]
mod tests {
    use super::{MAX_PRESENCE_QUERY, Presence, PresenceList, PresenceQuery, PresenceStatus};

    #[test]
    fn a_presence_serialises_with_snake_case_status_and_null_last_seen() {
        let presence = Presence {
            user_id: "01JABC1234567890ABCDEFGHJ3".to_owned(),
            status: PresenceStatus::Online,
            last_seen_ms: None,
        };

        let wire = serde_json::to_value(&presence).expect("presence must serialise");

        assert_eq!(wire["user_id"], "01JABC1234567890ABCDEFGHJ3");
        assert_eq!(wire["status"], "online");
        assert!(wire["last_seen_ms"].is_null());
    }

    #[test]
    fn an_offline_presence_carries_the_instant_the_user_was_last_seen() {
        let presence = Presence {
            user_id: "01JABC1234567890ABCDEFGHJ3".to_owned(),
            status: PresenceStatus::Offline,
            last_seen_ms: Some(1_750_000_000_000),
        };

        let wire = serde_json::to_value(&presence).expect("presence must serialise");

        assert_eq!(wire["status"], "offline");
        assert_eq!(wire["last_seen_ms"], 1_750_000_000_000_i64);
    }

    #[test]
    fn the_query_carries_a_comma_separated_list_and_the_list_wraps_presences() {
        let query: PresenceQuery = serde_json::from_value(serde_json::json!({ "user_ids": "a,b" }))
            .expect("the query must deserialise");
        assert_eq!(query.user_ids.as_deref(), Some("a,b"));

        let list = PresenceList {
            presences: vec![Presence {
                user_id: "01JABC1234567890ABCDEFGHJ3".to_owned(),
                status: PresenceStatus::Offline,
                last_seen_ms: None,
            }],
        };
        let wire = serde_json::to_value(&list).expect("the list must serialise");
        assert_eq!(wire["presences"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn the_query_bound_is_a_positive_screenful() {
        // A group's member list is the widest realistic single query; the bound
        // must comfortably hold one and stay finite.
        const {
            assert!(MAX_PRESENCE_QUERY >= 100, "a group member list must fit");
            assert!(
                MAX_PRESENCE_QUERY <= 1_000,
                "the point lookup must stay bounded"
            );
        }
    }
}
