//! Server-side liveness for one connection.
//!
//! A half-open TCP connection — a laptop whose Wi-Fi died, a network that was
//! severed without a FIN or an RST — is byte-for-byte indistinguishable from a
//! healthy one. ADR-0013 answers the *client's* half of that problem with the
//! server heartbeat and a staleness watchdog; the *server's* half needs evidence
//! flowing the other way, and the only such evidence the contract already has is
//! the client heartbeat ([`jiuyue_contract::ClientEvent::Ping`]).
//!
//! [`ConnectionLiveness`] is that evidence, and nothing more: a timestamp of the
//! last heartbeat the peer sent. It is deliberately **opt-in by convention** — a
//! connection that has never sent a heartbeat is never considered expired, so a
//! client written against the old contract (which never sent one) keeps working
//! exactly as it did. The client that does send heartbeats is the one we can hold
//! to a deadline, and the one the sweep can honestly declare dead.
//!
//! The type is pure — an [`Instant`] in, a bool out — so the bound is unit-tested
//! without a socket or a timer.

use std::time::{Duration, Instant};

/// The last heartbeat a peer sent on one connection.
#[derive(Debug, Default)]
pub(crate) struct ConnectionLiveness {
    /// When the peer last proved it was there; `None` until its first heartbeat.
    last_beat: Option<Instant>,
}

impl ConnectionLiveness {
    /// Record a heartbeat from the peer.
    pub(crate) fn record_beat(&mut self, now: Instant) {
        self.last_beat = Some(now);
    }

    /// Whether the peer has missed its heartbeat deadline.
    ///
    /// A connection that has never beaten is **not** expired: the deadline only
    /// applies to a peer that agreed to it by sending heartbeats in the first
    /// place. That is what keeps the sweep additive — an older client is never
    /// disconnected for a behaviour it never adopted.
    pub(crate) fn is_expired(&self, deadline: Duration, now: Instant) -> bool {
        match self.last_beat {
            Some(last_beat) => now.saturating_duration_since(last_beat) > deadline,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConnectionLiveness;
    use std::time::{Duration, Instant};

    const DEADLINE: Duration = Duration::from_secs(90);

    #[test]
    fn a_connection_that_never_beats_is_never_expired() {
        // The additive-compatibility rule: no heartbeat, no deadline to miss.
        let liveness = ConnectionLiveness::default();
        let now = Instant::now();

        assert!(
            !liveness.is_expired(DEADLINE, now + Duration::from_secs(86_400)),
            "no heartbeat means no deadline to miss"
        );
    }

    #[test]
    fn a_recent_heartbeat_keeps_the_connection_alive() {
        let now = Instant::now();
        let mut liveness = ConnectionLiveness::default();
        liveness.record_beat(now - Duration::from_secs(30));

        assert!(!liveness.is_expired(DEADLINE, now));
    }

    #[test]
    fn a_missed_deadline_expires_the_connection() {
        let now = Instant::now();
        let mut liveness = ConnectionLiveness::default();
        liveness.record_beat(now - Duration::from_secs(91));

        assert!(
            liveness.is_expired(DEADLINE, now),
            "three missed 30 s heartbeats must read as dead"
        );
    }

    #[test]
    fn the_deadline_is_exclusive_at_its_boundary() {
        let now = Instant::now();
        let mut liveness = ConnectionLiveness::default();
        liveness.record_beat(now - DEADLINE);

        assert!(
            !liveness.is_expired(DEADLINE, now),
            "exactly at the deadline is not yet late"
        );
    }

    #[test]
    fn a_fresh_heartbeat_revives_an_expired_connection() {
        let now = Instant::now();
        let mut liveness = ConnectionLiveness::default();
        liveness.record_beat(now - Duration::from_secs(120));
        assert!(liveness.is_expired(DEADLINE, now));

        liveness.record_beat(now);

        assert!(!liveness.is_expired(DEADLINE, now));
    }
}
