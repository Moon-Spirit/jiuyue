//! The per-connection Sync Cursor write-coalescing buffer.
//!
//! A client reports how far it has consumed a Conversation whenever it applies
//! Messages, which can be far more often than the database should be written to.
//! [`CursorCheckpoint`] is the guard: it keeps **one entry per Conversation**, so a
//! thousand reports about one Conversation collapse to a single row on the next
//! flush, and it only asks to be flushed eagerly once it holds a whole batch of
//! distinct Conversations.
//!
//! It is deliberately pure — no database, no connection, no clock — so the
//! coalescing rule (highest position wins, one entry per Conversation, bounded
//! growth) is unit-testable without a socket. The connection's session owns the
//! I/O around it.
//!
//! # Why highest wins
//!
//! Several connections of one Device may report the same Conversation, and a
//! replayed report can arrive after a newer one. A Sync Cursor only ever moves
//! forward (CONTEXT.md: it is a consumed position), so a lower value must be a
//! no-op rather than a rewind. The database applies the same `GREATEST` rule, which
//! makes the two layers agree.

use std::collections::HashMap;

use jiuyue_contract::SyncCursor;

use crate::CURSOR_CHECKPOINT_BATCH;

/// One connection's unflushed Sync Cursor reports, keyed by Conversation.
#[derive(Debug, Default)]
pub(crate) struct CursorCheckpoint {
    pending: HashMap<String, i64>,
}

impl CursorCheckpoint {
    /// Record a reported position, keeping the highest value per Conversation.
    ///
    /// Returns `true` once the buffer holds [`CURSOR_CHECKPOINT_BATCH`] distinct
    /// Conversations, which is the caller's signal to flush without waiting for the
    /// next heartbeat. Returning a decision instead of flushing here is what keeps
    /// this type free of I/O.
    pub(crate) fn record(&mut self, cursor: &SyncCursor) -> bool {
        self.pending
            .entry(cursor.conversation_id.clone())
            .and_modify(|value| *value = (*value).max(cursor.last_seq))
            .or_insert(cursor.last_seq);

        self.pending.len() >= CURSOR_CHECKPOINT_BATCH
    }

    /// Whether nothing is waiting to be checkpointed.
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Every pending report as a batch, in no particular order.
    ///
    /// A snapshot rather than a drain: a failed write must not lose progress, so the
    /// caller clears only after the database accepted the batch.
    pub(crate) fn batch(&self) -> Vec<SyncCursor> {
        self.pending
            .iter()
            .map(|(conversation_id, last_seq)| SyncCursor {
                conversation_id: conversation_id.clone(),
                last_seq: *last_seq,
            })
            .collect()
    }

    /// Forget everything, after a successful write.
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::CursorCheckpoint;
    use crate::CURSOR_CHECKPOINT_BATCH;
    use jiuyue_contract::SyncCursor;

    /// A report for `conversation_id` at `last_seq`.
    fn cursor(conversation_id: &str, last_seq: i64) -> SyncCursor {
        SyncCursor {
            conversation_id: conversation_id.to_owned(),
            last_seq,
        }
    }

    /// The single entry's position, panicking unless exactly one is pending.
    fn only_position(checkpoint: &CursorCheckpoint) -> i64 {
        let batch = checkpoint.batch();
        assert_eq!(batch.len(), 1, "exactly one Conversation was reported");
        batch[0].last_seq
    }

    #[test]
    fn a_new_buffer_is_empty() {
        assert!(CursorCheckpoint::default().is_empty());
    }

    #[test]
    fn many_reports_about_one_conversation_collapse_to_one_entry() {
        let mut checkpoint = CursorCheckpoint::default();

        // The client consumed a hundred Messages, one report each.
        for last_seq in 1..=100 {
            checkpoint.record(&cursor("c1", last_seq));
        }

        assert!(!checkpoint.is_empty());
        assert_eq!(
            only_position(&checkpoint),
            100,
            "one row and one statement, not a hundred"
        );
    }

    #[test]
    fn a_lower_position_never_rewinds_a_higher_one() {
        let mut checkpoint = CursorCheckpoint::default();

        assert!(!checkpoint.record(&cursor("c1", 9)));
        // A replayed or out-of-order report arrives afterwards.
        assert!(!checkpoint.record(&cursor("c1", 4)));

        assert_eq!(only_position(&checkpoint), 9);
    }

    #[test]
    fn the_batch_bound_is_where_eager_flushing_is_asked_for() {
        let mut checkpoint = CursorCheckpoint::default();

        // Distinct Conversations fill the buffer one entry at a time.
        for index in 0..CURSOR_CHECKPOINT_BATCH {
            let requested = checkpoint.record(&cursor(&format!("c{index}"), 1));
            let expected = index + 1 >= CURSOR_CHECKPOINT_BATCH;
            assert_eq!(
                requested, expected,
                "only the batch-th distinct Conversation asks for a flush"
            );
        }

        assert_eq!(checkpoint.batch().len(), CURSOR_CHECKPOINT_BATCH);
    }

    #[test]
    fn clearing_after_a_write_leaves_nothing_behind() {
        let mut checkpoint = CursorCheckpoint::default();
        checkpoint.record(&cursor("c1", 3));
        checkpoint.record(&cursor("c2", 5));
        assert_eq!(checkpoint.batch().len(), 2);

        checkpoint.clear();

        assert!(checkpoint.is_empty());
        assert!(checkpoint.batch().is_empty());
    }
}
