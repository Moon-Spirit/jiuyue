//! Bounded per-connection replay buffer.
//!
//! ADR-0003 keeps a **short-term replay buffer** at the connection layer (never in
//! persistent storage): a client that notices a hole in the envelope sequence `s`
//! asks the server to re-send what it missed, and the server answers from here
//! rather than re-reading a Conversation.
//!
//! # Bounded by construction
//!
//! The buffer is a ring of the most recently sent envelopes, capped at a
//! compile-time capacity. On the 2 vCPU / 2 GB production box an unbounded
//! per-connection backlog would be a memory bomb, so a position older than the
//! oldest retained envelope is answered with [`ReplayBuffer::after`] returning
//! `None` — "cannot replay" — and the client falls back to repairing each
//! Conversation from its own cursor. Dropping the oldest envelope is therefore a
//! deliberate, correct degradation, not data loss: the Conversation stream is the
//! durable record.

use std::collections::VecDeque;

use jiuyue_contract::ServerEnvelope;

/// How many recent envelopes one connection retains for gap repair.
///
/// Every envelope is a small JSON object (a heartbeat, an ack, or one Message);
/// 128 of them is on the order of tens of kilobytes per connection, so even
/// hundreds of simultaneous connections cost a few megabytes. It is comfortably
/// larger than the registry's bounded fan-out queue (64 events), which is the
/// only way an envelope is dropped in the first place, so a client that briefly
/// stops draining can always be caught up without a Conversation repair.
pub const DEFAULT_REPLAY_CAPACITY: usize = 128;

/// The most recent envelopes sent on one connection, newest last.
#[derive(Debug)]
pub struct ReplayBuffer {
    capacity: usize,
    envelopes: VecDeque<ServerEnvelope>,
}

impl ReplayBuffer {
    /// Build a buffer holding at most `capacity` envelopes.
    ///
    /// A capacity of zero is treated as one: a caller that asks for a buffer must
    /// get at least the newest envelope, otherwise replay could never succeed.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            envelopes: VecDeque::new(),
        }
    }

    /// Record an envelope that was just enqueued for delivery.
    ///
    /// When the buffer is full the oldest envelope is evicted. That is the bound:
    /// positions at or beyond the new oldest can still be replayed, older ones
    /// cannot and are reported as such by [`Self::after`].
    pub fn record(&mut self, envelope: &ServerEnvelope) {
        if self.envelopes.len() == self.capacity {
            self.envelopes.pop_front();
        }
        self.envelopes.push_back(envelope.clone());
    }

    /// How many envelopes are currently retained.
    pub fn len(&self) -> usize {
        self.envelopes.len()
    }

    /// Whether nothing has been sent on this connection yet.
    pub fn is_empty(&self) -> bool {
        self.envelopes.is_empty()
    }

    /// The envelopes strictly after `last_seq`, when continuity can be proven.
    ///
    /// `None` means the buffer cannot satisfy the request: it is empty, the wanted
    /// envelope (`last_seq + 1`) has been evicted, or `last_seq` is newer than
    /// anything sent on this connection (a stale sequence from a previous
    /// connection, or a bogus value). `Some(vec)` — possibly empty, when the client
    /// is already caught up — means the client needs no Conversation repair.
    pub fn after(&self, last_seq: u64) -> Option<Vec<ServerEnvelope>> {
        let oldest = self.envelopes.front()?.sequence();
        let newest = self.envelopes.back()?.sequence();

        // The client consumed everything up to `last_seq`; replay starts at the
        // next envelope, which must still be retained.
        let next = last_seq.checked_add(1)?;
        if next < oldest {
            // Evicted: there is a hole the buffer can no longer fill.
            return None;
        }
        if last_seq > newest {
            // A position this connection never reached — treat it as unprovable.
            return None;
        }

        Some(
            self.envelopes
                .iter()
                .filter(|envelope| envelope.sequence() > last_seq)
                .cloned()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_REPLAY_CAPACITY, ReplayBuffer};
    use jiuyue_contract::{Ping, ServerEnvelope, ServerEvent};

    /// A minimal envelope carrying `sequence`, for buffer bookkeeping only.
    fn envelope(sequence: u64) -> ServerEnvelope {
        ServerEnvelope::new(
            sequence,
            1_700_000_000_000,
            ServerEvent::Ping(Ping {
                seq: sequence,
                time_ms: 1_700_000_000_000,
                connection_id: None,
            }),
        )
    }

    /// Fill a buffer with `count` envelopes, sequences 1..=count.
    fn filled(capacity: usize, count: u64) -> ReplayBuffer {
        let mut buffer = ReplayBuffer::new(capacity);
        for sequence in 1..=count {
            buffer.record(&envelope(sequence));
        }
        buffer
    }

    /// The sequences of a replay result, in order.
    fn sequences(envelopes: &[ServerEnvelope]) -> Vec<u64> {
        envelopes.iter().map(ServerEnvelope::sequence).collect()
    }

    #[test]
    fn capacity_default_is_bounded() {
        assert_eq!(ReplayBuffer::new(DEFAULT_REPLAY_CAPACITY).len(), 0);
        assert_eq!(ReplayBuffer::new(0).len(), 0);
        let mut buffer = ReplayBuffer::new(0);
        buffer.record(&envelope(1));
        assert_eq!(buffer.len(), 1, "a zero capacity still keeps the newest");
    }

    #[test]
    fn recording_beyond_capacity_evicts_the_oldest() {
        let buffer = filled(3, 5);

        assert_eq!(buffer.len(), 3, "the buffer never exceeds its capacity");
        assert_eq!(
            buffer.after(2).map(|replay| sequences(&replay)),
            Some(vec![3, 4, 5]),
            "the retained tail is the three newest envelopes"
        );
    }

    #[test]
    fn a_contiguous_tail_is_replayable() {
        let buffer = filled(DEFAULT_REPLAY_CAPACITY, 5);

        assert_eq!(
            buffer.after(2).map(|replay| sequences(&replay)),
            Some(vec![3, 4, 5])
        );
        assert_eq!(
            buffer.after(0).map(|replay| sequences(&replay)),
            Some(vec![1, 2, 3, 4, 5]),
            "a client that consumed nothing replays from the beginning"
        );
    }

    #[test]
    fn an_already_caught_up_client_replays_nothing() {
        let buffer = filled(DEFAULT_REPLAY_CAPACITY, 5);

        assert_eq!(
            buffer.after(5).map(|replay| sequences(&replay)),
            Some(vec![]),
            "consuming the newest envelope is a valid, empty replay"
        );
    }

    #[test]
    fn an_evicted_position_cannot_be_replayed() {
        let buffer = filled(3, 5);

        assert_eq!(
            buffer.after(1),
            None,
            "envelope 2 was evicted, so the hole is unfillable"
        );
    }

    #[test]
    fn a_position_newer_than_this_connection_is_unprovable() {
        let buffer = filled(DEFAULT_REPLAY_CAPACITY, 3);

        assert_eq!(
            buffer.after(9),
            None,
            "a sequence this connection never sent cannot be resumed"
        );
    }

    #[test]
    fn an_empty_buffer_has_nothing_to_replay() {
        let buffer = ReplayBuffer::new(DEFAULT_REPLAY_CAPACITY);

        assert!(buffer.is_empty());
        assert_eq!(buffer.after(0), None);
    }
}
