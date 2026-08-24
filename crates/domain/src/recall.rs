//! Recall-window policy (pure function).
//!
//! WHY: Telegram-style recall — the sender may unsend within a short window.
//! This module answers exactly two questions ("is this requester the
//! sender?" and "is the message still inside the window?") as a stateless
//! pure function. It deliberately knows nothing about message *state*:
//! whether a message was already recalled is the [`crate::MessageStateMachine`]'s
//! job (a tombstone is terminal, so double recall is rejected there). The
//! server composes both checks: `policy.can_recall(...)?` then
//! `machine.transition(Recall)?`.
//!
//! `now` is always injected by the caller — no clock reads in domain logic —
//! which keeps every boundary deterministic in tests.

use time::OffsetDateTime;
use uuid::Uuid;

/// Why a recall request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecallError {
    /// Only the original sender may recall a message.
    #[error("only the sender may recall a message")]
    NotSender,
    /// The message was sent outside the recall window.
    #[error("recall window expired")]
    WindowExpired,
}

/// Sender-only, fixed-window recall rule (default 120s, Telegram-style).
///
/// Stateless and pure: `now` is injected so tests pin every boundary. It
/// intentionally does NOT know whether a message was already recalled —
/// that is a state question owned by [`crate::MessageStateMachine`] (a
/// tombstone is terminal, so double recall is an illegal transition there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallPolicy {
    window_secs: u64,
}

impl RecallPolicy {
    /// Default recall window: 2 minutes, matching the MVP spec
    /// ("发送者 2 分钟内可撤").
    pub const DEFAULT_WINDOW_SECS: u64 = 120;

    /// Creates a policy with a custom window in seconds.
    pub fn new(window_secs: u64) -> Self {
        Self { window_secs }
    }

    /// The configured window length in seconds.
    pub fn window_secs(&self) -> u64 {
        self.window_secs
    }

    /// Checks whether `requester_id` may recall a message sent by
    /// `message_sender_id` at `sent_at`, judged at instant `now`.
    ///
    /// The window is inclusive at its upper bound (`elapsed == window_secs`
    /// passes). Identity is checked before time, so a non-sender gets
    /// [`RecallError::NotSender`] even when the window has also expired.
    /// Slightly negative elapsed time (clock skew between instances) is
    /// tolerated — only positive elapsed time can expire the window.
    pub fn can_recall(
        &self,
        requester_id: Uuid,
        message_sender_id: Uuid,
        sent_at: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<(), RecallError> {
        if requester_id != message_sender_id {
            return Err(RecallError::NotSender);
        }
        let elapsed = now - sent_at;
        if elapsed.whole_seconds() > self.window_secs as i64 {
            return Err(RecallError::WindowExpired);
        }
        Ok(())
    }
}

impl Default for RecallPolicy {
    fn default() -> Self {
        Self {
            window_secs: Self::DEFAULT_WINDOW_SECS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    /// Fixed epoch seconds; no clock or RNG involved.
    fn at(epoch_secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(epoch_secs).unwrap()
    }

    fn sender() -> Uuid {
        Uuid::from_u128(0xA001)
    }

    fn other_user() -> Uuid {
        Uuid::from_u128(0xB002)
    }

    #[test]
    fn default_window_is_120_seconds() {
        assert_eq!(RecallPolicy::default().window_secs(), 120);
        assert_eq!(RecallPolicy::DEFAULT_WINDOW_SECS, 120);
    }

    #[test]
    fn sender_within_window_can_recall() {
        let policy = RecallPolicy::default();
        assert_eq!(
            policy.can_recall(sender(), sender(), at(1_000_000), at(1_000_060)),
            Ok(())
        );
    }

    #[test]
    fn boundary_exactly_at_window_limit_is_allowed() {
        // "2 分钟内可撤" is inclusive: elapsed == window still passes.
        let policy = RecallPolicy::default();
        assert_eq!(
            policy.can_recall(sender(), sender(), at(1_000_000), at(1_000_120)),
            Ok(())
        );
    }

    #[test]
    fn one_second_past_window_is_expired() {
        let policy = RecallPolicy::default();
        assert_eq!(
            policy.can_recall(sender(), sender(), at(1_000_000), at(1_000_121)),
            Err(RecallError::WindowExpired)
        );
    }

    #[test]
    fn non_sender_is_rejected_even_inside_window() {
        let policy = RecallPolicy::default();
        assert_eq!(
            policy.can_recall(other_user(), sender(), at(1_000_000), at(1_000_001)),
            Err(RecallError::NotSender)
        );
    }

    #[test]
    fn non_sender_outside_window_reports_not_sender_first() {
        // Identity is checked before time so the error names the stronger
        // violation; documented behavior, pinned here.
        let policy = RecallPolicy::default();
        assert_eq!(
            policy.can_recall(other_user(), sender(), at(1_000_000), at(1_999_999)),
            Err(RecallError::NotSender)
        );
    }

    #[test]
    fn negative_elapsed_clock_skew_is_tolerated() {
        // `now` slightly before `sent_at` (multi-instance clock skew) must
        // not expire the window — only positive elapsed time can.
        let policy = RecallPolicy::default();
        assert_eq!(
            policy.can_recall(sender(), sender(), at(1_000_100), at(1_000_070)),
            Ok(())
        );
    }
}
