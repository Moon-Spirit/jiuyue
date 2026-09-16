//! SQL for the chat tables.
//!
//! `conversations`, `conversation_members` and `messages` are the tables this
//! domain owns (ADR-0011); the SQL is here and nowhere else. Queries use the
//! runtime-checked `sqlx::query` API, not the compile-time macros, so building the
//! crate never needs a database — consistent with `jiuyue-auth` and `jiuyue-store`.
//!
//! Two statements in this module carry most of the correctness load:
//!
//! - [`ChatRepository::insert_direct`] uses `ON CONFLICT (direct_key) DO NOTHING`
//!   so two Users opening the same chat concurrently cannot create two rows.
//! - [`ChatRepository::insert_message_idempotent`] allocates the Sequence Number
//!   and writes the row in one transaction, and rolls the allocation back when the
//!   send turns out to be a replay. That is what keeps `seq` gapless *and* keeps a
//!   retry from writing twice.

use sqlx::postgres::PgRow;
use sqlx::types::time::OffsetDateTime;
use sqlx::{Executor, PgPool, Postgres, Row};

use crate::error::ChatError;

const INSERT_DIRECT_CONVERSATION: &str = "\
    INSERT INTO conversations (id, kind, direct_key) VALUES ($1, 'direct', $2) \
    ON CONFLICT (direct_key) DO NOTHING \
    RETURNING id::text AS id, kind, created_at";

const SELECT_CONVERSATION_BY_DIRECT_KEY: &str = "\
    SELECT id::text AS id, kind, created_at \
    FROM conversations WHERE direct_key = $1";

const INSERT_MEMBERS: &str = "\
    INSERT INTO conversation_members (conversation_id, user_id) \
    SELECT $1, member.user_id FROM unnest($2::text[]) AS member(user_id) \
    ON CONFLICT (conversation_id, user_id) DO NOTHING";

const SELECT_USER_BY_USERNAME: &str = "\
    SELECT id::text AS id, username, display_name, avatar_url \
    FROM users WHERE username = $1";

const SELECT_USER_BY_ID: &str = "\
    SELECT id::text AS id, username, display_name, avatar_url \
    FROM users WHERE id = $1";

const SELECT_CONVERSATIONS_FOR_USER: &str = "\
    SELECT c.id::text AS id, c.kind, c.created_at, \
           u.id::text AS peer_id, u.username AS peer_username, \
           u.display_name AS peer_display_name, u.avatar_url AS peer_avatar_url \
    FROM conversation_members AS me \
    JOIN conversations AS c ON c.id = me.conversation_id \
    JOIN conversation_members AS peer_member \
      ON peer_member.conversation_id = c.id AND peer_member.user_id <> me.user_id \
    JOIN users AS u ON u.id = peer_member.user_id \
    WHERE me.user_id = $1 AND c.kind = 'direct' \
    ORDER BY c.created_at DESC";

const SELECT_MEMBERSHIP: &str = "\
    SELECT EXISTS ( \
        SELECT 1 FROM conversation_members AS m \
        WHERE m.conversation_id = c.id AND m.user_id = $2 \
    ) AS is_member \
    FROM conversations AS c WHERE c.id = $1";

const SELECT_PARTICIPANTS: &str = "\
    SELECT user_id::text AS user_id \
    FROM conversation_members WHERE conversation_id = $1 ORDER BY joined_at";

const ALLOCATE_SEQ: &str = "\
    UPDATE conversations SET next_seq = next_seq + 1 \
    WHERE id = $1 RETURNING next_seq";

const INSERT_MESSAGE: &str = "\
    INSERT INTO messages (id, conversation_id, seq, sender_id, client_msg_id, body) \
    VALUES ($1, $2, $3, $4, $5, $6) \
    ON CONFLICT (sender_id, client_msg_id) DO NOTHING \
    RETURNING id::text AS id, conversation_id::text AS conversation_id, seq, \
              sender_id::text AS sender_id, client_msg_id, body, created_at";

const MESSAGE_COLUMNS: &str = "\
    id::text AS id, conversation_id::text AS conversation_id, seq, \
    sender_id::text AS sender_id, client_msg_id, body, created_at";

/// One page of history, walking backwards from an exclusive `seq` cursor.
///
/// The inner `ORDER BY seq DESC LIMIT` is served by the `messages_conversation_seq_key`
/// index (its leading columns are exactly `(conversation_id, seq)`), so the range
/// scan never materialises the Conversation. The outer `ORDER BY seq ASC` sorts
/// only the page's rows — `limit + 1` of them — never the whole Conversation.
const SELECT_MESSAGE_PAGE_BEFORE: &str = "\
    SELECT {columns} FROM ( \
        SELECT {columns} FROM messages \
        WHERE conversation_id = $1 AND seq < $2 \
        ORDER BY seq DESC LIMIT $3 \
    ) AS page ORDER BY seq ASC";

/// The newest page, for a request that carries no cursor.
const SELECT_MESSAGE_PAGE_LATEST: &str = "\
    SELECT {columns} FROM ( \
        SELECT {columns} FROM messages \
        WHERE conversation_id = $1 \
        ORDER BY seq DESC LIMIT $2 \
    ) AS page ORDER BY seq ASC";

/// One page of history, walking forwards from an exclusive `seq` cursor.
///
/// This is the repair direction (ADR-0003): the **oldest** Messages with
/// `seq > after`, ascending, so a client that holds everything up to a cursor can
/// pull exactly what it missed. The same `(conversation_id, seq)` index serves it
/// as an ascending range scan, and the `limit + 1` probe row answers "is there
/// anything newer?" without a second query.
const SELECT_MESSAGE_PAGE_AFTER: &str = "\
    SELECT {columns} FROM ( \
        SELECT {columns} FROM messages \
        WHERE conversation_id = $1 AND seq > $2 \
        ORDER BY seq ASC LIMIT $3 \
    ) AS page ORDER BY seq ASC";

/// Fill a page query's column list in, so the projection is stated once.
fn page_query(template: &str) -> String {
    template.replace("{columns}", MESSAGE_COLUMNS)
}

const SELECT_MESSAGE_BY_CLIENT_ID: &str = "\
    SELECT id::text AS id, conversation_id::text AS conversation_id, seq, \
           sender_id::text AS sender_id, client_msg_id, body, created_at \
    FROM messages WHERE sender_id = $1 AND client_msg_id = $2";

/// A row of `conversations`, as this module needs it.
#[derive(Debug, Clone)]
pub struct ConversationRow {
    /// ULID.
    pub id: String,
    /// `direct` or `group`.
    pub kind: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// The public columns of another User, for rendering a Direct Conversation.
#[derive(Debug, Clone)]
pub struct PeerRow {
    /// ULID.
    pub id: String,
    /// `@handle`.
    pub username: String,
    /// Display name.
    pub display_name: String,
    /// Avatar URL, when set.
    pub avatar_url: Option<String>,
}

/// A row of `messages`.
#[derive(Debug, Clone)]
pub struct MessageRow {
    /// Global Message ID (ULID).
    pub id: String,
    /// Owning Conversation.
    pub conversation_id: String,
    /// Per-Conversation Sequence Number.
    pub seq: i64,
    /// Sending User.
    pub sender_id: String,
    /// Sender's idempotency key.
    pub client_msg_id: String,
    /// Message text.
    pub body: String,
    /// Server creation time.
    pub created_at: OffsetDateTime,
}

/// Everything needed to insert a Message, except the Sequence Number.
#[derive(Debug, Clone)]
pub struct NewMessageRow {
    /// Application-generated ULID.
    pub id: String,
    /// Owning Conversation.
    pub conversation_id: String,
    /// Sending User.
    pub sender_id: String,
    /// Sender's idempotency key.
    pub client_msg_id: String,
    /// Message text.
    pub body: String,
}

/// Data access for the chat domain.
#[derive(Debug, Clone)]
pub struct ChatRepository {
    pool: PgPool,
}

impl ChatRepository {
    /// Wrap a pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a Direct Conversation, or return `None` when the pair already has one.
    ///
    /// The UNIQUE constraint on `direct_key` is what makes concurrent opens safe:
    /// the loser of the race inserts nothing and reads the winner's row instead.
    pub async fn insert_direct(
        &self,
        id: &str,
        direct_key: &str,
    ) -> Result<Option<ConversationRow>, ChatError> {
        sqlx::query(INSERT_DIRECT_CONVERSATION)
            .bind(id)
            .bind(direct_key)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| conversation_from_row(&row)))
            .map_err(ChatError::Database)
    }

    /// Read the Direct Conversation carrying this pair key, if any.
    pub async fn find_by_direct_key(
        &self,
        direct_key: &str,
    ) -> Result<Option<ConversationRow>, ChatError> {
        sqlx::query(SELECT_CONVERSATION_BY_DIRECT_KEY)
            .bind(direct_key)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| conversation_from_row(&row)))
            .map_err(ChatError::Database)
    }

    /// Add Participants to a Conversation, ignoring ones already present.
    pub async fn add_members(
        &self,
        conversation_id: &str,
        user_ids: &[String],
    ) -> Result<(), ChatError> {
        sqlx::query(INSERT_MEMBERS)
            .bind(conversation_id)
            .bind(user_ids)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(ChatError::Database)
    }

    /// Look a User up by their normalised `@handle`.
    pub async fn find_user_by_username(
        &self,
        username: &str,
    ) -> Result<Option<PeerRow>, ChatError> {
        sqlx::query(SELECT_USER_BY_USERNAME)
            .bind(username)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| peer_from_row(&row)))
            .map_err(ChatError::Database)
    }

    /// Look a User up by their ULID.
    pub async fn find_user_by_id(&self, user_id: &str) -> Result<Option<PeerRow>, ChatError> {
        sqlx::query(SELECT_USER_BY_ID)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| peer_from_row(&row)))
            .map_err(ChatError::Database)
    }

    /// Every Direct Conversation the User participates in, newest first.
    ///
    /// Group Conversations are excluded until the group ticket defines what a
    /// group summary renders; the query is deliberately not written to half-guess it.
    pub async fn list_conversations(
        &self,
        user_id: &str,
    ) -> Result<Vec<(ConversationRow, PeerRow)>, ChatError> {
        sqlx::query(SELECT_CONVERSATIONS_FOR_USER)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|row| (conversation_from_row(row), peer_from_join_row(row)))
                    .collect()
            })
            .map_err(ChatError::Database)
    }

    /// Whether a Conversation exists, and whether the caller belongs to it.
    ///
    /// `None` means the Conversation does not exist; `Some(false)` means it does
    /// but the caller is not a Participant. The two are different answers and the
    /// domain must not merge them.
    pub async fn membership(
        &self,
        conversation_id: &str,
        user_id: &str,
    ) -> Result<Option<bool>, ChatError> {
        sqlx::query(SELECT_MEMBERSHIP)
            .bind(conversation_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| row.get::<bool, _>("is_member")))
            .map_err(ChatError::Database)
    }

    /// Every Participant of a Conversation, in join order.
    pub async fn participants(&self, conversation_id: &str) -> Result<Vec<String>, ChatError> {
        sqlx::query(SELECT_PARTICIPANTS)
            .bind(conversation_id)
            .fetch_all(&self.pool)
            .await
            .map(|rows| rows.iter().map(|row| row.get("user_id")).collect())
            .map_err(ChatError::Database)
    }

    /// Store a Message, or return the one this idempotency key already wrote.
    ///
    /// Returns `(row, created)`: `created` is false when the send was a replay.
    ///
    /// The Sequence Number and the row are one unit of work. A replay detected
    /// after the allocation is resolved by *rolling the transaction back*, which
    /// undoes the `next_seq` increment — otherwise every retried send would punch a
    /// hole in the Conversation's ordering.
    pub async fn insert_message_idempotent(
        &self,
        message: &NewMessageRow,
    ) -> Result<(MessageRow, bool), ChatError> {
        let mut transaction = self.pool.begin().await.map_err(ChatError::Database)?;

        // Fast path: a sequential retry (the common case) never touches next_seq.
        if let Some(existing) = find_message_by_client_id(
            &mut *transaction,
            &message.sender_id,
            &message.client_msg_id,
        )
        .await?
        {
            transaction.commit().await.map_err(ChatError::Database)?;
            return Ok((existing, false));
        }

        let seq = sqlx::query(ALLOCATE_SEQ)
            .bind(&message.conversation_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(ChatError::Database)?
            .map(|row| row.get::<i64, _>("next_seq"))
            .ok_or(ChatError::ConversationNotFound)?;

        let inserted = sqlx::query(INSERT_MESSAGE)
            .bind(&message.id)
            .bind(&message.conversation_id)
            .bind(seq)
            .bind(&message.sender_id)
            .bind(&message.client_msg_id)
            .bind(&message.body)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(ChatError::Database)?;

        match inserted {
            Some(row) => {
                transaction.commit().await.map_err(ChatError::Database)?;
                Ok((message_from_row(&row), true))
            }
            None => {
                // A concurrent send with the same idempotency key won the insert.
                // Rolling back here is what returns the allocated `seq` to the pool.
                transaction.rollback().await.map_err(ChatError::Database)?;

                let existing = find_message_by_client_id(
                    &self.pool,
                    &message.sender_id,
                    &message.client_msg_id,
                )
                .await?
                .ok_or(ChatError::Internal)?;

                Ok((existing, false))
            }
        }
    }

    /// One page of a Conversation's history, oldest first.
    ///
    /// `before` is an exclusive Sequence Number cursor (`None` reads the newest
    /// page). Returns `(rows, has_more)`: the query reads `limit + 1` rows so that
    /// the presence of an extra, older row is the answer to "is there more?" —
    /// which is then dropped, leaving exactly one stable page.
    ///
    /// The page is selected and ordered by the `(conversation_id, seq)` index, so
    /// it neither scans nor sorts the Conversation.
    pub async fn list_message_page(
        &self,
        conversation_id: &str,
        before: Option<i64>,
        limit: i64,
    ) -> Result<(Vec<MessageRow>, bool), ChatError> {
        let probe = limit.saturating_add(1);

        let rows = match before {
            Some(cursor) => {
                sqlx::query(&page_query(SELECT_MESSAGE_PAGE_BEFORE))
                    .bind(conversation_id)
                    .bind(cursor)
                    .bind(probe)
                    .fetch_all(&self.pool)
                    .await
            }
            None => {
                sqlx::query(&page_query(SELECT_MESSAGE_PAGE_LATEST))
                    .bind(conversation_id)
                    .bind(probe)
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(ChatError::Database)?;

        let mut messages: Vec<MessageRow> = rows.iter().map(message_from_row).collect();
        // The probe row is the oldest of the batch; it exists only to answer
        // "has_more". Nothing about the cursor changes on a later page, which is
        // the whole point of cursor pagination.
        let has_more = messages.len() as i64 > limit;
        if has_more {
            messages.remove(0);
        }

        Ok((messages, has_more))
    }

    /// One forward page of a Conversation's history, oldest first.
    ///
    /// `after` is an exclusive Sequence Number cursor; this returns the oldest
    /// `limit` Messages newer than it. Returns `(rows, has_more)` where `has_more`
    /// means "newer Messages exist beyond this page" — the probe row is the newest
    /// of the batch and is dropped, mirroring [`Self::list_message_page`].
    ///
    /// A cursor at or above the newest Message yields an empty page with
    /// `has_more == false`: a client that is already caught up learns there is
    /// nothing to repair, which is a normal answer rather than an error.
    pub async fn list_message_page_after(
        &self,
        conversation_id: &str,
        after: i64,
        limit: i64,
    ) -> Result<(Vec<MessageRow>, bool), ChatError> {
        let probe = limit.saturating_add(1);

        let rows = sqlx::query(&page_query(SELECT_MESSAGE_PAGE_AFTER))
            .bind(conversation_id)
            .bind(after)
            .bind(probe)
            .fetch_all(&self.pool)
            .await
            .map_err(ChatError::Database)?;

        let mut messages: Vec<MessageRow> = rows.iter().map(message_from_row).collect();
        let has_more = messages.len() as i64 > limit;
        if has_more {
            // The probe row is the newest; drop it so the page is exactly `limit`.
            messages.pop();
        }

        Ok((messages, has_more))
    }
}

/// Find a Message by its idempotency key, inside any executor.
async fn find_message_by_client_id<'e, E>(
    executor: E,
    sender_id: &str,
    client_msg_id: &str,
) -> Result<Option<MessageRow>, ChatError>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(SELECT_MESSAGE_BY_CLIENT_ID)
        .bind(sender_id)
        .bind(client_msg_id)
        .fetch_optional(executor)
        .await
        .map(|row| row.map(|row| message_from_row(&row)))
        .map_err(ChatError::Database)
}

fn conversation_from_row(row: &PgRow) -> ConversationRow {
    ConversationRow {
        id: row.get("id"),
        kind: row.get("kind"),
        created_at: row.get("created_at"),
    }
}

fn peer_from_row(row: &PgRow) -> PeerRow {
    PeerRow {
        id: row.get("id"),
        username: row.get("username"),
        display_name: row.get("display_name"),
        avatar_url: row.get("avatar_url"),
    }
}

/// The peer columns as they come back from the conversation-list join.
fn peer_from_join_row(row: &PgRow) -> PeerRow {
    PeerRow {
        id: row.get("peer_id"),
        username: row.get("peer_username"),
        display_name: row.get("peer_display_name"),
        avatar_url: row.get("peer_avatar_url"),
    }
}

fn message_from_row(row: &PgRow) -> MessageRow {
    MessageRow {
        id: row.get("id"),
        conversation_id: row.get("conversation_id"),
        seq: row.get("seq"),
        sender_id: row.get("sender_id"),
        client_msg_id: row.get("client_msg_id"),
        body: row.get("body"),
        created_at: row.get("created_at"),
    }
}
