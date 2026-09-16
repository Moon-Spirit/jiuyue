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

use std::collections::HashMap;

use jiuyue_contract::{ReadReceipt, Role, SyncCursor};
use sqlx::postgres::PgRow;
use sqlx::types::time::OffsetDateTime;
use sqlx::{Executor, PgPool, Postgres, Row};

use crate::error::ChatError;

const INSERT_DIRECT_CONVERSATION: &str = "\
    INSERT INTO conversations (id, kind, direct_key) VALUES ($1, 'direct', $2) \
    ON CONFLICT (direct_key) DO NOTHING \
    RETURNING id::text AS id, kind, title, announcement, created_at";

const SELECT_CONVERSATION_BY_DIRECT_KEY: &str = "\
    SELECT id::text AS id, kind, title, announcement, created_at \
    FROM conversations WHERE direct_key = $1";

const SELECT_CONVERSATION: &str = "\
    SELECT id::text AS id, kind, title, announcement, created_at \
    FROM conversations WHERE id = $1";

/// Create a Group Conversation.
///
/// A group has no `direct_key` (the schema CHECK refuses one), so two groups can
/// never collide on the pair key and there is nothing to conflict on: the caller
/// supplies a fresh ULID.
const INSERT_GROUP_CONVERSATION: &str = "\
    INSERT INTO conversations (id, kind, title) VALUES ($1, 'group', $2) \
    RETURNING id::text AS id, kind, title, announcement, created_at";

/// Replace a Group's announcement. `NULL` clears it.
///
/// Only the announcement is written; `updated_at` moves with it so the row's own
/// clock stays honest. The kind/role checks happen in the service, which resolves
/// the caller's Role first — this statement carries no permission logic.
const SET_ANNOUNCEMENT: &str = "\
    UPDATE conversations SET announcement = $2, updated_at = now() \
    WHERE id = $1";

/// Every Participant's stored Unread Count in one Conversation.
///
/// Used when one change must be rendered **per recipient** — an announcement edit
/// sends each Participant their own summary, and that summary carries their own
/// Unread Count. One query for the whole group, never one per Participant.
const SELECT_MEMBER_UNREADS: &str = "\
    SELECT user_id::text AS user_id, unread_count \
    FROM conversation_members WHERE conversation_id = $1";

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
    SELECT c.id::text AS id, c.kind, c.title, c.announcement, c.created_at, me.unread_count, \
           u.id::text AS peer_id, u.username AS peer_username, \
           u.display_name AS peer_display_name, u.avatar_url AS peer_avatar_url \
    FROM conversation_members AS me \
    JOIN conversations AS c ON c.id = me.conversation_id \
    JOIN conversation_members AS peer_member \
      ON peer_member.conversation_id = c.id AND peer_member.user_id <> me.user_id \
    JOIN users AS u ON u.id = peer_member.user_id \
    WHERE me.user_id = $1 AND c.kind = 'direct' \
    ORDER BY c.created_at DESC";

/// Every Group Conversation the User participates in, newest first.
///
/// The Group counterpart of [`SELECT_CONVERSATIONS_FOR_USER`]: it carries the
/// group's title, the **caller's** Role and the current Participant count instead
/// of a peer. Keeping the two statements separate (rather than one query with a
/// nullable peer and a nullable group) keeps each projection exactly the columns
/// its kind has, and the service merges the two lists.
const SELECT_GROUP_CONVERSATIONS_FOR_USER: &str = "\
    SELECT c.id::text AS id, c.kind, c.title, c.announcement, c.created_at, me.role, me.unread_count, \
           (SELECT count(*) FROM conversation_members AS counted \
            WHERE counted.conversation_id = c.id) AS member_count \
    FROM conversation_members AS me \
    JOIN conversations AS c ON c.id = me.conversation_id \
    WHERE me.user_id = $1 AND c.kind = 'group' \
    ORDER BY c.created_at DESC";

/// Add one Participant with an explicit Role, for the Group path.
///
/// Deliberately not `ON CONFLICT DO NOTHING` like [`INSERT_MEMBERS`]: inviting
/// someone already in the group is a state conflict the caller must report, and a
/// silent no-op would let the caller believe the invite reached a new Participant.
const INSERT_MEMBER_WITH_ROLE: &str = "\
    INSERT INTO conversation_members (conversation_id, user_id, role) \
    VALUES ($1, $2, $3)";

/// One Participant's stored Role, or no row when they are not a Participant.
const SELECT_MEMBER_ROLE: &str = "\
    SELECT role FROM conversation_members \
    WHERE conversation_id = $1 AND user_id = $2";

/// Every Participant with their public profile and Role, in join order.
const SELECT_MEMBERS: &str = "\
    SELECT m.user_id::text AS user_id, u.username, u.display_name, u.avatar_url, \
           m.role, m.joined_at \
    FROM conversation_members AS m \
    JOIN users AS u ON u.id = m.user_id \
    WHERE m.conversation_id = $1 \
    ORDER BY m.joined_at, m.user_id";

const COUNT_MEMBERS: &str = "\
    SELECT count(*) FROM conversation_members WHERE conversation_id = $1";

/// Remove one Participant. Returns whether a row was actually deleted.
///
/// The `role <> 'owner'` guard is structural: an owner can never be removed by
/// this statement, whatever the caller checked. The owner leaves by transferring
/// or dissolving, never by removal.
const DELETE_MEMBER: &str = "\
    DELETE FROM conversation_members \
    WHERE conversation_id = $1 AND user_id = $2 AND role <> 'owner'";

/// Set one Participant's Role between `member` and `admin`.
///
/// The `role <> 'owner'` guard forbids promoting or demoting the owner here:
/// ownership only moves through [`ChatRepository::transfer_ownership`], which
/// demotes and promotes in one transaction. Returns whether a row changed.
const SET_MEMBER_ROLE: &str = "\
    UPDATE conversation_members SET role = $3 \
    WHERE conversation_id = $1 AND user_id = $2 AND role <> 'owner'";

/// Demote the outgoing owner before promoting the incoming one.
///
/// Order matters: `conversation_members_single_owner` is a partial UNIQUE index,
/// so a promote-then-demote would be a transient violation. In a single
/// transaction this pair leaves exactly one owner at every instant.
const DEMOTE_OWNER: &str = "\
    UPDATE conversation_members SET role = 'admin' \
    WHERE conversation_id = $1 AND user_id = $2 AND role = 'owner'";

const PROMOTE_OWNER: &str = "\
    UPDATE conversation_members SET role = 'owner' \
    WHERE conversation_id = $1 AND user_id = $2";

/// Delete a Conversation. The `conversation_members` and `messages` foreign keys
/// are `ON DELETE CASCADE`, so this is the whole dissolution.
const DELETE_CONVERSATION: &str = "\
    DELETE FROM conversations WHERE id = $1";

const SELECT_MEMBERSHIP: &str = "\
    SELECT EXISTS ( \
        SELECT 1 FROM conversation_members AS m \
        WHERE m.conversation_id = c.id AND m.user_id = $2 \
    ) AS is_member \
    FROM conversations AS c WHERE c.id = $1";

const SELECT_PARTICIPANTS: &str = "\
    SELECT user_id::text AS user_id \
    FROM conversation_members WHERE conversation_id = $1 ORDER BY joined_at";

/// Every User who shares a Conversation with the given User, excluding them.
///
/// This is the audience for a User's presence: the Participants of their shared
/// Conversations, and nobody else. It is deliberately a *relationship* query and
/// knows nothing about presence — the realtime gateway decides what to send, this
/// decides who is allowed to hear it. Keeping the two apart is what lets issue #27
/// narrow visibility by changing one caller rather than every fan-out.
///
/// The self-join is on `conversation_id` with `other.user_id <> mine.user_id`, so a
/// Direct Conversation yields the one peer and a Group yields the rest of its
/// members; `DISTINCT` collapses a peer reachable through several Conversations.
///
/// `candidates` narrows the answer to a caller-supplied set — the realtime path
/// passes `None` (the whole audience) and the presence REST read passes the ids it
/// was asked about, so a request for two peers does not materialise a User's entire
/// social graph. A `NULL` array means "no filter", which is why the parameter is
/// tested rather than the query being duplicated.
const SELECT_PRESENCE_AUDIENCE: &str = "\
    SELECT DISTINCT other.user_id::text AS user_id \
    FROM conversation_members AS mine \
    JOIN conversation_members AS other \
      ON other.conversation_id = mine.conversation_id AND other.user_id <> mine.user_id \
    WHERE mine.user_id = $1 \
      AND ($2::text[] IS NULL OR other.user_id = ANY($2::text[])) \
    ORDER BY user_id";

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

/// Checkpoint a Device's consumed positions in one statement.
///
/// The two parallel arrays are fed through `unnest`, exactly as [`INSERT_MEMBERS`]
/// feeds the member list: one round trip regardless of how many Conversations the
/// batch covers, which is the point of coalescing before writing.
///
/// `JOIN conversations` is what makes an unknown Conversation a silent no-op
/// instead of a foreign-key failure that would sink the whole batch — the cursor
/// is advisory state for this Device, so a stale report is dropped, not fatal.
/// `GREATEST` keeps the cursor monotonic: several connections of one Device may
/// report out of order, and a cursor must never rewind.
const UPSERT_SYNC_CURSORS: &str = "\
    INSERT INTO sync_cursors (session_id, conversation_id, last_seq) \
    SELECT $1, candidate.conversation_id, candidate.last_seq \
    FROM unnest($2::text[], $3::bigint[]) AS candidate (conversation_id, last_seq) \
    JOIN conversations AS c ON c.id = candidate.conversation_id \
    ON CONFLICT (session_id, conversation_id) \
    DO UPDATE SET last_seq = GREATEST(sync_cursors.last_seq, EXCLUDED.last_seq), \
                  updated_at = now()";

/// The Device's stored cursors, most recently advanced first.
///
/// Ordered and bounded so one connect can never hand a client an unbounded frame:
/// the caller passes `MAX_SYNC_CURSORS`, and the order is deterministic (a tie on
/// `updated_at` falls back to the Conversation id) so two reads agree.
const SELECT_SYNC_CURSORS: &str = "\
    SELECT conversation_id::text AS conversation_id, last_seq \
    FROM sync_cursors WHERE session_id = $1 \
    ORDER BY updated_at DESC, conversation_id DESC LIMIT $2";

/// Bump every recipient's Unread Count for a freshly written Message.
///
/// This runs **inside the same transaction as the insert**, so the count and the
/// stream can never disagree: a rollback (a lost idempotency race) undoes both.
/// The `read_marker_seq < $3` guard is what makes it correct rather than merely
/// additive — a Message the User has already read (its `seq` is at or below the
/// marker) must not re-inflate the count, however late this statement runs.
///
/// Concurrent sends to the same Conversation each run this `UPDATE`, and because
/// they touch the same participant rows they serialise on the row lock: N
/// concurrent Messages produce exactly N increments, not a lost update. The
/// sender's own row is excluded (`user_id <> $2`): a User's own Messages are
/// never unread to them.
const INCREMENT_UNREAD: &str = "\
    UPDATE conversation_members \
    SET unread_count = unread_count + 1 \
    WHERE conversation_id = $1 AND user_id <> $2 AND read_marker_seq < $3";

/// Advance one User's private Read Marker *and* public Read Receipt, and settle
/// the Unread Count.
///
/// The two columns are written together but remain two columns with two
/// meanings: `read_marker_seq` is the private position that drives the badge,
/// `read_receipt_seq` is the public position a peer will be shown. Nothing here
/// reads or returns another User's marker.
///
/// `LEAST($3, c.newest_seq)` clamps the reported position to the newest Message
/// that actually exists. `conversations.next_seq` is the allocator the send path
/// pre-increments (`UPDATE ... SET next_seq = next_seq + 1 ... RETURNING`), so at
/// rest it equals the highest Sequence Number handed out — and 0 when the
/// Conversation has no Messages at all. An over-reported value therefore cannot
/// mark unread Messages as read, which would otherwise wedge the guarded
/// increment above forever. `GREATEST` makes both positions monotonic, so a
/// replayed or out-of-order report cannot rewind them.
///
/// The Unread Count is recomputed from the invariant (Messages after the new
/// marker, excluding the User's own) in the same statement: one range read of the
/// `(conversation_id, seq)` index at human frequency, and it self-corrects the
/// incremental count. Returns no row when the caller is not a Participant.
const MARK_READ: &str = "\
    UPDATE conversation_members AS cm \
    SET read_marker_seq = GREATEST(cm.read_marker_seq, LEAST($3, c.newest_seq)), \
        read_receipt_seq = GREATEST(cm.read_receipt_seq, LEAST($3, c.newest_seq)), \
        unread_count = ( \
            SELECT count(*) FROM messages AS m \
            WHERE m.conversation_id = cm.conversation_id \
              AND m.sender_id <> cm.user_id \
              AND m.seq > GREATEST(cm.read_marker_seq, LEAST($3, c.newest_seq)) \
        ) \
    FROM ( \
        SELECT id, next_seq AS newest_seq \
        FROM conversations WHERE id = $1 \
    ) AS c \
    WHERE cm.conversation_id = c.id AND cm.user_id = $2 \
    RETURNING cm.read_marker_seq, cm.read_receipt_seq, cm.unread_count";

/// The **other** Participants' public Read Receipts for one Conversation.
///
/// This query names exactly one read column, `read_receipt_seq`. It cannot return
/// a Read Marker because it does not select that column — which is the structural
/// half of "a private marker is never served as a public receipt". The caller is
/// excluded (`user_id <> $2`): their own receipt is not news to them, and their
/// own marker is not in this result at all.
const SELECT_RECEIPTS: &str = "\
    SELECT conversation_id::text AS conversation_id, user_id::text AS user_id, \
           read_receipt_seq \
    FROM conversation_members \
    WHERE conversation_id = $1 AND user_id <> $2 \
    ORDER BY user_id";

/// One Participant's private read state, for the owning User alone.
///
/// Used by tests and by the server's own bookkeeping; it is never attached to a
/// peer-facing response.
const SELECT_READ_STATE: &str = "\
    SELECT read_marker_seq, read_receipt_seq, unread_count \
    FROM conversation_members WHERE conversation_id = $1 AND user_id = $2";

/// A row of `conversations`, as this module needs it.
#[derive(Debug, Clone)]
pub struct ConversationRow {
    /// ULID.
    pub id: String,
    /// `direct` or `group`.
    pub kind: String,
    /// The group's display name; `None` for a Direct Conversation.
    pub title: Option<String>,
    /// The group's announcement; `None` when absent, cleared, or Direct.
    pub announcement: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// A Participant with their public profile and Role.
#[derive(Debug, Clone)]
pub struct MemberRow {
    /// ULID of the User.
    pub user_id: String,
    /// `@handle`.
    pub username: String,
    /// Display name.
    pub display_name: String,
    /// Avatar URL, when set.
    pub avatar_url: Option<String>,
    /// Stored Role (`owner` | `admin` | `member`).
    pub role: String,
    /// When they became a Participant.
    pub joined_at: OffsetDateTime,
}

/// A Group Conversation as its list renders it: the Conversation, the caller's
/// Role, their Unread Count, and how many Participants there are.
#[derive(Debug, Clone)]
pub struct GroupConversationRow {
    /// The Conversation itself.
    pub conversation: ConversationRow,
    /// The **caller's** stored Role.
    pub role: String,
    /// The caller's Unread Count.
    pub unread_count: i64,
    /// Current Participant count.
    pub member_count: i64,
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

/// One User's private read state in one Conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadStateRow {
    /// Private Read Marker: highest Sequence Number the User has read.
    pub read_marker_seq: i64,
    /// Public Read Receipt: highest Sequence Number acknowledged to peers.
    pub read_receipt_seq: i64,
    /// Unread Count after the marker advanced.
    pub unread_count: i64,
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

    /// Create a Group Conversation with its owner and initial members, atomically.
    ///
    /// One transaction, so a group can never exist half-populated: either it has
    /// its owner and every invited member, or nothing was written.
    pub async fn create_group_with_members(
        &self,
        id: &str,
        title: &str,
        owner_id: &str,
        member_ids: &[String],
    ) -> Result<ConversationRow, ChatError> {
        let mut transaction = self.pool.begin().await.map_err(ChatError::Database)?;

        let conversation = sqlx::query(INSERT_GROUP_CONVERSATION)
            .bind(id)
            .bind(title)
            .fetch_one(&mut *transaction)
            .await
            .map(|row| conversation_from_row(&row))
            .map_err(ChatError::Database)?;

        sqlx::query(INSERT_MEMBER_WITH_ROLE)
            .bind(id)
            .bind(owner_id)
            .bind(Role::Owner.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(ChatError::Database)?;

        for member_id in member_ids {
            sqlx::query(INSERT_MEMBER_WITH_ROLE)
                .bind(id)
                .bind(member_id)
                .bind(Role::Member.as_str())
                .execute(&mut *transaction)
                .await
                .map_err(ChatError::Database)?;
        }

        transaction.commit().await.map_err(ChatError::Database)?;
        Ok(conversation)
    }

    /// Read one Conversation by id, whatever its kind.
    pub async fn conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationRow>, ChatError> {
        sqlx::query(SELECT_CONVERSATION)
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| conversation_from_row(&row)))
            .map_err(ChatError::Database)
    }

    /// Add one Participant with an explicit Role.
    ///
    /// A duplicate is a database error rather than a silent no-op; the service
    /// checks membership first and reports [`ChatError::AlreadyMember`], so this
    /// error path is the backstop for a concurrent invite, not the normal one.
    pub async fn add_member_with_role(
        &self,
        conversation_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), ChatError> {
        sqlx::query(INSERT_MEMBER_WITH_ROLE)
            .bind(conversation_id)
            .bind(user_id)
            .bind(role)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(ChatError::Database)
    }

    /// One Participant's stored Role, or `None` when they are not a Participant.
    pub async fn member_role(
        &self,
        conversation_id: &str,
        user_id: &str,
    ) -> Result<Option<String>, ChatError> {
        sqlx::query(SELECT_MEMBER_ROLE)
            .bind(conversation_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| row.get("role")))
            .map_err(ChatError::Database)
    }

    /// Every Participant with their profile and Role, in join order.
    pub async fn list_members(&self, conversation_id: &str) -> Result<Vec<MemberRow>, ChatError> {
        sqlx::query(SELECT_MEMBERS)
            .bind(conversation_id)
            .fetch_all(&self.pool)
            .await
            .map(|rows| rows.iter().map(member_from_row).collect())
            .map_err(ChatError::Database)
    }

    /// How many Participants a Conversation has.
    pub async fn member_count(&self, conversation_id: &str) -> Result<i64, ChatError> {
        sqlx::query(COUNT_MEMBERS)
            .bind(conversation_id)
            .fetch_one(&self.pool)
            .await
            .map(|row| row.get("count"))
            .map_err(ChatError::Database)
    }

    /// Remove one Participant. Returns whether a row was deleted.
    ///
    /// An owner is never deleted by this statement (the SQL carries the guard),
    /// so a caller that skipped the permission check still cannot decapitate a
    /// group.
    pub async fn remove_member(
        &self,
        conversation_id: &str,
        user_id: &str,
    ) -> Result<bool, ChatError> {
        sqlx::query(DELETE_MEMBER)
            .bind(conversation_id)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected() > 0)
            .map_err(ChatError::Database)
    }

    /// Set a Participant's Role between member and admin. Returns whether a row
    /// changed; the owner is never touched.
    pub async fn set_member_role(
        &self,
        conversation_id: &str,
        user_id: &str,
        role: &str,
    ) -> Result<bool, ChatError> {
        sqlx::query(SET_MEMBER_ROLE)
            .bind(conversation_id)
            .bind(user_id)
            .bind(role)
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected() > 0)
            .map_err(ChatError::Database)
    }

    /// Move ownership from `from_user_id` to `to_user_id` atomically.
    ///
    /// Both statements run in one transaction and in the order the partial UNIQUE
    /// index requires (demote, then promote), so no reader can observe two owners
    /// or none. Returns `false` when `from_user_id` was not the owner or
    /// `to_user_id` is not a Participant; the transaction rolls back either way,
    /// leaving the roles untouched.
    pub async fn transfer_ownership(
        &self,
        conversation_id: &str,
        from_user_id: &str,
        to_user_id: &str,
    ) -> Result<bool, ChatError> {
        let mut transaction = self.pool.begin().await.map_err(ChatError::Database)?;

        let demoted = sqlx::query(DEMOTE_OWNER)
            .bind(conversation_id)
            .bind(from_user_id)
            .execute(&mut *transaction)
            .await
            .map_err(ChatError::Database)?
            .rows_affected();

        if demoted == 0 {
            transaction.rollback().await.map_err(ChatError::Database)?;
            return Ok(false);
        }

        let promoted = sqlx::query(PROMOTE_OWNER)
            .bind(conversation_id)
            .bind(to_user_id)
            .execute(&mut *transaction)
            .await
            .map_err(ChatError::Database)?
            .rows_affected();

        if promoted == 0 {
            transaction.rollback().await.map_err(ChatError::Database)?;
            return Ok(false);
        }

        transaction.commit().await.map_err(ChatError::Database)?;
        Ok(true)
    }

    /// Replace a Conversation's announcement; `None` clears it.
    ///
    /// Returns whether a row changed. The caller has already resolved the group
    /// and the Role, so this is the plain write, not a permission check.
    pub async fn set_announcement(
        &self,
        conversation_id: &str,
        announcement: Option<&str>,
    ) -> Result<bool, ChatError> {
        sqlx::query(SET_ANNOUNCEMENT)
            .bind(conversation_id)
            .bind(announcement)
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected() > 0)
            .map_err(ChatError::Database)
    }

    /// Every Participant's Unread Count in one Conversation, keyed by User id.
    ///
    /// A Participant with no row cannot exist (membership is the row), so a
    /// missing key at a call site would be a programming error, not a state.
    pub async fn member_unread_counts(
        &self,
        conversation_id: &str,
    ) -> Result<HashMap<String, i64>, ChatError> {
        sqlx::query(SELECT_MEMBER_UNREADS)
            .bind(conversation_id)
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|row| (row.get("user_id"), row.get("unread_count")))
                    .collect()
            })
            .map_err(ChatError::Database)
    }

    /// Every Group Conversation the User participates in, newest first.
    pub async fn list_group_conversations(
        &self,
        user_id: &str,
    ) -> Result<Vec<GroupConversationRow>, ChatError> {
        sqlx::query(SELECT_GROUP_CONVERSATIONS_FOR_USER)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|row| GroupConversationRow {
                        conversation: conversation_from_row(row),
                        role: row.get("role"),
                        unread_count: row.get("unread_count"),
                        member_count: row.get("member_count"),
                    })
                    .collect()
            })
            .map_err(ChatError::Database)
    }

    /// Delete a Conversation. Returns whether a row was deleted.
    ///
    /// The memberships and Messages go with it through the foreign keys'
    /// `ON DELETE CASCADE`, so this one statement is the whole dissolution.
    pub async fn dissolve(&self, conversation_id: &str) -> Result<bool, ChatError> {
        sqlx::query(DELETE_CONVERSATION)
            .bind(conversation_id)
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected() > 0)
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
    /// Each entry also carries the **caller's** Unread Count, read from their own
    /// participant row. The count is stored, never recomputed by scanning
    /// `messages` here, because the Conversation list is the hottest read path.
    ///
    /// Group Conversations are excluded until the group ticket defines what a
    /// group summary renders; the query is deliberately not written to half-guess it.
    pub async fn list_conversations(
        &self,
        user_id: &str,
    ) -> Result<Vec<(ConversationRow, PeerRow, i64)>, ChatError> {
        sqlx::query(SELECT_CONVERSATIONS_FOR_USER)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        (
                            conversation_from_row(row),
                            peer_from_join_row(row),
                            row.get("unread_count"),
                        )
                    })
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

    /// Every User who shares a Conversation with `user_id`, or only those in
    /// `candidates` when one is given.
    ///
    /// See [`SELECT_PRESENCE_AUDIENCE`]: this is the "who has a reason to care"
    /// relation, not a presence read. A User with no Conversations has an empty
    /// audience, which is the correct answer rather than an error.
    pub async fn presence_audience(
        &self,
        user_id: &str,
        candidates: Option<&[String]>,
    ) -> Result<Vec<String>, ChatError> {
        sqlx::query(SELECT_PRESENCE_AUDIENCE)
            .bind(user_id)
            .bind(candidates)
            .fetch_all(&self.pool)
            .await
            .map(|rows| rows.iter().map(|row| row.get("user_id")).collect())
            .map_err(ChatError::Database)
    }

    /// Store a Message, or return the one this idempotency key already wrote.
    ///
    /// Returns `(row, created)`: `created` is false when the send was a replay.
    ///
    /// The Sequence Number, the row and the recipient Unread Count bumps are one
    /// unit of work. A replay detected after the allocation is resolved by
    /// *rolling the transaction back*, which undoes the `next_seq` increment and
    /// the badge bumps together — otherwise every retried send would punch a hole
    /// in the Conversation's ordering and inflate every badge.
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
                // The badge bump is part of the same transaction as the row, so a
                // crash cannot commit one without the other, and a later rollback
                // undoes both.
                sqlx::query(INCREMENT_UNREAD)
                    .bind(&message.conversation_id)
                    .bind(&message.sender_id)
                    .bind(seq)
                    .execute(&mut *transaction)
                    .await
                    .map_err(ChatError::Database)?;

                transaction.commit().await.map_err(ChatError::Database)?;
                Ok((message_from_row(&row), true))
            }
            None => {
                // A concurrent send with the same idempotency key won the insert.
                // Rolling back here is what returns the allocated `seq` to the pool
                // and discards the badge bump.
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

    /// Checkpoint a batch of a Device's consumed positions.
    ///
    /// `cursors` must already be de-duplicated by the caller: PostgreSQL refuses an
    /// `ON CONFLICT DO UPDATE` that would touch the same row twice, so a batch with
    /// two entries for one Conversation is a statement error, not a last-writer-win.
    /// Unknown Conversations are skipped by the `JOIN`; the rest are advanced
    /// monotonically (`GREATEST`), so a replayed or out-of-order report is harmless.
    pub async fn upsert_sync_cursors(
        &self,
        session_id: &str,
        cursors: &[SyncCursor],
    ) -> Result<(), ChatError> {
        let conversation_ids: Vec<String> = cursors
            .iter()
            .map(|cursor| cursor.conversation_id.clone())
            .collect();
        let last_seqs: Vec<i64> = cursors.iter().map(|cursor| cursor.last_seq).collect();

        sqlx::query(UPSERT_SYNC_CURSORS)
            .bind(session_id)
            .bind(&conversation_ids)
            .bind(&last_seqs)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(ChatError::Database)
    }

    /// A Device's stored cursors, most recently advanced first, at most `limit`.
    pub async fn list_sync_cursors(
        &self,
        session_id: &str,
        limit: i64,
    ) -> Result<Vec<SyncCursor>, ChatError> {
        sqlx::query(SELECT_SYNC_CURSORS)
            .bind(session_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|row| SyncCursor {
                        conversation_id: row.get("conversation_id"),
                        last_seq: row.get("last_seq"),
                    })
                    .collect()
            })
            .map_err(ChatError::Database)
    }

    /// Advance one User's private Read Marker and public Read Receipt.
    ///
    /// Both are clamped to the newest Message that exists and move only forward
    /// (`GREATEST`), and the Unread Count is recomputed from the same invariant in
    /// the same statement. Returns `None` when the User is not a Participant —
    /// there is no row to advance, and that must be distinguishable from success.
    pub async fn mark_read(
        &self,
        conversation_id: &str,
        user_id: &str,
        last_read_seq: i64,
    ) -> Result<Option<ReadStateRow>, ChatError> {
        sqlx::query(MARK_READ)
            .bind(conversation_id)
            .bind(user_id)
            .bind(last_read_seq)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| read_state_from_row(&row)))
            .map_err(ChatError::Database)
    }

    /// The other Participants' public Read Receipts for a Conversation.
    ///
    /// `exclude_user_id` is the caller: their own receipt is not news to them.
    /// Only `read_receipt_seq` is selected, so no private Read Marker can appear
    /// in this result.
    pub async fn list_receipts(
        &self,
        conversation_id: &str,
        exclude_user_id: &str,
    ) -> Result<Vec<ReadReceipt>, ChatError> {
        sqlx::query(SELECT_RECEIPTS)
            .bind(conversation_id)
            .bind(exclude_user_id)
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|row| ReadReceipt {
                        conversation_id: row.get("conversation_id"),
                        reader_id: row.get("user_id"),
                        last_read_seq: row.get("read_receipt_seq"),
                    })
                    .collect()
            })
            .map_err(ChatError::Database)
    }

    /// One User's private read state, or `None` when they are not a Participant.
    pub async fn read_state(
        &self,
        conversation_id: &str,
        user_id: &str,
    ) -> Result<Option<ReadStateRow>, ChatError> {
        sqlx::query(SELECT_READ_STATE)
            .bind(conversation_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.map(|row| read_state_from_row(&row)))
            .map_err(ChatError::Database)
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
        title: row.get("title"),
        announcement: row.get("announcement"),
        created_at: row.get("created_at"),
    }
}

fn member_from_row(row: &PgRow) -> MemberRow {
    MemberRow {
        user_id: row.get("user_id"),
        username: row.get("username"),
        display_name: row.get("display_name"),
        avatar_url: row.get("avatar_url"),
        role: row.get("role"),
        joined_at: row.get("joined_at"),
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

fn read_state_from_row(row: &PgRow) -> ReadStateRow {
    ReadStateRow {
        read_marker_seq: row.get("read_marker_seq"),
        read_receipt_seq: row.get("read_receipt_seq"),
        unread_count: row.get("unread_count"),
    }
}
