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

use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;

use jiuyue_contract::{
    ConversationKind, ConversationSummary, DEFAULT_MESSAGE_PAGE_SIZE, FieldError, FieldErrorCode,
    MAX_CLIENT_MSG_ID_BYTES, MAX_MESSAGE_BODY_CHARS, MAX_MESSAGE_PAGE_SIZE, MessageList,
    MessagePageQuery, MessageView, PeerSummary, SendMessage,
};

use crate::error::ChatError;
use crate::repository::{ChatRepository, ConversationRow, MessageRow, NewMessageRow, PeerRow};

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

        let caller_view = summary_from(&conversation, &peer);
        let peer_view = summary_from(&conversation, &caller);
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

    /// Every Direct Conversation the caller participates in, newest first.
    pub async fn list_conversations(
        &self,
        caller_id: &str,
    ) -> Result<Vec<ConversationSummary>, ChatError> {
        Ok(self
            .repository
            .list_conversations(caller_id)
            .await?
            .into_iter()
            .map(|(conversation, peer)| summary_from(&conversation, &peer))
            .collect())
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
fn summary_from(conversation: &ConversationRow, peer: &PeerRow) -> ConversationSummary {
    ConversationSummary {
        id: conversation.id.clone(),
        kind: kind_from(&conversation.kind),
        peer: Some(PeerSummary {
            id: peer.id.clone(),
            username: peer.username.clone(),
            display_name: peer.display_name.clone(),
            avatar_url: peer.avatar_url.clone(),
        }),
        created_at_ms: unix_millis(conversation.created_at),
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

    use super::{canonical_direct_key, is_ulid, kind_from, page_size, unix_millis};
    use jiuyue_contract::{ConversationKind, DEFAULT_MESSAGE_PAGE_SIZE, MAX_MESSAGE_PAGE_SIZE};

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
}
