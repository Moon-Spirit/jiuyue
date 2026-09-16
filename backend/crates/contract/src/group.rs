//! Group Conversation wire types: Roles, membership, and the membership events.
//!
//! CONTEXT.md calls a Group Conversation one with three or more Participants and
//! a [`Role`]. This module is where that vocabulary becomes a contract: the Role
//! is a closed enum, a Participant is a [`MemberView`], and every membership
//! change is a [`MembershipChange`] — never a silent row edit.
//!
//! # One event, six shapes
//!
//! Joining, leaving, being removed, a role change, an ownership transfer and a
//! dissolution all travel as [`MembershipChanged`]. The recipient set differs per
//! shape, and the server decides it (see `jiuyue-chat`'s membership service): a
//! Participant who has left or been removed is *not* in the fan-out for the
//! Group's Messages, but is told about their own exit so their client drops the
//! Conversation. A Group Conversation that is dissolved reaches every former
//! Participant; nothing else does.
//!
//! # Why one event rather than six
//!
//! Every membership change has the same shape — "this happened in this
//! Conversation, caused by this actor" — and a client applies them through one
//! reducer that keeps its member list and its Conversation summary coherent. Six
//! top-level [`crate::ServerEvent`] variants would spread that one reducer across
//! six cases and make it easy to forget a summary update on one of them.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::chat::ConversationSummary;

/// A Participant's permission level in a Group Conversation (CONTEXT.md: Role).
///
/// Serialised lowercase because the value is stored verbatim in
/// `conversation_members.role`. It is a **Group** concept: a Direct Conversation's
/// Participants carry [`Role::Member`] and no permission rule reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Role {
    /// Created the group; the only role that may change roles, transfer ownership
    /// or dissolve.
    Owner,
    /// May invite and remove ordinary members.
    Admin,
    /// May post and leave.
    Member,
}

impl Role {
    /// The stored/wire form, matching the `conversation_members.role` CHECK.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    /// Parse the stored form.
    ///
    /// The schema's CHECK admits only the three values, so the fallback arm is a
    /// defensive `Member` rather than a panic; a role the database cannot hold is
    /// not a reason to fail a read.
    pub fn from_stored(value: &str) -> Self {
        match value {
            "owner" => Self::Owner,
            "admin" => Self::Admin,
            _ => Self::Member,
        }
    }
}

/// Default cap on the Participants of one Group Conversation.
///
/// Fan-out is write-based today (every Participant's every Device gets a copy),
/// which ADR-0005 records as fine for small groups and insufficient for large
/// ones. This bound is where that strategy must change — read-fanout for large
/// groups — so it is a deliberate ceiling, not an arbitrary one.
pub const MAX_GROUP_MEMBERS: i64 = 500;

/// Smallest Participant count a Group Conversation may have.
///
/// CONTEXT.md defines a Group Conversation as three or more Participants, so a
/// create or invite that would leave fewer than three is refused rather than
/// quietly producing a two-person "group".
pub const MIN_GROUP_MEMBERS: i64 = 3;

/// Hard cap on a group title, in Unicode scalar values.
pub const MAX_GROUP_TITLE_CHARS: usize = 100;

/// One Participant as the member list renders them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MemberView {
    /// ULID of the User.
    pub user_id: String,
    /// `@handle`.
    pub username: String,
    /// Display name.
    pub display_name: String,
    /// Avatar URL, when one has been set.
    pub avatar_url: Option<String>,
    /// The Participant's Role.
    pub role: Role,
    /// When they became a Participant, milliseconds since the Unix epoch.
    #[ts(type = "number")]
    pub joined_at_ms: i64,
}

/// The Group-specific part of a Conversation's summary.
///
/// A Direct Conversation has no such part: its summary names the peer instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct GroupSummary {
    /// The group's display name.
    pub title: String,
    /// How many Participants the group has now.
    #[ts(type = "number")]
    pub member_count: i64,
    /// The **caller's own** Role in the group.
    pub my_role: Role,
}

/// `POST /conversations/group` body.
///
/// Members are named by `@handle`: a user knows a handle, and the contacts module
/// that would offer an id picker is a later ticket. The caller is always the
/// owner and is never listed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CreateGroupConversationRequest {
    /// Group name, 1–100 characters.
    pub title: String,
    /// `@handle`s of the initial members, matched case-insensitively.
    pub member_usernames: Vec<String>,
}

/// `POST /conversations/{id}/members` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AddGroupMembersRequest {
    /// `@handle`s of the members to invite, matched case-insensitively.
    pub member_usernames: Vec<String>,
}

/// `PATCH /conversations/{id}/members/{user_id}` body.
///
/// Owner-only. `member` demotes an admin; `admin` promotes one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ChangeMemberRoleRequest {
    /// The Role to set; [`Role::Owner`] is refused here — ownership moves only
    /// through [`TransferOwnershipRequest`].
    pub role: Role,
}

/// `POST /conversations/{id}/transfer` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TransferOwnershipRequest {
    /// ULID of the Participant who becomes the new owner. Must currently be one.
    pub user_id: String,
}

/// `GET /conversations/{id}` response: the info panel's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct GroupInfo {
    /// The Conversation as the caller sees it.
    pub conversation: ConversationSummary,
    /// Every Participant with their Role, in join order.
    pub members: Vec<MemberView>,
}

/// What changed about a Group's membership.
///
/// Internally tagged (`{"type": "...", ...}`) so a client reduces all six shapes
/// through one switch. A client that does not recognise a shape ignores it, which
/// keeps the vocabulary additive (ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum MembershipChange {
    /// A User became a Participant.
    Joined {
        /// The Participant who joined.
        member: MemberView,
    },
    /// A User left of their own accord.
    Left {
        /// ULID of the User who left.
        user_id: String,
    },
    /// A User was removed by someone entitled to remove them.
    Removed {
        /// ULID of the User who was removed.
        user_id: String,
    },
    /// A Participant's Role changed.
    RoleChanged {
        /// ULID of the Participant whose Role changed.
        user_id: String,
        /// The Role they hold now.
        role: Role,
    },
    /// Ownership moved from one Participant to another.
    OwnershipTransferred {
        /// The former owner, now an admin.
        from_user_id: String,
        /// The new owner.
        to_user_id: String,
    },
    /// The Group was dissolved; it no longer exists for anyone.
    Dissolved,
}

/// `ServerEvent::MembershipChanged` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MembershipChanged {
    /// ULID of the Conversation the change happened in.
    pub conversation_id: String,
    /// ULID of the User who caused the change — the inviter, the remover, the
    /// leaver themselves, or the owner who changed a Role or dissolved.
    pub actor_id: String,
    /// What changed.
    pub change: MembershipChange,
}

#[cfg(test)]
mod tests {
    use super::{MembershipChange, Role};
    use crate::chat::ConversationSummary;

    #[test]
    fn a_role_round_trips_through_its_stored_form() {
        for role in [Role::Owner, Role::Admin, Role::Member] {
            assert_eq!(Role::from_stored(role.as_str()), role);
        }
        assert_eq!(
            Role::from_stored("something-the-schema-forbids"),
            Role::Member
        );
    }

    #[test]
    fn membership_changes_are_internally_tagged() {
        let change = MembershipChange::RoleChanged {
            user_id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            role: Role::Admin,
        };

        let wire = serde_json::to_value(&change).expect("a change must serialise");

        assert_eq!(wire["type"], "role_changed");
        assert_eq!(wire["role"], "admin");
    }

    #[test]
    fn a_dissolution_carries_only_its_tag() {
        let wire = serde_json::to_value(MembershipChange::Dissolved)
            .expect("a dissolution must serialise");

        assert_eq!(wire["type"], "dissolved");
    }

    /// The summary a Group Conversation renders carries a title, the member count
    /// and the caller's own Role — and no peer.
    #[test]
    fn a_group_summary_carries_the_group_fields() {
        let summary = ConversationSummary {
            id: "01JABC1234567890ABCDEFGHJ1".to_owned(),
            kind: crate::chat::ConversationKind::Group,
            peer: None,
            group: Some(super::GroupSummary {
                title: "九月小组".to_owned(),
                member_count: 3,
                my_role: Role::Owner,
            }),
            unread_count: 0,
            created_at_ms: 1_700_000_000_000,
        };

        let wire = serde_json::to_value(&summary).expect("a summary must serialise");

        assert_eq!(wire["group"]["title"], "九月小组");
        assert_eq!(wire["group"]["my_role"], "owner");
        assert!(wire["peer"].is_null());
    }
}
