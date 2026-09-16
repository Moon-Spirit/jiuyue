//! The one place a Group permission decision is made (CONTEXT.md: Role).
//!
//! `docs/spec/0001-product-spec.md` requires group permission checks to be
//! centralised: a `role == Owner` scattered through handlers is how privilege
//! bugs get in. Every handler and every use case in this crate therefore asks
//! **this module**, and never inspects a [`Role`] itself.
//!
//! Two questions are answered here, in one decision table:
//!
//! | Capability            | Owner | Admin | Member |
//! | --------------------- | :---: | :---: | :----: |
//! | View members          |   ✓   |   ✓   |   ✓    |
//! | Post a Message        |   ✓   |   ✓   |   ✓    |
//! | Leave                 |  *    |   ✓   |   ✓    |
//! | Invite members        |   ✓   |   ✓   |   ✗    |
//! | Remove a member       |   ✓   |  *    |   ✗    |
//! | Edit group info       |   ✓   |   ✓   |   ✗    |
//! | Change roles          |   ✓   |   ✗   |   ✗    |
//! | Transfer ownership    |   ✓   |   ✗   |   ✗    |
//! | Dissolve              |   ✓   |   ✗   |   ✗    |
//!
//! The two starred cells are not a role alone, which is why they are separate
//! functions rather than rows in [`may`]:
//!
//! - **Leave** — an owner may leave only when they are the last Participant;
//!   otherwise the group would be left with no Role that can administer it.
//!   [`may_leave`] takes the number of Participants who would remain.
//! - **Remove** — an admin may remove an ordinary member but not the owner and not
//!   another admin; only the owner may remove an admin. [`may_remove`] takes the
//!   target's Role.
//!
//! Being a Participant at all is a separate, earlier check (a non-participant does
//! not have a Role to ask about): use cases resolve membership first and surface
//! [`crate::ChatError::NotAParticipant`], so "you are not in this group" is never
//! confused with "your role forbids this".

use jiuyue_contract::Role;

use crate::error::ChatError;

/// An action a Participant might take in a Group Conversation.
///
/// A closed enum rather than a free string, so the decision table is exhaustive
/// and a new action cannot be added without answering "who may do this?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Read the member list.
    ViewMembers,
    /// Post a Message into the group.
    PostMessage,
    /// Invite Users into the group.
    InviteMembers,
    /// Remove another Participant (refined by [`may_remove`]).
    RemoveMembers,
    /// Promote an admin or demote one back to member.
    ChangeRoles,
    /// Hand ownership to another Participant.
    TransferOwnership,
    /// Dissolve the group for everyone.
    Dissolve,
    /// Edit the group's own profile (title, announcement).
    EditGroupInfo,
    /// Leave (refined by [`may_leave`]).
    Leave,
}

impl Capability {
    /// The human phrase used in a refusal, so wording lives with the rule.
    pub fn describe(self) -> &'static str {
        match self {
            Self::ViewMembers => "查看成员列表",
            Self::PostMessage => "发送消息",
            Self::InviteMembers => "邀请成员",
            Self::RemoveMembers => "移除成员",
            Self::ChangeRoles => "设置成员角色",
            Self::TransferOwnership => "转让群主",
            Self::Dissolve => "解散群聊",
            Self::EditGroupInfo => "编辑群资料",
            Self::Leave => "退出群聊",
        }
    }
}

/// Whether `role` may perform `capability`, ignoring per-target refinements.
///
/// Exhaustive over both enums: adding a Role or a Capability stops this compiling
/// until the answer is written down.
pub fn may(role: Role, capability: Capability) -> bool {
    match capability {
        Capability::ViewMembers | Capability::PostMessage | Capability::Leave => true,
        Capability::InviteMembers | Capability::RemoveMembers | Capability::EditGroupInfo => {
            matches!(role, Role::Owner | Role::Admin)
        }
        Capability::ChangeRoles | Capability::TransferOwnership | Capability::Dissolve => {
            role == Role::Owner
        }
    }
}

/// Refuse unless `role` may `capability`.
pub fn ensure(role: Role, capability: Capability) -> Result<(), ChatError> {
    if may(role, capability) {
        Ok(())
    } else {
        Err(refuse(capability))
    }
}

/// Whether a Participant holding `actor` may remove one holding `target`.
///
/// - the owner may remove an admin or a member, but not themselves (the only
///   owner cannot be the target, and `actor == target` is refused earlier as
///   "you cannot remove yourself");
/// - an admin may remove an ordinary member only — not the owner, not a peer;
/// - a member may remove nobody.
pub fn may_remove(actor: Role, target: Role) -> bool {
    match actor {
        Role::Owner => target != Role::Owner,
        Role::Admin => target == Role::Member,
        Role::Member => false,
    }
}

/// Refuse unless `actor` may remove a Participant holding `target`.
pub fn ensure_remove(actor: Role, target: Role) -> Result<(), ChatError> {
    if may_remove(actor, target) {
        Ok(())
    } else {
        Err(refuse(Capability::RemoveMembers))
    }
}

/// Whether a Participant holding `role` may leave when `others_remaining`
/// Participants would stay behind.
///
/// Everyone may leave an empty group — that is just leaving. An owner may not
/// leave Participants behind, because the group would have no one who can
/// administer it; they must transfer ownership or dissolve first.
pub fn may_leave(role: Role, others_remaining: i64) -> bool {
    role != Role::Owner || others_remaining <= 0
}

/// Refuse unless a Participant holding `role` may leave.
pub fn ensure_leave(role: Role, others_remaining: i64) -> Result<(), ChatError> {
    if may_leave(role, others_remaining) {
        Ok(())
    } else {
        Err(ChatError::OwnerCannotLeave)
    }
}

/// The one refusal constructor, so every denial is a [`ChatError::NotPermitted`]
/// carrying the capability's own wording.
fn refuse(capability: Capability) -> ChatError {
    ChatError::NotPermitted {
        action: capability.describe(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Capability, ensure, ensure_remove, may, may_leave, may_remove};
    use crate::error::ChatError;
    use jiuyue_contract::Role;

    #[test]
    fn an_owner_may_do_everything() {
        for capability in [
            Capability::ViewMembers,
            Capability::PostMessage,
            Capability::InviteMembers,
            Capability::RemoveMembers,
            Capability::ChangeRoles,
            Capability::TransferOwnership,
            Capability::Dissolve,
            Capability::EditGroupInfo,
            Capability::Leave,
        ] {
            assert!(
                may(Role::Owner, capability),
                "owner must allow {capability:?}"
            );
        }
    }

    #[test]
    fn an_admin_may_invite_and_edit_but_not_govern() {
        assert!(may(Role::Admin, Capability::InviteMembers));
        assert!(may(Role::Admin, Capability::EditGroupInfo));
        assert!(!may(Role::Admin, Capability::ChangeRoles));
        assert!(!may(Role::Admin, Capability::TransferOwnership));
        assert!(!may(Role::Admin, Capability::Dissolve));
    }

    #[test]
    fn a_member_may_only_view_post_and_leave() {
        assert!(may(Role::Member, Capability::ViewMembers));
        assert!(may(Role::Member, Capability::PostMessage));
        assert!(may(Role::Member, Capability::Leave));
        assert!(!may(Role::Member, Capability::InviteMembers));
        assert!(!may(Role::Member, Capability::RemoveMembers));
    }

    #[test]
    fn an_admin_removes_only_ordinary_members() {
        assert!(may_remove(Role::Admin, Role::Member));
        assert!(!may_remove(Role::Admin, Role::Admin));
        assert!(!may_remove(Role::Admin, Role::Owner));
        assert!(!may_remove(Role::Member, Role::Member));
    }

    #[test]
    fn an_owner_removes_admins_and_members_but_not_the_owner() {
        assert!(may_remove(Role::Owner, Role::Admin));
        assert!(may_remove(Role::Owner, Role::Member));
        assert!(
            !may_remove(Role::Owner, Role::Owner),
            "the only owner is the actor themselves; removing yourself is not removal"
        );
    }

    #[test]
    fn an_owner_leaves_only_when_nobody_stays_behind() {
        assert!(!may_leave(Role::Owner, 1));
        assert!(!may_leave(Role::Owner, 7));
        assert!(may_leave(Role::Owner, 0));
        assert!(may_leave(Role::Admin, 3));
        assert!(may_leave(Role::Member, 3));
    }

    #[test]
    fn a_refusal_is_a_not_permitted_error_carrying_the_action() {
        let error =
            ensure(Role::Member, Capability::Dissolve).expect_err("a member may not dissolve");

        match error {
            ChatError::NotPermitted { action } => assert_eq!(action, "解散群聊"),
            other => panic!("expected NotPermitted, got {other:?}"),
        }
    }

    #[test]
    fn refusing_a_removal_names_removal() {
        let error =
            ensure_remove(Role::Admin, Role::Owner).expect_err("an admin may not remove an owner");

        match error {
            ChatError::NotPermitted { action } => assert_eq!(action, "移除成员"),
            other => panic!("expected NotPermitted, got {other:?}"),
        }
    }
}
