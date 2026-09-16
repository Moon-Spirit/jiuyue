//! Presence: who is reachable right now, and when they last were.
//!
//! # The model
//!
//! A **User** is online when *any* of their Devices is connected, and offline only
//! when the last one goes. That is the Device-versus-User line CONTEXT.md draws:
//! Sync Cursors are per Device, Presence is per User. The registry already counts a
//! User's live connections, so a transition is exactly "this registration was the
//! User's first" or "this removal was the User's last" — and nothing in between,
//! which is what stops a phone and a laptop flickering each other offline.
//!
//! # Two halves
//!
//! - **Reachability** is in-process state in the [`ConnectionRegistry`]. There is
//!   deliberately no Redis (ADR-0010), and there does not need to be: a process
//!   restart empties the registry, so *every* User is offline afterwards and no
//!   socket can leave a ghost behind. Correctness after a restart is therefore a
//!   property of the design rather than a cleanup job.
//! - **Last seen** is the durable half, in `user_presence` (this module's table).
//!   Without it a restart would forget when the Users who were online stopped
//!   being reachable, and a peer would be shown a stale instant from days ago.
//!
//! # Write discipline (ADR-0014, applied to last-seen)
//!
//! One write per presence change would be one write per connect/disconnect across
//! the whole node. Instead [`PresenceCheckpoint`] coalesces the Users to stamp
//! **in memory**, and [`LastSeenStore`] writes them in one statement per flush:
//!
//! - on a connection's **heartbeat** (every [`crate::HEARTBEAT_INTERVAL`]), which is
//!   also what keeps a long-lived online User's instant fresh;
//! - on a User's **last Device disconnecting**, so the offline instant is durable
//!   immediately rather than waiting for the next beat;
//! - when the pending set crosses [`crate::PRESENCE_CHECKPOINT_BATCH`].
//!
//! **What a crash between checkpoints costs**: the last-seen instant of every User
//! who was online is at most one heartbeat interval stale (the next beat re-stamps
//! them, and the stored value never moves backwards). A User who went offline just
//! before the crash keeps the previous checkpoint's instant for them. Nothing is
//! lost that a peer could not have been shown anyway — last seen is advisory.
//!
//! # Fan-out is aimed, never broadcast
//!
//! A transition is delivered only to Participants of a shared Conversation,
//! resolved through [`jiuyue_chat::ChatService::presence_audience`] — the one seam
//! issue #27 narrows when "who can see my presence" becomes a setting. Fanning a
//! presence change out to every connected User would be invisible on one node and
//! fatal at the scale ADR-0005 designs for, so the audience is a decision the
//! domain makes, not a loop over the registry.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use jiuyue_contract::{Presence as PresenceView, PresenceStatus, ServerEvent};
use sqlx::Row;
use sqlx::types::time::OffsetDateTime;
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::{PRESENCE_CHECKPOINT_BATCH, RealtimeError, RealtimeHub};

/// The Users whose last-seen instant is waiting to be written.
///
/// A set, not a map: the instant that gets stored is PostgreSQL's `now()` at flush
/// time, so the only thing worth coalescing is *who* was reachable since the last
/// flush. A User who connects and disconnects between two flushes appears once.
///
/// Deliberately free of I/O and of async, like `CursorCheckpoint`: the coalescing
/// rule (one entry per User, a bound that asks for an early flush) is unit-tested
/// without a database.
#[derive(Debug, Default)]
pub(crate) struct PresenceCheckpoint {
    pending: Mutex<HashSet<String>>,
}

impl PresenceCheckpoint {
    /// The guard, recovering rather than panicking if a writer panicked.
    ///
    /// A poisoned lock here means some other thread panicked between taking the
    /// lock and releasing it; the set is still a consistent `HashSet`, so the
    /// right move is to keep using it, not to take the whole node down.
    fn lock(&self) -> MutexGuard<'_, HashSet<String>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Note that a User must be stamped, returning whether the batch bound was hit.
    ///
    /// The bool is the caller's signal to flush without waiting for the heartbeat;
    /// returning a decision instead of flushing here is what keeps this type free
    /// of I/O.
    pub(crate) fn mark(&self, user_id: &str) -> bool {
        let mut pending = self.lock();
        pending.insert(user_id.to_owned());
        pending.len() >= PRESENCE_CHECKPOINT_BATCH
    }

    /// Note many Users at once, with the same early-flush signal as [`Self::mark`].
    pub(crate) fn mark_many(&self, user_ids: &[String]) -> bool {
        let mut pending = self.lock();
        for user_id in user_ids {
            pending.insert(user_id.clone());
        }
        pending.len() >= PRESENCE_CHECKPOINT_BATCH
    }

    /// Whether nothing is waiting to be written.
    pub(crate) fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The pending Users as a batch, in no particular order.
    ///
    /// A snapshot rather than a drain: a failed write must not lose the instant, so
    /// the caller clears only after PostgreSQL accepted the batch.
    pub(crate) fn batch(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }

    /// Forget everything, after a successful write.
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }
}

/// The `user_presence` table — the durable half of presence.
///
/// Owned by this crate (ADR-0011: one table, one owning crate). It stores only the
/// last-reachable instant; whether a User is online *now* is never written here.
#[derive(Debug, Clone)]
pub struct LastSeenStore {
    pool: PgPool,
}

impl LastSeenStore {
    /// Wrap a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Stamp the given Users as reachable as of PostgreSQL's `now()`.
    ///
    /// `GREATEST` keeps the stored instant monotonic, so an out-of-order or
    /// replayed checkpoint can never rewind it. The `JOIN users` drops an id that
    /// no longer names a row (a deleted account) instead of failing the whole batch
    /// on the foreign key — the same defensive join `UPSERT_SYNC_CURSORS` uses, and
    /// the reason a single stale id cannot wedge every later checkpoint.
    pub async fn save_last_seen(&self, user_ids: &[String]) -> Result<(), RealtimeError> {
        if user_ids.is_empty() {
            return Ok(());
        }

        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO user_presence (user_id, last_seen_at, updated_at) \
             SELECT candidate.user_id, now(), now() \
             FROM unnest(",
        );
        query.push_bind(user_ids);
        query.push(
            ") AS candidate (user_id) \
             JOIN users AS account ON account.id = candidate.user_id \
             ON CONFLICT (user_id) DO UPDATE \
             SET last_seen_at = GREATEST(user_presence.last_seen_at, EXCLUDED.last_seen_at), \
                 updated_at = now()",
        );

        query
            .build()
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(RealtimeError::Presence)
    }

    /// The stored last-seen instant of each named User, keyed by User id.
    ///
    /// A User who has never been seen has no row and so no entry; the caller reads
    /// a missing key as "never seen", which is a normal answer for a User who has
    /// not connected since the feature shipped.
    pub async fn last_seen(
        &self,
        user_ids: &[String],
    ) -> Result<HashMap<String, i64>, RealtimeError> {
        if user_ids.is_empty() {
            return Ok(HashMap::new());
        }

        sqlx::query("SELECT user_id::text AS user_id, last_seen_at FROM user_presence WHERE user_id = ANY($1)")
            .bind(user_ids)
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        (
                            row.get::<String, _>("user_id"),
                            unix_millis(row.get::<OffsetDateTime, _>("last_seen_at")),
                        )
                    })
                    .collect()
            })
            .map_err(RealtimeError::Presence)
    }
}

/// Process-wide presence state: the coalescing checkpoint, its store, and the one
/// guard that keeps a heartbeat tick from becoming one flush per connection.
#[derive(Debug)]
pub(crate) struct PresenceState {
    checkpoint: PresenceCheckpoint,
    store: LastSeenStore,
    /// Held across a flush so a tick that finds it taken yields instead of
    /// duplicating the write. Presence is a property of the node, not of a
    /// connection, so only one connection may checkpoint per interval.
    flush_lock: tokio::sync::Mutex<()>,
}

impl PresenceState {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self {
            checkpoint: PresenceCheckpoint::default(),
            store: LastSeenStore::new(pool),
            flush_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Write every pending User's last-seen instant, clearing only on success.
    ///
    /// A failure keeps the batch so the next heartbeat retries: a checkpoint is
    /// never lost silently, and the cost of the retry is one statement, not a gap.
    async fn write_checkpoint(&self) -> Result<(), RealtimeError> {
        if self.checkpoint.is_empty() {
            return Ok(());
        }

        let batch = self.checkpoint.batch();
        self.store.save_last_seen(&batch).await?;
        self.checkpoint.clear();
        Ok(())
    }
}

impl RealtimeHub {
    /// Whether any Device of `user_id` is connected right now.
    pub async fn is_online(&self, user_id: &str) -> bool {
        self.registry.is_online(user_id).await
    }

    /// The presence of the requested Users that `viewer_id` is allowed to see.
    ///
    /// **Visibility is applied here, at the read**: only a User who shares a
    /// Conversation with the caller appears, so the endpoint answers about peers
    /// and cannot be used as a directory of strangers. Request order is preserved,
    /// and a requested User outside the audience is omitted rather than reported
    /// offline — the omission *is* the visibility answer.
    ///
    /// `last_seen_ms` is read only for Users who are offline: while a User is
    /// reachable the present is the answer and a past instant would be noise.
    pub async fn presence_for(
        &self,
        viewer_id: &str,
        targets: &[String],
    ) -> Result<Vec<PresenceView>, RealtimeError> {
        let visible: HashSet<String> = self
            .chat
            .presence_audience(viewer_id, Some(targets))
            .await?
            .into_iter()
            .collect();

        let offline: Vec<String> = targets
            .iter()
            .filter(|user_id| visible.contains(*user_id))
            .cloned()
            .collect();

        let stored = self.presence.store.last_seen(&offline).await?;

        let mut presences = Vec::with_capacity(offline.len());
        for user_id in offline {
            let online = self.registry.is_online(&user_id).await;
            presences.push(PresenceView {
                status: if online {
                    PresenceStatus::Online
                } else {
                    PresenceStatus::Offline
                },
                last_seen_ms: if online {
                    None
                } else {
                    stored.get(&user_id).copied()
                },
                user_id,
            });
        }

        Ok(presences)
    }

    /// Record that a User became reachable, and tell their audience.
    ///
    /// Called only for the User's **first** live Device (the registry decides), so
    /// a second Device never produces a second `online`.
    pub(crate) async fn user_came_online(&self, user_id: &str) {
        // Pending, not written: the next heartbeat stamps the User, and stamping
        // now means a crash between here and then loses nothing extra.
        self.presence.checkpoint.mark(user_id);
        self.broadcast_presence(user_id, PresenceStatus::Online, None)
            .await;
    }

    /// Record that a User stopped being reachable, persist the instant, and tell
    /// their audience.
    ///
    /// Called only for the User's **last** live Device. The checkpoint is flushed
    /// here rather than left to the heartbeat: the User has left the online set, so
    /// this is the last chance to record *when*, and a restart before the next beat
    /// would otherwise lose the whole session's instant.
    pub(crate) async fn user_went_offline(&self, user_id: &str) {
        let last_seen_ms = crate::now_ms().ok();
        self.presence.checkpoint.mark(user_id);
        if let Err(error) = self.presence.write_checkpoint().await {
            tracing::warn!(%error, "could not persist a last-seen instant; it will retry on the next heartbeat");
        }

        self.broadcast_presence(user_id, PresenceStatus::Offline, last_seen_ms)
            .await;
    }

    /// The heartbeat tick's presence work: stamp every reachable User.
    ///
    /// The flush lock makes this one write per interval rather than one per
    /// connection; a tick that cannot take the lock yields, and its Users are
    /// covered by the connection that did.
    pub(crate) async fn checkpoint_presence(&self) {
        let Ok(_guard) = self.presence.flush_lock.try_lock() else {
            return;
        };

        let online = self.registry.online_user_ids().await;
        self.presence.checkpoint.mark_many(&online);

        if let Err(error) = self.presence.write_checkpoint().await {
            tracing::warn!(%error, "could not checkpoint presence; it will retry on the next heartbeat");
        }
    }

    /// How long a silent connection may hold a User online before the sweep ends it.
    pub fn dead_connection_timeout(&self) -> std::time::Duration {
        self.dead_connection_timeout
    }

    /// Deliver one User's presence change to the Participants of their shared
    /// Conversations.
    ///
    /// A resolution failure is logged and swallowed: presence is advisory state,
    /// and a database hiccup must not cost a User their connection. An empty
    /// audience (a User in no Conversation) sends nothing — which is the correct
    /// outcome, not an error.
    async fn broadcast_presence(
        &self,
        user_id: &str,
        status: PresenceStatus,
        last_seen_ms: Option<i64>,
    ) {
        let audience = match self.chat.presence_audience(user_id, None).await {
            Ok(audience) => audience,
            Err(error) => {
                tracing::warn!(%error, "could not resolve a presence audience");
                return;
            }
        };

        if audience.is_empty() {
            return;
        }

        self.registry
            .deliver(
                &audience,
                &ServerEvent::Presence(PresenceView {
                    user_id: user_id.to_owned(),
                    status,
                    last_seen_ms,
                }),
            )
            .await;
    }
}

/// A stored `TIMESTAMPTZ` as milliseconds since the Unix epoch.
fn unix_millis(at: OffsetDateTime) -> i64 {
    at.unix_timestamp() * 1_000 + i64::from(at.millisecond())
}

#[cfg(test)]
mod tests {
    use super::{PresenceCheckpoint, unix_millis};
    use crate::PRESENCE_CHECKPOINT_BATCH;
    use sqlx::types::time::OffsetDateTime;

    fn user(index: usize) -> String {
        format!("01JABC1234567890ABCDEFGH{index:02}")
    }

    #[test]
    fn a_new_checkpoint_is_empty() {
        assert!(PresenceCheckpoint::default().is_empty());
        assert!(PresenceCheckpoint::default().batch().is_empty());
    }

    #[test]
    fn many_marks_of_one_user_collapse_to_one_entry() {
        let checkpoint = PresenceCheckpoint::default();

        // A User flapping between two Devices, marked on every transition.
        for _ in 0..100 {
            checkpoint.mark("01JABC1234567890ABCDEFGHJ3");
        }

        assert_eq!(
            checkpoint.batch().len(),
            1,
            "one User is one row on the next flush, not a hundred"
        );
    }

    #[test]
    fn the_batch_bound_is_where_an_early_flush_is_asked_for() {
        let checkpoint = PresenceCheckpoint::default();

        for index in 0..PRESENCE_CHECKPOINT_BATCH {
            let requested = checkpoint.mark(&user(index));
            let expected = index + 1 >= PRESENCE_CHECKPOINT_BATCH;
            assert_eq!(
                requested, expected,
                "only the batch-th distinct User asks for an early flush"
            );
        }

        assert_eq!(checkpoint.batch().len(), PRESENCE_CHECKPOINT_BATCH);
    }

    #[test]
    fn marking_many_uses_the_same_bound_and_deduplicates() {
        let checkpoint = PresenceCheckpoint::default();
        let ids: Vec<String> = (0..10).map(user).collect();

        assert!(!checkpoint.mark_many(&ids));
        assert!(!checkpoint.mark_many(&ids), "a repeated id is a no-op");
        assert_eq!(checkpoint.batch().len(), 10);
    }

    #[test]
    fn clearing_after_a_write_leaves_nothing_behind() {
        let checkpoint = PresenceCheckpoint::default();
        checkpoint.mark(&user(1));
        checkpoint.mark(&user(2));
        assert_eq!(checkpoint.batch().len(), 2);

        checkpoint.clear();

        assert!(checkpoint.is_empty());
    }

    #[test]
    fn a_stored_instant_reads_back_as_the_same_number_of_milliseconds() {
        let at = OffsetDateTime::from_unix_timestamp(1_750_000_000)
            .expect("a representable instant")
            + std::time::Duration::from_millis(123);

        assert_eq!(unix_millis(at), 1_750_000_000_123);
    }
}
