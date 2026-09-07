//! JiuYue pure domain logic.
//!
//! Zero-IO layer: sequence allocation abstraction, message lifecycle state
//! machine, and recall-window policy. Database/Redis adapters live in the
//! server crate; this crate must stay free of sqlx/redis dependencies.

mod progress;
mod recall;
mod seq;
mod state_machine;

pub use progress::{
    level_from_xp, title_for_level, total_xp_for, xp_for_level, DAILY_LOGIN_XP, MSG_XP_CHAR_BLOCK,
    MSG_XP_DAILY_CAP, MSG_XP_PER_BLOCK,
};
pub use recall::{RecallError, RecallPolicy};
pub use seq::{InMemorySeqAllocator, SeqAllocator};
pub use state_machine::{MessageStateMachine, MessageStatus, TransitionEvent};

/// Domain error type for all pure logic in this crate.
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum DomainError {
    /// A state transition was requested that the state machine forbids.
    ///
    /// The payload names the offending `(current status, event)` pair — e.g.
    /// `"Recalled --MarkSent-->"` — so callers can log exactly what was
    /// rejected instead of just "something failed".
    #[error("illegal transition: {0}")]
    IllegalTransition(&'static str),
    /// An internal synchronization primitive was poisoned by a panic in
    /// another thread. Pure logic cannot recover from this; callers should
    /// treat it as a bug in the owning service rather than retry.
    #[error("lock poisoned: {0}")]
    LockPoisoned(&'static str),
}
