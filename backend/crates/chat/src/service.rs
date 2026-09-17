//! The chat use cases.
//!
//! [`ChatService`] is the whole surface the HTTP layer and the realtime gateway
//! need: open a Direct Conversation, list the caller's Conversations, read a
//! bounded window of Messages, and send one. It composes the repository and the
//! validation rules, so a handler never has to know how a Conversation is keyed,
//! how a Sequence Number is allocated, or which table holds what.
//!
//! Everything here is deterministic on retry: opening the same Direct
//! Conversation twice returns the same Conversation, and sending with the same
//! Client Message ID twice returns the same Message.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;

use jiuyue_contract::group::{MAX_ANNOUNCEMENT_CHARS, UpdateAnnouncementRequest};
use jiuyue_contract::{
    AddGroupMembersRequest, ChangeMemberRoleRequest, ConversationKind, ConversationSummary,
    CreateGroupConversationRequest, DEFAULT_MESSAGE_PAGE_SIZE, FieldError, FieldErrorCode,
    GroupInfo, GroupSummary, MAX_CLIENT_MSG_ID_BYTES, MAX_GROUP_MEMBERS, MAX_GROUP_TITLE_CHARS,
    MAX_MESSAGE_BODY_CHARS, MAX_MESSAGE_PAGE_SIZE, MIN_GROUP_MEMBERS, MarkRead, MemberView,
    MembershipChange, MessageList, MessagePageQuery, MessageView, PeerSummary, ReadReceipt, Role,
    SendMessage, SyncCursor, TransferOwnershipRequest,
};

use crate::error::ChatError;
use crate::permission::{self, Capability};
use crate::repository::{
    ChatRepository, ConversationRow, MemberRow, MessageRow, NewMessageRow, PeerRow, ReadStateRow,
};

/// Upper bound on the cursors one connection is handed at once.
///
/// The per-connection sync frame is O(conversations), and an unbounded frame would
/// be a memory risk on the 2 vCPU / 2 GB box (the socket's `MAX_MESSAGE_SIZE` is
/// the backstop, not the plan). A Device holding more stored cursors than this is
/// sent the most recently advanced ones; the remainder fall back to the
/// pre-cursor behaviour of loading the newest page, which never loses a Message —
/// it only re-reads, because the Conversation stream is the durable record.
pub const MAX_SYNC_CURSORS: i64 = 1000;

/// The notification one Participant should receive when a Conversation is created.
///
/// A Direct Conversation has no single rendering: each Participant's view names
/// the *other* party as the peer. The payload is therefore per-recipient, not one
/// event shared by everyone.
#[derive(Debug, Clone)]
pub struct ConversationNotice {
    /// ULID of the User who should receive [`Self::conversation`].
    pub user_id: String,
    /// The Conversation as that User sees it.
    pub conversation: ConversationSummary,
}

/// A Conversation the caller just opened, plus the notices to push.
#[derive(Debug, Clone)]
pub struct OpenedConversation {
    /// The Conversation as the caller sees it.
    pub summary: ConversationSummary,
    /// Per-Participant renderings, for the realtime fan-out.
    pub notices: Vec<ConversationNotice>,
    /// Whether this call created the Conversation, as opposed to reopening it.
    pub created: bool,
}

/// One thing to tell one set of Users after a Group membership change.
///
/// The service decides **who** should learn **what**; the caller (the HTTP layer)
/// is the only thing that knows how to deliver it. That split is deliberate: it
/// keeps `jiuyue-chat` free of the realtime registry, and it is where "a removed
/// member must not receive group traffic" is expressed in the domain rather than
/// in client-side politeness.
#[derive(Debug, Clone)]
pub enum MembershipNotice {
    /// This User should now see the Conversation; delivered as a
    /// `ConversationCreated` event.
    Conversation {
        /// The User who gains the Conversation.
        user_id: String,
        /// The Conversation as that User sees it.
        conversation: ConversationSummary,
    },
    /// These Users should apply this change; delivered as a
    /// [`jiuyue_contract::MembershipChanged`] event.
    Changed {
        /// Every User who should receive the change.
        user_ids: Vec<String>,
        /// What changed.
        change: MembershipChange,
    },
}

/// The outcome of one membership mutation: who to tell, and what.
#[derive(Debug, Clone)]
pub struct MembershipUpdate {
    /// The Conversation the change happened in.
    pub conversation_id: String,
    /// ULID of the User who caused the change, carried into the event so every
    /// recipient can attribute it without a second lookup.
    pub actor_id: String,
    /// The recipients and their events.
    pub notices: Vec<MembershipNotice>,
}

/// A stored Message, plus the Participants the caller must fan it out to.
#[derive(Debug, Clone)]
pub struct SentMessage {
    /// The stored Message (newly written, or the original of a replayed send).
    pub message: MessageView,
    /// ULIDs of every Participant, for the realtime fan-out.
    pub participants: Vec<String>,
    /// Whether this call wrote the row.
    ///
    /// `false` means the send was a replay — the Message already existed and this
    /// call only observed it. The acknowledgement is still returned (the sender's
    /// acknowledgement may have been lost), but the caller must **not** fan out
    /// again: a retry is a no-op, and a second [`jiuyue_contract::NewMessage`]
    /// would force every peer to de-duplicate by Message id.
    pub created: bool,
}

/// The outcome of a User marking a Conversation read.
///
/// It carries **both** positions, and the caller must keep them apart: the
/// [`Self::read_marker_seq`] is private (fan out to the reader's own Devices
/// only) and [`Self::read_receipt_seq`] is public (fan out to the other
/// Participants including the reader's identity). See [`crate::service`]'s
/// `mark_read` and `jiuyue_contract::read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadStateUpdate {
    /// The Conversation that was read.
    pub conversation_id: String,
    /// The reader (the private marker's owner).
    pub user_id: String,
    /// The reader's private Read Marker after the advance.
    pub read_marker_seq: i64,
    /// The reader's public Read Receipt after the advance.
    pub read_receipt_seq: i64,
    /// The reader's Unread Count after the advance.
    pub unread_count: i64,
    /// Every **other** Participant, for the public receipt fan-out. Never
    /// contains the reader, so the private marker's owner is not in this set.
    pub others: Vec<String>,
}

/// One User's private read state in one Conversation.
///
/// This is the owner's own view of both positions; it is never a peer-facing
/// shape. The Read Marker drives the Unread Count and is not shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadState {
    /// The User's private Read Marker.
    pub read_marker_seq: i64,
    /// The User's public Read Receipt.
    pub read_receipt_seq: i64,
    /// The User's Unread Count.
    pub unread_count: i64,
}

/// The chat domain's public API.
pub struct ChatService {
    repository: ChatRepository,
}

impl ChatService {
    /// Build the service over a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self {
            repository: ChatRepository::new(pool),
        }
    }

    /// Open the Direct Conversation between the caller and the named peer.
    ///
    /// Idempotent by construction: the Conversation is keyed by the unordered pair
    /// of ULIDs, so calling this twice — sequentially or on two nodes at the same
    /// instant — yields exactly one row.
    pub async fn open_direct(
        &self,
        caller_id: &str,
        peer_username: &str,
    ) -> Result<OpenedConversation, ChatError> {
        let username = peer_username.trim().to_lowercase();
        if username.is_empty() {
            return Err(validation(
                "peer_username",
                FieldErrorCode::Required,
                "请输入对方用户名",
            ));
        }

        let peer = self
            .repository
            .find_user_by_username(&username)
            .await?
            .ok_or(ChatError::UserNotFound)?;

        if peer.id == caller_id {
            return Err(validation(
                "peer_username",
                FieldErrorCode::InvalidFormat,
                "不能与自己开始单聊",
            ));
        }

        // The caller's own profile, so the peer's view of the Conversation can name
        // the caller as *its* peer. The caller is authenticated, so the row exists.
        let caller = self
            .repository
            .find_user_by_id(caller_id)
            .await?
            .ok_or(ChatError::Internal)?;

        let direct_key = canonical_direct_key(caller_id, &peer.id);
        let proposed_id = new_id();

        let (conversation, created) = match self
            .repository
            .insert_direct(&proposed_id, &direct_key)
            .await?
        {
            Some(row) => (row, true),
            None => (
                self.repository
                    .find_by_direct_key(&direct_key)
                    .await?
                    .ok_or(ChatError::Internal)?,
                false,
            ),
        };

        let participants = vec![caller_id.to_owned(), peer.id.clone()];
        self.repository
            .add_members(&conversation.id, &participants)
            .await?;

        if created {
            tracing::info!(
                conversation_id = %conversation.id,
                "direct conversation created"
            );
        }

        let caller_view = summary_from(&conversation, &peer, 0);
        let peer_view = summary_from(&conversation, &caller, 0);
        let notices = vec![
            ConversationNotice {
                user_id: caller_id.to_owned(),
                conversation: caller_view.clone(),
            },
            ConversationNotice {
                user_id: peer.id,
                conversation: peer_view,
            },
        ];

        Ok(OpenedConversation {
            summary: caller_view,
            notices,
            created,
        })
    }

    /// Every Conversation the caller participates in, newest first.
    ///
    /// Direct and Group Conversations are two projections (a Direct one names the
    /// peer; a Group one carries its title, its Participant count and the caller's
    /// Role) read separately and merged here, so a Group never has to pretend to
    /// have a peer and vice versa. The merge is ordered by creation time, the same
    /// order each query already returns.
    pub async fn list_conversations(
        &self,
        caller_id: &str,
    ) -> Result<Vec<ConversationSummary>, ChatError> {
        let direct = self
            .repository
            .list_conversations(caller_id)
            .await?
            .into_iter()
            .map(|(conversation, peer, unread_count)| {
                summary_from(&conversation, &peer, unread_count)
            });

        let groups = self
            .repository
            .list_group_conversations(caller_id)
            .await?
            .into_iter()
            .map(|group| {
                group_summary_from(
                    &group.conversation,
                    Role::from_stored(&group.role),
                    group.member_count,
                    group.unread_count,
                )
            });

        let mut summaries: Vec<ConversationSummary> = direct.chain(groups).collect();
        // Newest first; `Reverse` keeps the key extraction cheap and the intent
        // obvious, rather than a hand-written descending comparator.
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.created_at_ms));
        Ok(summaries)
    }

    /// Create a Group Conversation with the caller as owner.
    ///
    /// CONTEXT.md defines a Group as three or more Participants, so a create that
    /// would leave fewer is refused: the caller plus at least two others. Members
    /// are resolved by `@handle`, deduplicated by ULID, and written with the owner
    /// and the members in one transaction, so a group never exists half-populated.
    ///
    /// The returned notices tell every invited member (and the caller's other
    /// Devices) that the group now exists, each with **their own** Role in the
    /// summary — the caller `owner`, everyone else `member`.
    pub async fn create_group(
        &self,
        caller_id: &str,
        request: CreateGroupConversationRequest,
    ) -> Result<OpenedConversation, ChatError> {
        let title = request.title.trim().to_owned();
        if title.is_empty() {
            return Err(validation(
                "title",
                FieldErrorCode::Required,
                "请输入群名称",
            ));
        }
        if title.chars().count() > MAX_GROUP_TITLE_CHARS {
            return Err(validation(
                "title",
                FieldErrorCode::TooLong,
                "群名称最多 100 个字符",
            ));
        }

        let usernames = normalise_usernames(&request.member_usernames);
        if usernames.is_empty() {
            return Err(validation(
                "member_usernames",
                FieldErrorCode::Required,
                "请至少邀请两位成员",
            ));
        }

        let mut members: Vec<PeerRow> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for username in &usernames {
            let user = self
                .repository
                .find_user_by_username(username)
                .await?
                .ok_or(ChatError::UserNotFound)?;
            if user.id == caller_id {
                return Err(validation(
                    "member_usernames",
                    FieldErrorCode::InvalidFormat,
                    "群主无需重复添加自己",
                ));
            }
            if seen.insert(user.id.clone()) {
                members.push(user);
            }
        }

        let member_ids: Vec<String> = members.iter().map(|member| member.id.clone()).collect();
        let total = 1 + member_ids.len() as i64;
        if total < MIN_GROUP_MEMBERS {
            return Err(validation(
                "member_usernames",
                FieldErrorCode::TooShort,
                "群聊至少需要三位成员",
            ));
        }
        if total > MAX_GROUP_MEMBERS {
            return Err(validation(
                "member_usernames",
                FieldErrorCode::TooLong,
                "群聊成员数量已达上限",
            ));
        }

        let conversation = self
            .repository
            .create_group_with_members(&new_id(), &title, caller_id, &member_ids)
            .await?;

        tracing::info!(
            conversation_id = %conversation.id,
            members = total,
            "group conversation created"
        );

        let caller_summary = group_summary_from(&conversation, Role::Owner, total, 0);
        let mut notices = vec![ConversationNotice {
            user_id: caller_id.to_owned(),
            conversation: caller_summary.clone(),
        }];
        for member in &members {
            notices.push(ConversationNotice {
                user_id: member.id.clone(),
                conversation: group_summary_from(&conversation, Role::Member, total, 0),
            });
        }

        Ok(OpenedConversation {
            summary: caller_summary,
            notices,
            created: true,
        })
    }

    /// The info panel's data: the caller's Conversation view plus every Participant.
    ///
    /// Requires being a Participant; a non-member is refused with
    /// [`ChatError::NotAParticipant`], and a Direct Conversation with
    /// [`ChatError::NotAGroup`].
    pub async fn group_info(
        &self,
        caller_id: &str,
        conversation_id: &str,
    ) -> Result<GroupInfo, ChatError> {
        let (conversation, role) =
            require_group_role(&self.repository, conversation_id, caller_id).await?;

        let members = self.repository.list_members(conversation_id).await?;
        let unread_count = self
            .repository
            .read_state(conversation_id, caller_id)
            .await?
            .map_or(0, |state| state.unread_count);

        Ok(GroupInfo {
            conversation: group_summary_from(
                &conversation,
                role,
                members.len() as i64,
                unread_count,
            ),
            members: members.iter().map(member_view).collect(),
        })
    }

    /// Replace a Group's announcement, or clear it.
    ///
    /// Owner or admin only — the same [`Capability::EditGroupInfo`] row that
    /// governs the group's title, because both edit the group's own profile. The
    /// capability is asked through the one permission module, never re-derived
    /// here, so the answer is written down in [`crate::permission`]'s table.
    ///
    /// The write happens first and the fan-out second, so the text that reaches
    /// the members is the text that is stored — never a value the database
    /// refused. Every current Participant receives **their own** updated
    /// [`ConversationSummary`] (their Role and their Unread Count), which the
    /// transport delivers as a `ConversationCreated` event. A User who left or was
    /// removed is not a Participant any more and is therefore not among the
    /// recipients, so a removed member receives no announcement update.
    ///
    /// Clearing is expressed as `None`, and a blank string normalises to it: an
    /// announcement is either a non-empty notice or absent, never `''`.
    pub async fn update_group_announcement(
        &self,
        caller_id: &str,
        conversation_id: &str,
        request: UpdateAnnouncementRequest,
    ) -> Result<MembershipUpdate, ChatError> {
        let (mut conversation, role) =
            require_group_role(&self.repository, conversation_id, caller_id).await?;
        permission::ensure(role, Capability::EditGroupInfo)?;

        let announcement = normalise_announcement(request.announcement)?;

        if !self
            .repository
            .set_announcement(conversation_id, announcement.as_deref())
            .await?
        {
            return Err(ChatError::ConversationNotFound);
        }

        // The recipients' summaries must carry the **new** text, so the in-memory
        // row is brought in step with the write before it is projected.
        conversation.announcement = announcement;

        let members = self.repository.list_members(conversation_id).await?;
        let unread = self
            .repository
            .member_unread_counts(conversation_id)
            .await?;
        let member_count = members.len() as i64;

        let notices = members
            .iter()
            .map(|member| MembershipNotice::Conversation {
                user_id: member.user_id.clone(),
                conversation: group_summary_from(
                    &conversation,
                    Role::from_stored(&member.role),
                    member_count,
                    unread.get(&member.user_id).copied().unwrap_or(0),
                ),
            })
            .collect();

        tracing::info!(
            conversation_id = %conversation_id,
            "group announcement updated"
        );

        Ok(MembershipUpdate {
            conversation_id: conversation_id.to_owned(),
            actor_id: caller_id.to_owned(),
            notices,
        })
    }

    /// Invite Users into a Group Conversation.
    ///
    /// Owner or admin only ([`Capability::InviteMembers`]). A handle that is
    /// already a Participant is a conflict rather than a no-op, so the caller
    /// always knows whether the invite actually added anyone. Every current
    /// Participant (the new ones included) is told who joined; each new member
    /// also gains the Conversation.
    pub async fn add_group_members(
        &self,
        caller_id: &str,
        conversation_id: &str,
        request: AddGroupMembersRequest,
    ) -> Result<MembershipUpdate, ChatError> {
        let (conversation, actor_role) =
            require_group_role(&self.repository, conversation_id, caller_id).await?;
        permission::ensure(actor_role, Capability::InviteMembers)?;

        let usernames = normalise_usernames(&request.member_usernames);
        if usernames.is_empty() {
            return Err(validation(
                "member_usernames",
                FieldErrorCode::Required,
                "请至少选择一位成员",
            ));
        }

        let current_count = self.repository.member_count(conversation_id).await?;
        let mut invited: Vec<PeerRow> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for username in &usernames {
            let user = self
                .repository
                .find_user_by_username(username)
                .await?
                .ok_or(ChatError::UserNotFound)?;
            if user.id == caller_id {
                return Err(validation(
                    "member_usernames",
                    FieldErrorCode::InvalidFormat,
                    "你已经在群聊中",
                ));
            }
            if !seen.insert(user.id.clone()) {
                continue;
            }
            if self
                .repository
                .member_role(conversation_id, &user.id)
                .await?
                .is_some()
            {
                return Err(ChatError::AlreadyMember);
            }
            invited.push(user);
        }

        if current_count + invited.len() as i64 > MAX_GROUP_MEMBERS {
            return Err(validation(
                "member_usernames",
                FieldErrorCode::TooLong,
                "群聊成员数量已达上限",
            ));
        }

        for member in &invited {
            self.repository
                .add_member_with_role(conversation_id, &member.id, Role::Member.as_str())
                .await?;
        }

        // Re-read so each `Joined` carries the database's own `joined_at` rather
        // than a client-side approximation of it.
        let members = self.repository.list_members(conversation_id).await?;
        let everyone: Vec<String> = members.iter().map(|row| row.user_id.clone()).collect();
        let new_count = members.len() as i64;

        let mut notices = Vec::new();
        for member in &invited {
            notices.push(MembershipNotice::Conversation {
                user_id: member.id.clone(),
                conversation: group_summary_from(&conversation, Role::Member, new_count, 0),
            });
        }
        for member in &invited {
            let row = members
                .iter()
                .find(|row| row.user_id == member.id)
                .ok_or(ChatError::Internal)?;
            notices.push(MembershipNotice::Changed {
                user_ids: everyone.clone(),
                change: MembershipChange::Joined {
                    member: member_view(row),
                },
            });
        }

        Ok(MembershipUpdate {
            conversation_id: conversation_id.to_owned(),
            actor_id: caller_id.to_owned(),
            notices,
        })
    }

    /// Remove a Participant from a Group Conversation.
    ///
    /// Owner or admin ([`Capability::RemoveMembers`]), refined by
    /// [`permission::ensure_remove`]: an admin may remove an ordinary member but
    /// not the owner and not another admin. The removed User is told about their
    /// own removal (so their client drops the Conversation) and is no longer in
    /// `participants`, which is what stops the Group's Messages reaching them.
    pub async fn remove_group_member(
        &self,
        caller_id: &str,
        conversation_id: &str,
        member_id: &str,
    ) -> Result<MembershipUpdate, ChatError> {
        let (_, actor_role) =
            require_group_role(&self.repository, conversation_id, caller_id).await?;
        permission::ensure(actor_role, Capability::RemoveMembers)?;

        if member_id == caller_id {
            return Err(validation(
                "user_id",
                FieldErrorCode::InvalidFormat,
                "不能移除自己，请使用退出群聊",
            ));
        }

        let target_role = self
            .repository
            .member_role(conversation_id, member_id)
            .await?
            .ok_or(ChatError::MemberNotFound)?;
        permission::ensure_remove(actor_role, Role::from_stored(&target_role))?;

        if !self
            .repository
            .remove_member(conversation_id, member_id)
            .await?
        {
            return Err(ChatError::MemberNotFound);
        }

        let remaining = self.repository.participants(conversation_id).await?;
        let mut recipients = remaining;
        recipients.push(member_id.to_owned());

        Ok(MembershipUpdate {
            conversation_id: conversation_id.to_owned(),
            actor_id: caller_id.to_owned(),
            notices: vec![MembershipNotice::Changed {
                user_ids: recipients,
                change: MembershipChange::Removed {
                    user_id: member_id.to_owned(),
                },
            }],
        })
    }

    /// Leave a Group Conversation of one's own accord.
    ///
    /// Anyone may leave, except that an owner must not leave Participants behind
    /// ([`permission::ensure_leave`]) — the group would be left with no Role that
    /// can administer it. An owner who is the last Participant simply ends the
    /// group, which is the same dissolution as [dissolving](Self::dissolve_group).
    pub async fn leave_group(
        &self,
        caller_id: &str,
        conversation_id: &str,
    ) -> Result<MembershipUpdate, ChatError> {
        let (_, role) = require_group_role(&self.repository, conversation_id, caller_id).await?;

        let count = self.repository.member_count(conversation_id).await?;
        let others_remaining = count.saturating_sub(1);
        permission::ensure_leave(role, others_remaining)?;

        // The owner may only reach here as the last Participant; leaving then
        // means the group has nobody left, so it is dissolved rather than left
        // as an owner-less shell. Anyone else reaching here is also the last
        // Participant — there is nobody left to administer or to notify.
        if others_remaining <= 0 {
            let participants = self.repository.participants(conversation_id).await?;
            if !self.repository.dissolve(conversation_id).await? {
                return Err(ChatError::ConversationNotFound);
            }
            return Ok(MembershipUpdate {
                conversation_id: conversation_id.to_owned(),
                actor_id: caller_id.to_owned(),
                notices: vec![MembershipNotice::Changed {
                    user_ids: participants,
                    change: MembershipChange::Dissolved,
                }],
            });
        }

        if !self
            .repository
            .remove_member(conversation_id, caller_id)
            .await?
        {
            return Err(ChatError::Internal);
        }

        let remaining = self.repository.participants(conversation_id).await?;
        let mut recipients = remaining;
        recipients.push(caller_id.to_owned());

        Ok(MembershipUpdate {
            conversation_id: conversation_id.to_owned(),
            actor_id: caller_id.to_owned(),
            notices: vec![MembershipNotice::Changed {
                user_ids: recipients,
                change: MembershipChange::Left {
                    user_id: caller_id.to_owned(),
                },
            }],
        })
    }

    /// Promote a member to admin, or demote an admin back to member.
    ///
    /// Owner only ([`Capability::ChangeRoles`]). [`Role::Owner`] is refused as a
    /// target: ownership moves only through
    /// [transfer](Self::transfer_group_ownership), which is a different action
    /// with a different rule.
    pub async fn change_member_role(
        &self,
        caller_id: &str,
        conversation_id: &str,
        member_id: &str,
        request: ChangeMemberRoleRequest,
    ) -> Result<MembershipUpdate, ChatError> {
        let (_, actor_role) =
            require_group_role(&self.repository, conversation_id, caller_id).await?;
        permission::ensure(actor_role, Capability::ChangeRoles)?;

        if request.role == Role::Owner {
            return Err(validation(
                "role",
                FieldErrorCode::InvalidFormat,
                "请使用转让群主来移交群聊",
            ));
        }

        let target_role = self
            .repository
            .member_role(conversation_id, member_id)
            .await?
            .ok_or(ChatError::MemberNotFound)?;
        if Role::from_stored(&target_role) == Role::Owner {
            return Err(ChatError::NotPermitted {
                action: Capability::ChangeRoles.describe(),
            });
        }

        if !self
            .repository
            .set_member_role(conversation_id, member_id, request.role.as_str())
            .await?
        {
            return Err(ChatError::MemberNotFound);
        }

        Ok(MembershipUpdate {
            conversation_id: conversation_id.to_owned(),
            actor_id: caller_id.to_owned(),
            notices: vec![MembershipNotice::Changed {
                user_ids: self.repository.participants(conversation_id).await?,
                change: MembershipChange::RoleChanged {
                    user_id: member_id.to_owned(),
                    role: request.role,
                },
            }],
        })
    }

    /// Hand ownership of a Group Conversation to another Participant.
    ///
    /// Owner only ([`Capability::TransferOwnership`]). The target must currently
    /// be a Participant — transferring to someone who has already left is refused
    /// with [`ChatError::MemberNotFound`]. The outgoing owner becomes an admin and
    /// the target the owner in one transaction, so the permission checks that
    /// follow see the new roles immediately.
    pub async fn transfer_group_ownership(
        &self,
        caller_id: &str,
        conversation_id: &str,
        request: TransferOwnershipRequest,
    ) -> Result<MembershipUpdate, ChatError> {
        let (_, actor_role) =
            require_group_role(&self.repository, conversation_id, caller_id).await?;
        permission::ensure(actor_role, Capability::TransferOwnership)?;

        let target_id = request.user_id;
        if target_id == caller_id {
            return Err(validation(
                "user_id",
                FieldErrorCode::InvalidFormat,
                "不能把群主转让给自己",
            ));
        }

        // A target who is a Participant can still lose the race to leave before
        // the transfer commits; `transfer_ownership` performs the demote/promote
        // pair atomically and is the check that actually matters. This read is
        // what turns "they already left" into a precise [`ChatError::MemberNotFound`]
        // instead of a database conflict.
        self.repository
            .member_role(conversation_id, &target_id)
            .await?
            .ok_or(ChatError::MemberNotFound)?;

        if !self
            .repository
            .transfer_ownership(conversation_id, caller_id, &target_id)
            .await?
        {
            return Err(ChatError::MemberNotFound);
        }

        tracing::info!(
            conversation_id = %conversation_id,
            "group ownership transferred"
        );

        Ok(MembershipUpdate {
            conversation_id: conversation_id.to_owned(),
            actor_id: caller_id.to_owned(),
            notices: vec![MembershipNotice::Changed {
                user_ids: self.repository.participants(conversation_id).await?,
                change: MembershipChange::OwnershipTransferred {
                    from_user_id: caller_id.to_owned(),
                    to_user_id: target_id,
                },
            }],
        })
    }

    /// Dissolve a Group Conversation for everyone.
    ///
    /// Owner only ([`Capability::Dissolve`]). The Conversation row is deleted and
    /// its memberships and Messages go with it through `ON DELETE CASCADE`; every
    /// former Participant is told, so a group disappears from every list rather
    /// than lingering for the people who were in it.
    pub async fn dissolve_group(
        &self,
        caller_id: &str,
        conversation_id: &str,
    ) -> Result<MembershipUpdate, ChatError> {
        let (_, role) = require_group_role(&self.repository, conversation_id, caller_id).await?;
        permission::ensure(role, Capability::Dissolve)?;

        // Capture the audience before the delete: after it there are no rows left
        // to ask, and the people who need to hear about the dissolution are
        // exactly the ones who were in it.
        let participants = self.repository.participants(conversation_id).await?;

        if !self.repository.dissolve(conversation_id).await? {
            return Err(ChatError::ConversationNotFound);
        }

        tracing::info!(
            conversation_id = %conversation_id,
            "group conversation dissolved"
        );

        Ok(MembershipUpdate {
            conversation_id: conversation_id.to_owned(),
            actor_id: caller_id.to_owned(),
            notices: vec![MembershipNotice::Changed {
                user_ids: participants,
                change: MembershipChange::Dissolved,
            }],
        })
    }

    /// One page of a Conversation's history, oldest first.
    ///
    /// The page is a cursor walk on the Sequence Number (ADR-0003), in one of two
    /// directions:
    ///
    /// - **Backwards** (`before`): no cursor reads the newest page, and the
    ///   response's `next_before` reads the page before it. This is the history
    ///   browse.
    /// - **Forwards** (`after`): the oldest Messages newer than the cursor, so a
    ///   client that reconnected can pull exactly what it missed and continue with
    ///   `next_after`. This is the repair path (ADR-0003's reconnect requirement).
    ///
    /// `before` and `after` together is a validation failure: they name opposite
    /// ends of a page and the intent would be ambiguous.
    ///
    /// `limit` is advisory — it is clamped by [`page_size`] so a client cannot pull
    /// the whole Conversation in one request.
    ///
    /// Reading history requires being a Participant: a non-member is refused with
    /// [`ChatError::NotAParticipant`], and a Conversation that does not exist with
    /// [`ChatError::ConversationNotFound`]. The two are deliberately distinct.
    pub async fn list_messages(
        &self,
        caller_id: &str,
        conversation_id: &str,
        page: MessagePageQuery,
    ) -> Result<MessageList, ChatError> {
        if !is_ulid(conversation_id) {
            return Err(ChatError::ConversationNotFound);
        }

        if page.before.is_some() && page.after.is_some() {
            return Err(validation(
                "after",
                FieldErrorCode::InvalidFormat,
                "before 与 after 不能同时使用",
            ));
        }

        require_participant(&self.repository, conversation_id, caller_id).await?;

        let limit = page_size(page.limit);

        // The page also carries the other Participants' public Read Receipts, so a
        // client can render its "read" indicators on first paint rather than only
        // after a live event arrives. The query selects only `read_receipt_seq`:
        // the caller's own private marker is not in this result, and neither is
        // anyone else's.
        let read_receipts = self
            .repository
            .list_receipts(conversation_id, caller_id)
            .await?;

        match page.after {
            Some(after) => {
                let (rows, has_more) = self
                    .repository
                    .list_message_page_after(conversation_id, after, limit)
                    .await?;

                // The continuation cursor is this page's newest Message. It is only
                // meaningful when a probe row proved a newer page exists.
                let next_after = if has_more {
                    rows.last().map(|row| row.seq)
                } else {
                    None
                };

                Ok(MessageList {
                    messages: rows.into_iter().map(message_view).collect(),
                    next_before: None,
                    next_after,
                    has_more,
                    read_receipts,
                })
            }
            None => {
                let (rows, has_more) = self
                    .repository
                    .list_message_page(conversation_id, page.before, limit)
                    .await?;

                // The cursor for the next older page is the Sequence Number of this
                // page's oldest Message. It is only meaningful when a probe row
                // proved an older page exists, so `has_more` gates it.
                let next_before = if has_more {
                    rows.first().map(|row| row.seq)
                } else {
                    None
                };

                Ok(MessageList {
                    messages: rows.into_iter().map(message_view).collect(),
                    next_before,
                    next_after: None,
                    has_more,
                    read_receipts,
                })
            }
        }
    }

    /// Store a text Message, or return the one this idempotency key already wrote.
    ///
    /// Validation rejects an empty or over-long body and a malformed Client Message
    /// ID before any database work; the reply carries the fan-out set so the caller
    /// can push it to every Participant.
    pub async fn send_message(
        &self,
        sender_id: &str,
        send: SendMessage,
    ) -> Result<SentMessage, ChatError> {
        validate_send(&send)?;
        require_participant(&self.repository, &send.conversation_id, sender_id).await?;

        let (row, created) = self
            .repository
            .insert_message_idempotent(&NewMessageRow {
                id: new_id(),
                conversation_id: send.conversation_id.clone(),
                sender_id: sender_id.to_owned(),
                client_msg_id: send.client_msg_id.clone(),
                body: send.body,
            })
            .await?;

        if created {
            tracing::debug!(
                conversation_id = %row.conversation_id,
                seq = row.seq,
                "message stored"
            );
        }

        let participants = self.repository.participants(&row.conversation_id).await?;

        Ok(SentMessage {
            message: message_view(row),
            participants,
            created,
        })
    }

    /// Mark a Conversation read up to a Sequence Number.
    ///
    /// The caller must be a Participant. `mark.last_read_seq` is clamped to the
    /// newest Message that exists and both positions only move forward, so a
    /// replayed or over-eager report is harmless.
    ///
    /// The two positions are returned together but must be fanned out
    /// **separately**: [`ReadStateUpdate::read_marker_seq`] is private and goes to
    /// the reader's own Devices; [`ReadStateUpdate::read_receipt_seq`] is public
    /// and goes to [`ReadStateUpdate::others`]. Keeping both on one step result is
    /// what lets the caller do that without a second read — and keeping them in
    /// two fields is what stops one being sent where the other belongs.
    pub async fn mark_read(
        &self,
        user_id: &str,
        mark: MarkRead,
    ) -> Result<ReadStateUpdate, ChatError> {
        if !is_ulid(&mark.conversation_id) {
            return Err(ChatError::ConversationNotFound);
        }
        if mark.last_read_seq < 0 {
            return Err(validation(
                "last_read_seq",
                FieldErrorCode::InvalidFormat,
                "已读位置不合法",
            ));
        }

        require_participant(&self.repository, &mark.conversation_id, user_id).await?;

        let state = self
            .repository
            .mark_read(&mark.conversation_id, user_id, mark.last_read_seq)
            .await?
            .ok_or(ChatError::Internal)?;

        let others = self
            .repository
            .participants(&mark.conversation_id)
            .await?
            .into_iter()
            .filter(|participant| participant != user_id)
            .collect();

        Ok(ReadStateUpdate {
            conversation_id: mark.conversation_id,
            user_id: user_id.to_owned(),
            read_marker_seq: state.read_marker_seq,
            read_receipt_seq: state.read_receipt_seq,
            unread_count: state.unread_count,
            others,
        })
    }

    /// One User's private read state in a Conversation, for that User alone.
    ///
    /// Not a peer-facing read model — it returns the caller's **own** Read Marker
    /// and Unread Count. A peer's private state is never read here.
    pub async fn read_state(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ReadState>, ChatError> {
        if !is_ulid(conversation_id) {
            return Ok(None);
        }

        Ok(self
            .repository
            .read_state(conversation_id, user_id)
            .await?
            .map(read_state))
    }

    /// The other Participants' public Read Receipts for a Conversation.
    ///
    /// Requires the caller to be a Participant; the caller's own receipt is
    /// excluded, and a private Read Marker can never appear here (the query reads
    /// only `read_receipt_seq`).
    pub async fn list_receipts(
        &self,
        caller_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<ReadReceipt>, ChatError> {
        if !is_ulid(conversation_id) {
            return Err(ChatError::ConversationNotFound);
        }

        require_participant(&self.repository, conversation_id, caller_id).await?;

        self.repository
            .list_receipts(conversation_id, caller_id)
            .await
    }

    /// The Users who share a Conversation with `user_id` — the audience for that
    /// User's presence.
    ///
    /// Presence is not chat state, so this returns a **relationship**, not a
    /// presence: it answers "who has a reason to care" and leaves the decision of
    /// what to send, and the sending, to `jiuyue-realtime`. That split is the same
    /// one [`SentMessage::participants`] draws for Message fan-out, and it is the
    /// seam issue #27 narrows when "who can see my presence" becomes a setting.
    ///
    /// `candidates` narrows the answer to a caller-supplied set; `None` returns the
    /// whole audience. A User with no Conversations has an empty audience, which is
    /// a normal answer, not a failure.
    pub async fn presence_audience(
        &self,
        user_id: &str,
        candidates: Option<&[String]>,
    ) -> Result<Vec<String>, ChatError> {
        self.repository.presence_audience(user_id, candidates).await
    }

    /// The **other** Participants of one Conversation — the audience for a
    /// per-Conversation ephemeral hint such as a Typing Indicator.
    ///
    /// This is the same relationship answer as [`Self::presence_audience`], scoped
    /// to a single Conversation and with the caller already removed: it is
    /// deliberately *not* the caller's whole social graph, so typing in one
    /// Conversation can never reach a peer the sender shares a different
    /// Conversation with. The caller must be a Participant; a stranger's signal is
    /// refused with [`ChatError::NotAParticipant`] (or
    /// [`ChatError::ConversationNotFound`]) and therefore reaches nobody.
    ///
    /// Like the other recipient-set reads, it carries no payload and knows nothing
    /// about the transport: `jiuyue-realtime` decides what to send and how.
    pub async fn conversation_audience(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<String>, ChatError> {
        if !is_ulid(conversation_id) {
            return Err(ChatError::ConversationNotFound);
        }

        require_participant(&self.repository, conversation_id, user_id).await?;

        Ok(self
            .repository
            .participants(conversation_id)
            .await?
            .into_iter()
            .filter(|participant| participant != user_id)
            .collect())
    }

    /// The Device's stored Sync Cursors, most recently advanced first.
    ///
    /// `device_id` is a `sessions` row id — the Device identity CONTEXT.md draws —
    /// not a `user_id`: two Devices of one account hold independent positions, and
    /// conflating them would let a phone's progress hide a laptop's gap.
    pub async fn list_sync_cursors(&self, device_id: &str) -> Result<Vec<SyncCursor>, ChatError> {
        self.repository
            .list_sync_cursors(device_id, MAX_SYNC_CURSORS)
            .await
    }

    /// Checkpoint a batch of a Device's consumed positions.
    ///
    /// The batch is normalised first (see [`normalise_cursors`]): reporting is
    /// advisory state for one Device, so malformed or duplicate entries are
    /// discarded rather than turned into a connection failure. What survives is
    /// persisted monotonically, so a replayed report cannot rewind a cursor.
    pub async fn save_sync_cursors(
        &self,
        device_id: &str,
        cursors: &[SyncCursor],
    ) -> Result<(), ChatError> {
        let normalised = normalise_cursors(cursors);
        if normalised.is_empty() {
            return Ok(());
        }

        self.repository
            .upsert_sync_cursors(device_id, &normalised)
            .await
    }
}

/// Keep only the well-formed, highest cursor per Conversation.
///
/// Three things are dropped, each for a concrete reason:
///
/// - a Conversation id that is not a ULID cannot name a row;
/// - a `last_seq` below 1 is the "no row" value (see the schema's
///   `sync_cursors_last_seq_positive`) and would be silently wrong;
/// - a repeated Conversation would make the single `INSERT ... ON CONFLICT DO
///   UPDATE` touch one row twice, which PostgreSQL rejects outright, so the
///   batch must collapse to one entry per Conversation before it reaches SQL.
///
/// The surviving entry keeps the **highest** value: cursors only move forward.
fn normalise_cursors(cursors: &[SyncCursor]) -> Vec<SyncCursor> {
    let mut highest: HashMap<&str, i64> = HashMap::new();

    for cursor in cursors {
        if !is_ulid(&cursor.conversation_id) || cursor.last_seq < 1 {
            continue;
        }
        highest
            .entry(cursor.conversation_id.as_str())
            .and_modify(|value| *value = (*value).max(cursor.last_seq))
            .or_insert(cursor.last_seq);
    }

    highest
        .into_iter()
        .map(|(conversation_id, last_seq)| SyncCursor {
            conversation_id: conversation_id.to_owned(),
            last_seq,
        })
        .collect()
}

/// Refuse the caller unless the Conversation exists and they are a Participant.
async fn require_participant(
    repository: &ChatRepository,
    conversation_id: &str,
    user_id: &str,
) -> Result<(), ChatError> {
    match repository.membership(conversation_id, user_id).await? {
        None => Err(ChatError::ConversationNotFound),
        Some(true) => Ok(()),
        Some(false) => Err(ChatError::NotAParticipant),
    }
}

/// The effective page size for a history request.
///
/// A missing `limit` takes the default and an out-of-range one is clamped to
/// `[1, MAX_MESSAGE_PAGE_SIZE]` — deliberately not rejected, so an over-eager
/// client still gets a useful page instead of an error, but never more rows than
/// the box can afford to materialise.
fn page_size(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(DEFAULT_MESSAGE_PAGE_SIZE)
        .clamp(1, MAX_MESSAGE_PAGE_SIZE)
}

/// Check the fields of a send before any database work.
fn validate_send(send: &SendMessage) -> Result<(), ChatError> {
    if !is_ulid(&send.conversation_id) {
        return Err(validation(
            "conversation_id",
            FieldErrorCode::InvalidFormat,
            "会话标识不合法",
        ));
    }

    if send.client_msg_id.is_empty() {
        return Err(validation(
            "client_msg_id",
            FieldErrorCode::Required,
            "缺少客户端消息标识",
        ));
    }
    if send.client_msg_id.len() > MAX_CLIENT_MSG_ID_BYTES {
        return Err(validation(
            "client_msg_id",
            FieldErrorCode::TooLong,
            "客户端消息标识过长",
        ));
    }

    let count = send.body.chars().count();
    if send.body.trim().is_empty() {
        return Err(validation(
            "body",
            FieldErrorCode::Required,
            "消息内容不能为空",
        ));
    }
    if count > MAX_MESSAGE_BODY_CHARS {
        return Err(validation(
            "body",
            FieldErrorCode::TooLong,
            "消息内容最多 4000 个字符",
        ));
    }

    Ok(())
}

/// The canonical key for an unordered pair of Users.
///
/// Both participants must compute the same string, so the ULIDs are sorted. The
/// schema's `conversations_direct_key_canonical` check refuses the other order,
/// which is what turns the UNIQUE constraint into "one row per pair".
fn canonical_direct_key(first: &str, second: &str) -> String {
    if first <= second {
        format!("{first}:{second}")
    } else {
        format!("{second}:{first}")
    }
}

/// Whether a string is a well-formed ULID.
///
/// 26 characters of Crockford Base32 (case-insensitive; `I`, `L`, `O`, `U` never
/// appear), matching the CHECK constraints on every id column.
fn is_ulid(value: &str) -> bool {
    value.len() == 26
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() && !matches!(c.to_ascii_uppercase(), 'I' | 'L' | 'O' | 'U')
        })
}

/// A fresh ULID, matching the `users` / `sessions` conventions.
fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Project a stored Conversation and the viewer's peer onto the list shape.
fn summary_from(
    conversation: &ConversationRow,
    peer: &PeerRow,
    unread_count: i64,
) -> ConversationSummary {
    ConversationSummary {
        id: conversation.id.clone(),
        kind: kind_from(&conversation.kind),
        peer: Some(PeerSummary {
            id: peer.id.clone(),
            username: peer.username.clone(),
            display_name: peer.display_name.clone(),
            avatar_url: peer.avatar_url.clone(),
        }),
        group: None,
        unread_count,
        created_at_ms: unix_millis(conversation.created_at),
    }
}

/// Project a stored Group Conversation onto the list shape.
///
/// `my_role` is the **viewer's** Role, which is why the same Conversation projects
/// to a different summary for each Participant: the group is shared, the caller's
/// standing in it is not.
fn group_summary_from(
    conversation: &ConversationRow,
    my_role: Role,
    member_count: i64,
    unread_count: i64,
) -> ConversationSummary {
    ConversationSummary {
        id: conversation.id.clone(),
        kind: ConversationKind::Group,
        peer: None,
        group: Some(GroupSummary {
            // A group row always has a title (the schema CHECK requires it); the
            // empty fallback is unreachable, not a real state.
            title: conversation.title.clone().unwrap_or_default(),
            member_count,
            my_role,
            // An announcement is optional: `None` means the group has none, which
            // is a normal state and not a missing column.
            announcement: conversation.announcement.clone(),
        }),
        unread_count,
        created_at_ms: unix_millis(conversation.created_at),
    }
}

/// Project a stored member row onto the wire shape.
fn member_view(row: &MemberRow) -> MemberView {
    MemberView {
        user_id: row.user_id.clone(),
        username: row.username.clone(),
        display_name: row.display_name.clone(),
        avatar_url: row.avatar_url.clone(),
        role: Role::from_stored(&row.role),
        joined_at_ms: unix_millis(row.joined_at),
    }
}

/// Resolve a Conversation to the caller's Role, refusing non-groups and
/// non-members.
///
/// Membership is checked **before** the kind, so a non-participant cannot use a
/// `NotAGroup` answer to learn that a Conversation they are not in exists. The
/// two failures stay distinct: [`ChatError::NotAParticipant`] for someone outside
/// the Conversation, [`ChatError::NotAGroup`] for a Participant of a Direct one
/// who asked a Group question.
async fn require_group_role(
    repository: &ChatRepository,
    conversation_id: &str,
    user_id: &str,
) -> Result<(ConversationRow, Role), ChatError> {
    if !is_ulid(conversation_id) {
        return Err(ChatError::ConversationNotFound);
    }

    let conversation = repository
        .conversation(conversation_id)
        .await?
        .ok_or(ChatError::ConversationNotFound)?;

    let role = repository
        .member_role(conversation_id, user_id)
        .await?
        .ok_or(ChatError::NotAParticipant)?;

    if conversation.kind != "group" {
        return Err(ChatError::NotAGroup);
    }

    Ok((conversation, Role::from_stored(&role)))
}

/// Normalise a requested announcement to the one stored shape.
///
/// Three outcomes, and no fourth:
///
/// - absent, blank or whitespace-only → `None` (clear it); this is why "delete the
///   announcement" has a single representation, SQL `NULL`, and never `''`;
/// - 1–[`MAX_ANNOUNCEMENT_CHARS`] characters → `Some` with the surrounding
///   whitespace trimmed;
/// - longer → a field-level validation failure, refused **before** the write, so
///   an over-length announcement stores nothing.
fn normalise_announcement(announcement: Option<String>) -> Result<Option<String>, ChatError> {
    let trimmed = announcement.unwrap_or_default();
    let trimmed = trimmed.trim();

    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_ANNOUNCEMENT_CHARS {
        return Err(validation(
            "announcement",
            FieldErrorCode::TooLong,
            "群公告最多 2000 个字符",
        ));
    }

    Ok(Some(trimmed.to_owned()))
}

/// Trim, lowercase, drop blanks, and drop repeats — keeping first-seen order.
///
/// `@handle`s are stored lowercase, so normalising here means a caller can type
/// `Bob` and still match. Repeats are dropped rather than refused: a client that
/// sends the same handle twice asked for one invite, not an error.
fn normalise_usernames(input: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();

    input
        .iter()
        .map(|username| username.trim().to_lowercase())
        .filter(|username| !username.is_empty() && seen.insert(username.clone()))
        .collect()
}

/// Project a repository read-state row onto the service shape.
fn read_state(row: ReadStateRow) -> ReadState {
    ReadState {
        read_marker_seq: row.read_marker_seq,
        read_receipt_seq: row.read_receipt_seq,
        unread_count: row.unread_count,
    }
}

/// Map the stored `kind` text onto the wire enum.
///
/// The `conversations_kind` check admits only `direct` and `group`, so the
/// fallback arm is `direct` by construction.
fn kind_from(kind: &str) -> ConversationKind {
    match kind {
        "group" => ConversationKind::Group,
        _ => ConversationKind::Direct,
    }
}

/// Project a stored Message onto the wire shape.
fn message_view(row: MessageRow) -> MessageView {
    MessageView {
        id: row.id,
        conversation_id: row.conversation_id,
        seq: row.seq,
        sender_id: row.sender_id,
        client_msg_id: row.client_msg_id,
        body: row.body,
        created_at_ms: unix_millis(row.created_at),
    }
}

/// Build a one-field validation failure.
fn validation(field: &str, code: FieldErrorCode, message: &str) -> ChatError {
    ChatError::Validation(vec![FieldError {
        field: field.to_owned(),
        code,
        message: message.to_owned(),
    }])
}

/// Milliseconds since the Unix epoch, in the range JavaScript can represent.
fn unix_millis(value: OffsetDateTime) -> i64 {
    value
        .unix_timestamp()
        .saturating_mul(1000)
        .saturating_add(i64::from(value.millisecond()))
}

#[cfg(test)]
mod tests {
    use sqlx::types::time::OffsetDateTime;

    use super::{
        canonical_direct_key, is_ulid, kind_from, normalise_announcement, normalise_cursors,
        page_size, unix_millis,
    };
    use crate::error::ChatError;
    use jiuyue_contract::group::MAX_ANNOUNCEMENT_CHARS;
    use jiuyue_contract::{
        ConversationKind, DEFAULT_MESSAGE_PAGE_SIZE, FieldErrorCode, MAX_MESSAGE_PAGE_SIZE,
        SyncCursor,
    };

    #[test]
    fn the_pair_key_does_not_depend_on_argument_order() {
        let first =
            canonical_direct_key("01JABC1234567890ABCDEFGHJ1", "01JABC1234567890ABCDEFGHJ2");
        let second =
            canonical_direct_key("01JABC1234567890ABCDEFGHJ2", "01JABC1234567890ABCDEFGHJ1");

        assert_eq!(first, second);
        assert_eq!(
            first,
            "01JABC1234567890ABCDEFGHJ1:01JABC1234567890ABCDEFGHJ2"
        );
    }

    #[test]
    fn a_ulid_is_twenty_six_crockford_characters() {
        assert!(is_ulid("01JABC1234567890ABCDEFGHJ1"));
        assert!(!is_ulid("01JABC1234567890ABCDEFGHJ"), "25 characters");
        assert!(!is_ulid("01JABC1234567890ABCDEFGHJ11"), "27 characters");
        assert!(!is_ulid("01JABC1234567890ABCDEFGHI1"), "contains I");
        assert!(!is_ulid("01JABC1234567890ABCDEFGHO1"), "contains O");
        assert!(!is_ulid("01JABC1234567890ABCDEFGH-1"), "not alphanumeric");
    }

    #[test]
    fn the_stored_kind_maps_onto_the_wire_enum() {
        assert_eq!(kind_from("direct"), ConversationKind::Direct);
        assert_eq!(kind_from("group"), ConversationKind::Group);
    }

    #[test]
    fn a_page_size_is_defaulted_when_missing_and_clamped_when_absurd() {
        assert_eq!(page_size(None), DEFAULT_MESSAGE_PAGE_SIZE);
        assert_eq!(page_size(Some(10)), 10);
        assert_eq!(page_size(Some(0)), 1, "zero clamps up to one row");
        assert_eq!(page_size(Some(-25)), 1, "a negative limit clamps up to one");
        assert_eq!(
            page_size(Some(1_000_000)),
            MAX_MESSAGE_PAGE_SIZE,
            "an absurd limit is clamped, never honoured"
        );
        assert_eq!(page_size(Some(i64::MAX)), MAX_MESSAGE_PAGE_SIZE);
    }

    #[test]
    fn timestamps_become_unix_milliseconds() {
        let instant = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("a valid fixed timestamp")
            .replace_millisecond(250)
            .expect("a valid millisecond");

        assert_eq!(unix_millis(instant), 1_700_000_000_250);
    }

    /// A cursor naming `conversation_id` at `last_seq`.
    fn cursor(conversation_id: &str, last_seq: i64) -> SyncCursor {
        SyncCursor {
            conversation_id: conversation_id.to_owned(),
            last_seq,
        }
    }

    #[test]
    fn cursors_collapse_to_the_highest_per_conversation() {
        const FIRST: &str = "01JABC1234567890ABCDEFGHJ1";
        const SECOND: &str = "01JABC1234567890ABCDEFGHJ2";

        // Out-of-order reports — several connections of one Device — must not
        // rewind, and the duplicate that would break `ON CONFLICT` is gone.
        let normalised = normalise_cursors(&[
            cursor(FIRST, 9),
            cursor(SECOND, 4),
            cursor(FIRST, 3),
            cursor(FIRST, 12),
        ]);

        assert_eq!(normalised.len(), 2, "one entry per conversation, never two");
        let mut by_id: Vec<(&str, i64)> = normalised
            .iter()
            .map(|cursor| (cursor.conversation_id.as_str(), cursor.last_seq))
            .collect();
        by_id.sort_unstable();
        assert_eq!(by_id, vec![(FIRST, 12), (SECOND, 4)]);
    }

    #[test]
    fn malformed_cursors_are_dropped_rather_than_failing_the_batch() {
        let normalised = normalise_cursors(&[
            cursor("not-a-ulid", 5),
            cursor("01JABC1234567890ABCDEFGHI1", 5), // `I` is not Crockford Base32
            cursor("01JABC1234567890ABCDEFGHJ1", 5),
        ]);

        assert_eq!(normalised.len(), 1);
        assert_eq!(normalised[0].last_seq, 5);
    }

    #[test]
    fn a_non_positive_cursor_is_dropped() {
        let normalised = normalise_cursors(&[
            cursor("01JABC1234567890ABCDEFGHJ1", 0),
            cursor("01JABC1234567890ABCDEFGHJ2", -7),
        ]);

        assert!(
            normalised.is_empty(),
            "0 is the schema's `no row` value and must never be stored"
        );
    }

    #[test]
    fn an_announcement_is_trimmed_and_absence_or_blank_clears_it() {
        assert_eq!(
            normalise_announcement(None).expect("absence clears"),
            None,
            "an absent announcement means 'clear it'"
        );
        assert_eq!(
            normalise_announcement(Some("   ".to_owned())).expect("blank clears"),
            None,
            "whitespace-only is a clear, never a stored empty string"
        );
        assert_eq!(
            normalise_announcement(Some("  周六开会  ".to_owned())).expect("valid"),
            Some("周六开会".to_owned())
        );
    }

    #[test]
    fn an_announcement_at_the_cap_is_accepted_and_one_over_is_refused() {
        let at_cap = "九".repeat(MAX_ANNOUNCEMENT_CHARS);
        assert_eq!(
            normalise_announcement(Some(at_cap)).expect("the cap itself is allowed"),
            Some("九".repeat(MAX_ANNOUNCEMENT_CHARS))
        );

        let over = "九".repeat(MAX_ANNOUNCEMENT_CHARS + 1);
        let error = normalise_announcement(Some(over)).expect_err("one over the cap is refused");

        match error {
            ChatError::Validation(fields) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].field, "announcement");
                assert_eq!(fields[0].code, FieldErrorCode::TooLong);
            }
            other => panic!("expected a validation failure, got {other:?}"),
        }
    }
}
