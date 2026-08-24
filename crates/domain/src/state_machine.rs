//! Message lifecycle state machine.
//!
//! WHY: a message is not a mutable blob — it moves through a fixed lifecycle
//! (`sending → sent → delivered → read → recalled`) driven by explicit events
//! (server ACK, receiver ACKs, read receipts, recall). Encoding the legal
//! edges in one place means every other layer (WS handlers, DB writes) can
//! trust that an observed status was reached legally.
//!
//! Recall is modeled as a *tombstone*: content is removed but the record
//! stays so receivers learn about the removal through the normal seq-based
//! sync. That is why `Read → Recalled` is legal — having been read does not
//! shield a message from recall — while `Recalled → anything` is illegal
//! because a tombstone is terminal.

use crate::DomainError;

/// Lifecycle status of a message.
///
/// Ordered along the delivery pipeline; `Recalled` is terminal (tombstone).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageStatus {
    /// Accepted from the client, not yet persisted/ACKed by the server.
    Sending,
    /// Persisted and ACKed to the sender (persist-then-ack semantics).
    Sent,
    /// Delivered to at least one of the recipient's devices.
    Delivered,
    /// Seen by the recipient (read receipt applied).
    Read,
    /// Recalled by the sender: content removed, tombstone record remains so
    /// receivers learn the removal via normal seq-based sync.
    Recalled,
}

/// Events that drive [`MessageStateMachine`] transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionEvent {
    /// Server persisted the message (persist-then-ack).
    MarkSent,
    /// A recipient device acknowledged delivery.
    MarkDelivered,
    /// The recipient read the message.
    MarkRead,
    /// The sender recalled the message (subject to [`crate::RecallPolicy`]).
    Recall,
}

/// Enforces which lifecycle transitions are legal.
///
/// Legal edges:
/// `Sending→Sent`, `Sent→Delivered`, `Sent→Read`, `Delivered→Read`,
/// `Sent→Recalled`, `Delivered→Recalled`, `Read→Recalled`.
/// Everything else — self-transitions, skipping states, anything out of
/// `Recalled` — is rejected with [`DomainError::IllegalTransition`] naming
/// the offending pair. A rejected transition never mutates the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageStateMachine {
    status: MessageStatus,
}

impl MessageStateMachine {
    /// Creates a machine in the initial [`MessageStatus::Sending`] state
    /// (every message is born un-ACKed).
    pub fn new() -> Self {
        Self::from(MessageStatus::Sending)
    }

    /// Current status.
    pub fn status(&self) -> MessageStatus {
        self.status
    }

    /// Applies `event`; on success advances the internal status and returns
    /// the new one, otherwise returns an error naming the rejected pair and
    /// leaves the state untouched.
    pub fn transition(&mut self, event: TransitionEvent) -> Result<MessageStatus, DomainError> {
        let next = match (self.status, event) {
            (MessageStatus::Sending, TransitionEvent::MarkSent) => MessageStatus::Sent,
            (MessageStatus::Sent, TransitionEvent::MarkDelivered) => MessageStatus::Delivered,
            (MessageStatus::Sent, TransitionEvent::MarkRead) => MessageStatus::Read,
            (MessageStatus::Delivered, TransitionEvent::MarkRead) => MessageStatus::Read,
            (MessageStatus::Sent, TransitionEvent::Recall)
            | (MessageStatus::Delivered, TransitionEvent::Recall)
            | (MessageStatus::Read, TransitionEvent::Recall) => MessageStatus::Recalled,
            (from, ev) => return Err(DomainError::IllegalTransition(illegal_pair_name(from, ev))),
        };
        self.status = next;
        Ok(next)
    }
}

impl Default for MessageStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl From<MessageStatus> for MessageStateMachine {
    fn from(status: MessageStatus) -> Self {
        Self { status }
    }
}

/// Builds the static `(status --event-->)` pair name for an illegal
/// transition. Exhaustive over all 20 combinations so the compiler forces a
/// decision if a status or event is ever added.
fn illegal_pair_name(from: MessageStatus, event: TransitionEvent) -> &'static str {
    use MessageStatus::{Delivered, Read, Recalled, Sending, Sent};
    use TransitionEvent::{MarkDelivered, MarkRead, MarkSent, Recall};
    match (from, event) {
        (Sending, MarkSent) => "Sending --MarkSent-->",
        (Sending, MarkDelivered) => "Sending --MarkDelivered-->",
        (Sending, MarkRead) => "Sending --MarkRead-->",
        (Sending, Recall) => "Sending --Recall-->",
        (Sent, MarkSent) => "Sent --MarkSent-->",
        (Sent, MarkDelivered) => "Sent --MarkDelivered-->",
        (Sent, MarkRead) => "Sent --MarkRead-->",
        (Sent, Recall) => "Sent --Recall-->",
        (Delivered, MarkSent) => "Delivered --MarkSent-->",
        (Delivered, MarkDelivered) => "Delivered --MarkDelivered-->",
        (Delivered, MarkRead) => "Delivered --MarkRead-->",
        (Delivered, Recall) => "Delivered --Recall-->",
        (Read, MarkSent) => "Read --MarkSent-->",
        (Read, MarkDelivered) => "Read --MarkDelivered-->",
        (Read, MarkRead) => "Read --MarkRead-->",
        (Read, Recall) => "Read --Recall-->",
        (Recalled, MarkSent) => "Recalled --MarkSent-->",
        (Recalled, MarkDelivered) => "Recalled --MarkDelivered-->",
        (Recalled, MarkRead) => "Recalled --MarkRead-->",
        (Recalled, Recall) => "Recalled --Recall-->",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DomainError;

    /// The complete transition matrix: every `(status, event)` pair there is
    /// (5 statuses × 4 events = 20 rows). Legal rows expect the resulting
    /// status; illegal rows expect the offending pair named inside
    /// [`DomainError::IllegalTransition`].
    fn transition_matrix() -> Vec<(
        MessageStatus,
        TransitionEvent,
        Result<MessageStatus, &'static str>,
    )> {
        use MessageStatus::{Delivered, Read, Recalled, Sending, Sent};
        use TransitionEvent::{MarkDelivered, MarkRead, MarkSent, Recall};
        vec![
            // --- legal edges ---
            (Sending, MarkSent, Ok(Sent)),
            (Sent, MarkDelivered, Ok(Delivered)),
            (Sent, MarkRead, Ok(Read)),
            (Delivered, MarkRead, Ok(Read)),
            (Sent, Recall, Ok(Recalled)),
            (Delivered, Recall, Ok(Recalled)),
            (Read, Recall, Ok(Recalled)),
            // --- illegal: self-transitions and skips ---
            (Sending, MarkDelivered, Err("Sending --MarkDelivered-->")),
            (Sending, MarkRead, Err("Sending --MarkRead-->")),
            (Sending, Recall, Err("Sending --Recall-->")),
            (Sent, MarkSent, Err("Sent --MarkSent-->")),
            (Delivered, MarkSent, Err("Delivered --MarkSent-->")),
            (
                Delivered,
                MarkDelivered,
                Err("Delivered --MarkDelivered-->"),
            ),
            (Read, MarkSent, Err("Read --MarkSent-->")),
            (Read, MarkDelivered, Err("Read --MarkDelivered-->")),
            (Read, MarkRead, Err("Read --MarkRead-->")),
            // --- illegal: Recalled is terminal (double-recall included) ---
            (Recalled, MarkSent, Err("Recalled --MarkSent-->")),
            (Recalled, MarkDelivered, Err("Recalled --MarkDelivered-->")),
            (Recalled, MarkRead, Err("Recalled --MarkRead-->")),
            (Recalled, Recall, Err("Recalled --Recall-->")),
        ]
    }

    #[test]
    fn full_transition_matrix_matches_spec() {
        // Guards against silently shrinking the table: 5 × 4 pairs must all
        // be present exactly once.
        let matrix = transition_matrix();
        assert_eq!(matrix.len(), 20);
        let mut seen = std::collections::HashSet::new();
        for (status, event, _) in &matrix {
            assert!(
                seen.insert((*status, *event)),
                "duplicate row {status:?} x {event:?}"
            );
        }

        for (start, event, expected) in matrix {
            let mut sm = MessageStateMachine::from(start);
            match expected {
                Ok(to) => {
                    assert_eq!(
                        sm.transition(event),
                        Ok(to),
                        "{start:?} + {event:?} should reach {to:?}"
                    );
                    assert_eq!(sm.status(), to, "status must advance on success");
                }
                Err(pair) => {
                    assert_eq!(
                        sm.transition(event),
                        Err(DomainError::IllegalTransition(pair)),
                        "{start:?} + {event:?} must be rejected naming the pair"
                    );
                    assert_eq!(sm.status(), start, "failed transition must not mutate");
                }
            }
        }
    }

    #[test]
    fn new_message_starts_in_sending() {
        assert_eq!(MessageStateMachine::new().status(), MessageStatus::Sending);
    }

    #[test]
    fn full_happy_path_walks_every_legal_edge() {
        let mut sm = MessageStateMachine::new();
        assert_eq!(
            sm.transition(TransitionEvent::MarkSent),
            Ok(MessageStatus::Sent)
        );
        assert_eq!(
            sm.transition(TransitionEvent::MarkDelivered),
            Ok(MessageStatus::Delivered)
        );
        assert_eq!(
            sm.transition(TransitionEvent::MarkRead),
            Ok(MessageStatus::Read)
        );
        assert_eq!(
            sm.transition(TransitionEvent::Recall),
            Ok(MessageStatus::Recalled)
        );
    }

    #[test]
    fn double_recall_is_rejected_by_the_state_machine() {
        // The recall POLICY (identity + time window) is stateless; guarding
        // an already-recalled message is the state machine's job. A tombstone
        // is terminal, so the second Recall — even from the sender within the
        // window — is an illegal transition.
        let mut sm = MessageStateMachine::from(MessageStatus::Sent);
        assert_eq!(
            sm.transition(TransitionEvent::Recall),
            Ok(MessageStatus::Recalled)
        );
        assert_eq!(
            sm.transition(TransitionEvent::Recall),
            Err(DomainError::IllegalTransition("Recalled --Recall-->"))
        );
    }
}
