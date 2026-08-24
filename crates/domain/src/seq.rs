//! Per-conversation monotonic sequence allocation.
//!
//! WHY: JiuYue separates message identity (UUIDv7, application-generated)
//! from ordering (per-conversation `seq BIGINT`). This is the Telegram pts
//! model: each conversation has a monotonically increasing counter and every
//! message takes the next value. Clients use `seq` as a sync cursor
//! (`member_state.last_delivered_seq / last_read_seq`), so the only hard
//! guarantee needed is *strictly increasing per conversation* — gaps are
//! legal (a lost write must not stall the cursor), duplicates are not.
//!
//! The DB-backed implementation (ticket 05) allocates inside the message
//! INSERT transaction via `UPDATE conversations SET last_seq = last_seq + 1
//! ... RETURNING last_seq`. This trait exists so domain logic and tests can
//! run without a database.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::DomainError;

/// Allocates conversation-scoped, strictly monotonic sequence numbers.
///
/// Contract:
/// - For a given `conversation_id`, returned values are strictly increasing.
/// - Gaps between values are legal; duplicates within one conversation are not.
/// - Different conversations are fully independent counters.
///
/// `Send + Sync` is required because the server shares one allocator across
/// connection-handler tasks.
pub trait SeqAllocator: Send + Sync {
    /// Returns the next sequence number for `conversation_id`.
    fn next_seq(&self, conversation_id: i64) -> Result<i64, DomainError>;
}

/// In-memory [`SeqAllocator`] used by tests and local bootstrap.
///
/// Starts every conversation at 0 internally and returns `last + 1`, so the
/// first allocated seq for any conversation is 1. A `Mutex<HashMap>` keeps
/// allocation atomic under concurrency — uniqueness is guaranteed even when
/// many threads allocate for the same conversation interleaved.
#[derive(Debug, Default)]
pub struct InMemorySeqAllocator {
    /// conversation_id -> last allocated seq (0 = nothing allocated yet).
    counters: Mutex<HashMap<i64, i64>>,
}

impl InMemorySeqAllocator {
    /// Creates an empty allocator.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SeqAllocator for InMemorySeqAllocator {
    fn next_seq(&self, conversation_id: i64) -> Result<i64, DomainError> {
        let mut counters = self
            .counters
            .lock()
            .map_err(|_| DomainError::LockPoisoned("InMemorySeqAllocator"))?;
        let counter = counters.entry(conversation_id).or_insert(0);
        *counter += 1;
        Ok(*counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    #[test]
    fn fresh_conversation_starts_at_1() {
        let alloc = InMemorySeqAllocator::new();
        assert_eq!(alloc.next_seq(1).unwrap(), 1);
    }

    #[test]
    fn sequential_allocation_is_strictly_monotonic_per_conversation() {
        let alloc = InMemorySeqAllocator::new();
        let seqs: Vec<i64> = (0..100).map(|_| alloc.next_seq(42).unwrap()).collect();
        let expected: Vec<i64> = (1..=100).collect();
        assert_eq!(seqs, expected);
    }

    #[test]
    fn interleaved_conversations_have_independent_counters() {
        let alloc = InMemorySeqAllocator::new();
        // Alternate conversations; each counts from 1 independently.
        assert_eq!(alloc.next_seq(1).unwrap(), 1);
        assert_eq!(alloc.next_seq(2).unwrap(), 1);
        assert_eq!(alloc.next_seq(1).unwrap(), 2);
        assert_eq!(alloc.next_seq(2).unwrap(), 2);
        assert_eq!(alloc.next_seq(1).unwrap(), 3);
        assert_eq!(alloc.next_seq(2).unwrap(), 3);
    }

    #[test]
    fn concurrent_allocation_on_one_conversation_is_unique_and_monotonic() {
        const THREADS: usize = 8;
        const ALLOCS_PER_THREAD: usize = 200;

        // Allocate through the trait object to also prove the trait object
        // itself is usable from multiple threads (Send + Sync bound).
        let alloc: Arc<dyn SeqAllocator> = Arc::new(InMemorySeqAllocator::new());
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    (0..ALLOCS_PER_THREAD)
                        .map(|_| alloc.next_seq(7).unwrap())
                        .collect::<Vec<i64>>()
                })
            })
            .collect();

        let mut all: Vec<i64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(all.len(), THREADS * ALLOCS_PER_THREAD);

        let total = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(
            all.len(),
            total,
            "duplicate seq allocated under concurrency"
        );
        assert_eq!(all[0], 1, "first ever allocation must be 1");
        assert_eq!(
            all[total - 1],
            total as i64,
            "no seq may be skipped by the in-memory implementation"
        );
    }

    #[test]
    fn concurrent_interleaved_conversations_stay_independent() {
        const THREADS: usize = 4;
        const CONVERSATIONS: [i64; 3] = [11, 22, 33];
        const PER_THREAD_PER_CONV: usize = 50;

        let alloc: Arc<dyn SeqAllocator> = Arc::new(InMemorySeqAllocator::new());
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    CONVERSATIONS
                        .iter()
                        .flat_map(|&conv| {
                            let alloc = Arc::clone(&alloc);
                            (0..PER_THREAD_PER_CONV)
                                .map(move |_| (conv, alloc.next_seq(conv).unwrap()))
                        })
                        .collect::<Vec<(i64, i64)>>()
                })
            })
            .collect();

        let mut per_conv: HashMap<i64, Vec<i64>> = HashMap::new();
        for handle in handles {
            for (conv, seq) in handle.join().unwrap() {
                per_conv.entry(conv).or_default().push(seq);
            }
        }

        let expected_count = THREADS * PER_THREAD_PER_CONV;
        let mut seen_convs = HashSet::new();
        for (conv, mut seqs) in per_conv {
            seen_convs.insert(conv);
            assert_eq!(seqs.len(), expected_count, "conv {conv} lost allocations");
            seqs.sort_unstable();
            seqs.dedup();
            assert_eq!(seqs.len(), expected_count, "conv {conv} duplicate seq");
            assert_eq!(seqs[0], 1, "conv {conv} must start at 1");
            assert_eq!(
                seqs[expected_count - 1],
                expected_count as i64,
                "conv {conv} must be contiguous 1..={expected_count}"
            );
        }
        assert_eq!(seen_convs.len(), CONVERSATIONS.len());
    }
}
