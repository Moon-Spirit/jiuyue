//! Typing Indicator: who is composing right now, and the throttle that keeps the
//! cheapest event in the protocol from becoming the loudest.
//!
//! # Why the throttle *is* the feature
//!
//! A typing signal is the cheapest possible event to send: two ids and a boolean.
//! It is therefore the easiest way to build an amplifier, because it is the one
//! event a client is tempted to send **per keystroke**. The arithmetic is brutal:
//!
//! - A 200-Participant Group, one careless client emitting one signal per key at
//!   5 keys/s, and one delivery per Participant: `200 * 5 = 1000` deliveries per
//!   second, per typer, on a 2 vCPU / 2 GB box (ADR-0005, ADR-0010).
//! - With the server throttle below (one broadcast per [`TypingLimits::throttle`]
//!   per Participant per Conversation, 5 s at production), the same minute of
//!   continuous typing costs at most `200 * ceil(60 / 5) = 2400` deliveries —
//!   `12` fan-outs instead of `300`, a 25x reduction, and it is flat in keystroke
//!   rate: a faster typist costs exactly the same.
//!
//! The throttle therefore lives on the **server**, not only in the client. A
//! client-side debounce is a courtesy that protects the network; a server-side
//! one is the guarantee that protects the node, and only the server can make it
//! against a client that is broken or hostile. The frontend still debounces (it
//! must not spend a frame per key), but the bound asserted by the tests is the
//! server's.
//!
//! # Ephemeral by construction
//!
//! [`TypingDirectory`] is a plain in-memory map with an [`Instant`] deadline. There
//! is no table in this crate for it, no transaction, and no Message id or Sequence
//! Number on the wire — a typing signal has nothing the persistence layer could
//! even key on. It is also absent from the connection replay buffer's guarantees:
//! replay is scoped to one `connection_id` (ADR-0013), and a reconnect is a new
//! connection, so a typing event can never be replayed across one. Correctness
//! after a restart is therefore structural: the map is empty, and every stale
//! indicator is gone with it.
//!
//! # Expiry is mandatory, a stop event is an optimisation
//!
//! A client that closes mid-sentence never sends a stop, so an indicator that only
//! cleared on a stop would leak. Every entry expires after [`TypingLimits::ttl`]
//! (10 s at production) on the **receiver's** own clock; the
//! [`Stopped`](jiuyue_contract::events::TypingState::Stopped) event is what makes
//! the common case instant rather than lagging by the bound. Sending a Message also
//! clears it ([`RealtimeHub::clear_typing`]).
//!
//! # Ordering: a stop can never cancel a later start
//!
//! Two connections may be involved (a user with a phone and a laptop), so
//! per-connection transport order is not enough. Every transition for a key is
//! applied **and fanned out while holding the directory lock** ([`TypingTracker`]),
//! which gives all receivers one total order. A `Stopped` only broadcasts while
//! the key is still active, so a stop arriving after a start the server ordered
//! later is a no-op rather than a cancellation — and a start that follows a stop
//! re-activates the key and broadcasts after it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jiuyue_contract::events::{ServerEvent, Typing, TypingSignal, TypingState};
use tokio::sync::Mutex;

use crate::RealtimeHub;

/// The two clocks a Typing Indicator runs on.
///
/// They are one value because they are only meaningful together: `ttl` must exceed
/// `throttle`, or continuous typing would blink (the receiver's bound would fire
/// before the next refresh is allowed to be broadcast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypingLimits {
    /// How long an active indicator survives without a fresh signal. This is the
    /// receiver's bound and the sender's idle timer, and it is the number the
    /// frontend mirrors so its local expiry matches the server's.
    pub ttl: Duration,
    /// The minimum interval between two `Started` fan-outs for one Participant in
    /// one Conversation. This is the fan-out bound: keystrokes inside the window
    /// are coalesced into the one signal already sent.
    pub throttle: Duration,
}

impl TypingLimits {
    /// Production limits: a 10 s indicator, refreshed at most every 5 s.
    ///
    /// `ttl = 2 * throttle`, so a continuously typing Participant whose refresh
    /// lands exactly on the throttle boundary never blinks: the newest broadcast is
    /// at most one throttle old, half the bound.
    pub const fn production() -> Self {
        Self {
            ttl: Duration::from_secs(10),
            throttle: Duration::from_secs(5),
        }
    }

    /// Explicit limits, for tests that cannot wait ten seconds.
    pub const fn new(ttl: Duration, throttle: Duration) -> Self {
        Self { ttl, throttle }
    }
}

/// One Participant in one Conversation — the scope a typing state lives in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TypingKey {
    conversation_id: String,
    user_id: String,
}

impl TypingKey {
    fn new(conversation_id: &str, user_id: &str) -> Self {
        Self {
            conversation_id: conversation_id.to_owned(),
            user_id: user_id.to_owned(),
        }
    }
}

/// One Participant's typing state in one Conversation.
///
/// `last_change_at` is when the *broadcast* state last changed. It is the anchor
/// for both clocks: `ttl` measures from it, and `throttle` measures from it.
#[derive(Debug, Clone, Copy)]
struct TypingEntry {
    active: bool,
    last_change_at: Instant,
}

impl TypingEntry {
    fn is_active(&self, now: Instant, ttl: Duration) -> bool {
        self.active && now.saturating_duration_since(self.last_change_at) < ttl
    }
}

/// The in-memory typing state: one entry per Participant per Conversation, and the
/// throttle/TTL clock that bounds it.
///
/// Deliberately free of I/O and of async, like `PresenceCheckpoint`: the whole
/// rule — when a signal earns a broadcast, when a stop is stale, when an entry has
/// expired — is unit-tested without a socket or a database.
#[derive(Debug)]
pub(crate) struct TypingDirectory {
    entries: HashMap<TypingKey, TypingEntry>,
    limits: TypingLimits,
}

impl TypingDirectory {
    pub(crate) fn new(limits: TypingLimits) -> Self {
        Self {
            entries: HashMap::new(),
            limits,
        }
    }

    /// Whether a signal earns a fan-out, given the current state.
    ///
    /// A pure read: the caller resolves the audience and then [`Self::record`]s the
    /// transition. Splitting the two is what lets the broadcast wait for the
    /// recipient set without the decision being re-taken against a changed map —
    /// the caller holds the lock across both.
    pub(crate) fn decide(&self, key: &TypingKey, state: TypingState, now: Instant) -> bool {
        let Some(entry) = self.entries.get(key) else {
            // Never seen: a start appears, a stop has nothing to cancel.
            return state == TypingState::Started;
        };

        match state {
            // A fresh start after expiry, or a refresh past the throttle window.
            TypingState::Started => {
                !entry.is_active(now, self.limits.ttl)
                    || now.saturating_duration_since(entry.last_change_at) >= self.limits.throttle
            }
            // A stop is only meaningful while the indicator is still showing; a
            // stop for an expired or already-cleared entry is dropped rather than
            // broadcast as a no-op.
            //
            // What this does NOT protect against: a stop generated *before* a
            // newer start but arriving after it. Two Devices of one account mean
            // two connections, and arrival order at the lock is the only order
            // there is, so that stop clears the newer indicator. The cost is a
            // flicker rather than a lost message - the sender is still composing,
            // so the next start that clears the throttle re-asserts the
            // indicator - but it is a real gap, and closing it needs a monotonic
            // per-(Conversation, User) sequence carried on the signal, which is a
            // protocol change this ticket does not make.
            TypingState::Stopped => entry.is_active(now, self.limits.ttl),
        }
    }

    /// Commit a transition the caller has decided to broadcast.
    pub(crate) fn record(&mut self, key: &TypingKey, state: TypingState, now: Instant) {
        self.entries.insert(
            key.clone(),
            TypingEntry {
                active: state == TypingState::Started,
                last_change_at: now,
            },
        );
    }

    /// Drop any state for a key — used when the signal turns out to be
    /// unauthorised, so a rejected sender leaves nothing behind.
    pub(crate) fn forget(&mut self, key: &TypingKey) {
        self.entries.remove(key);
    }

    /// Drop expired and cleared entries.
    ///
    /// Run on the heartbeat tick like the presence checkpoint: the map is bounded
    /// by "Participants who are actively typing right now", and an entry that has
    /// neither a live indicator nor a reason to be remembered costs nothing to
    /// forget.
    pub(crate) fn sweep(&mut self, now: Instant) {
        let ttl = self.limits.ttl;
        self.entries.retain(|_, entry| entry.is_active(now, ttl));
    }

    /// The number of entries currently held, for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The hub's typing state: the directory and the one lock that serializes a
/// transition with its fan-out.
///
/// The lock is held across the audience resolution and the `deliver` call — never
/// released in between — so there is no window in which a `Stopped` and a later
/// `Started` could be reordered relative to each other. It is held for one indexed
/// read and one `try_send` fan-out, at most once per throttle window per typer,
/// which is why a single lock is affordable on the 2 vCPU box.
#[derive(Debug)]
pub(crate) struct TypingTracker {
    directory: Mutex<TypingDirectory>,
}

impl TypingTracker {
    pub(crate) fn new(limits: TypingLimits) -> Self {
        Self {
            directory: Mutex::new(TypingDirectory::new(limits)),
        }
    }
}

impl RealtimeHub {
    /// Handle one client typing signal: coalesce it, authorise it, fan it out.
    ///
    /// The order of the checks is the order of the guarantees:
    ///
    /// 1. **Coalesce** against the directory first, so a client that emits per
    ///    keystroke never reaches the database or the fan-out more than once per
    ///    window.
    /// 2. **Authorise** through [`jiuyue_chat::ChatService::conversation_audience`]:
    ///    the caller must be a Participant, and the audience is the **other**
    ///    Participants of *this* Conversation — never the caller's whole social
    ///    graph, and never the caller's own Devices (so your phone does not echo
    ///    your laptop).
    /// 3. **Broadcast** the transition while still holding the lock, so all
    ///    receivers see one total order.
    ///
    /// A failure at step 2 is logged and dropped: a typing signal is a hint, and a
    /// database hiccup must cost nobody their connection. The entry is forgotten so
    /// a rejected sender does not leave state behind.
    pub(crate) async fn handle_typing(&self, user_id: &str, signal: &TypingSignal) {
        let key = TypingKey::new(&signal.conversation_id, user_id);
        let mut directory = self.typing.directory.lock().await;
        let now = Instant::now();

        if !directory.decide(&key, signal.state, now) {
            return;
        }

        let audience = match self
            .chat
            .conversation_audience(user_id, &signal.conversation_id)
            .await
        {
            Ok(audience) => audience,
            Err(error) => {
                tracing::debug!(%error, "dropping a typing signal that is not from a participant");
                directory.forget(&key);
                return;
            }
        };

        directory.record(&key, signal.state, now);

        if audience.is_empty() {
            return;
        }

        self.registry
            .deliver(
                &audience,
                &ServerEvent::Typing(Typing {
                    conversation_id: signal.conversation_id.clone(),
                    user_id: user_id.to_owned(),
                    state: signal.state,
                }),
            )
            .await;
    }

    /// Clear a Participant's typing state because they sent a Message.
    ///
    /// A Message is the strongest possible "I stopped typing", and the sender's
    /// client may not have sent a stop — it may have hit Enter and been busy
    /// rendering the optimistic bubble. Clearing here is what makes the indicator
    /// disappear the moment the Message does, without trusting the client to order
    /// its own frames.
    ///
    /// Cheap when there is nothing to clear: the directory is consulted first, and
    /// an inactive key never reaches the database or the fan-out.
    pub(crate) async fn clear_typing(&self, user_id: &str, conversation_id: &str) {
        let key = TypingKey::new(conversation_id, user_id);
        let mut directory = self.typing.directory.lock().await;
        let now = Instant::now();

        if !directory.decide(&key, TypingState::Stopped, now) {
            return;
        }

        let audience = match self
            .chat
            .conversation_audience(user_id, conversation_id)
            .await
        {
            Ok(audience) => audience,
            Err(error) => {
                tracing::debug!(%error, "could not resolve a typing audience on send");
                directory.forget(&key);
                return;
            }
        };

        directory.record(&key, TypingState::Stopped, now);

        if audience.is_empty() {
            return;
        }

        self.registry
            .deliver(
                &audience,
                &ServerEvent::Typing(Typing {
                    conversation_id: conversation_id.to_owned(),
                    user_id: user_id.to_owned(),
                    state: TypingState::Stopped,
                }),
            )
            .await;
    }

    /// Drop typing entries that have expired or been cleared.
    ///
    /// Called on the heartbeat tick, like the presence checkpoint, so the map
    /// cannot grow by one entry per (Participant, Conversation) forever.
    pub(crate) async fn sweep_typing(&self) {
        let mut directory = self.typing.directory.lock().await;
        directory.sweep(Instant::now());
    }

    /// The typing limits in force, for tests and diagnostics.
    pub async fn typing_limits(&self) -> TypingLimits {
        self.typing.directory.lock().await.limits
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use jiuyue_contract::events::TypingState;

    use super::{TypingDirectory, TypingKey, TypingLimits};

    const TTL: Duration = Duration::from_secs(10);
    const THROTTLE: Duration = Duration::from_secs(5);

    fn limits() -> TypingLimits {
        TypingLimits::new(TTL, THROTTLE)
    }

    fn key() -> TypingKey {
        TypingKey::new("01JABC1234567890ABCDEFGHJ1", "01JABC1234567890ABCDEFGHJ2")
    }

    #[test]
    fn a_first_start_always_broadcasts_and_a_stop_with_nothing_to_cancel_does_not() {
        let directory = TypingDirectory::new(limits());
        let start = Instant::now();

        assert!(
            directory.decide(&key(), TypingState::Started, start),
            "the first start is what makes the indicator appear"
        );
        assert!(
            !directory.decide(&key(), TypingState::Stopped, start),
            "a stop for a Participant who is not typing has nothing to clear"
        );
    }

    #[test]
    fn keystrokes_inside_the_throttle_window_collapse_to_one_broadcast() {
        let mut directory = TypingDirectory::new(limits());
        let start = Instant::now();
        directory.record(&key(), TypingState::Started, start);

        // A careless client emitting one signal per key for five seconds.
        for millis in 0..THROTTLE.as_millis() {
            let now = start + Duration::from_millis(millis as u64);
            assert!(
                !directory.decide(&key(), TypingState::Started, now),
                "a refresh inside the window must not fan out again (at {millis} ms)"
            );
        }

        assert!(
            directory.decide(&key(), TypingState::Started, start + THROTTLE),
            "the broadcast is allowed again exactly at the throttle boundary"
        );
    }

    #[test]
    fn a_stop_clears_the_indicator_and_the_next_start_broadcasts_immediately() {
        let mut directory = TypingDirectory::new(limits());
        let start = Instant::now();
        directory.record(&key(), TypingState::Started, start);

        let stop = start + Duration::from_secs(1);
        assert!(directory.decide(&key(), TypingState::Stopped, stop));

        directory.record(&key(), TypingState::Stopped, stop);
        assert!(
            !directory.decide(&key(), TypingState::Stopped, stop),
            "a repeated stop is a no-op"
        );
        assert!(
            directory.decide(
                &key(),
                TypingState::Started,
                stop + Duration::from_millis(1)
            ),
            "after a stop, a new start is a new indicator, not a throttled refresh"
        );
    }

    #[test]
    fn an_indicator_expires_on_its_own_without_a_stop() {
        let mut directory = TypingDirectory::new(limits());
        let start = Instant::now();
        directory.record(&key(), TypingState::Started, start);

        // Inside the bound the indicator is still live, so a refresh is throttled
        // rather than broadcast again.
        let alive = start + THROTTLE - Duration::from_millis(1);
        assert!(
            !directory.decide(&key(), TypingState::Started, alive),
            "inside the bound the indicator is still live and throttled"
        );

        let expired = start + TTL;
        assert!(
            !directory.decide(&key(), TypingState::Stopped, expired),
            "after the bound the indicator is gone, so a late stop cancels nothing"
        );
        assert!(
            directory.decide(&key(), TypingState::Started, expired),
            "a start after expiry is a fresh indicator, not a duplicate"
        );
    }

    #[test]
    fn a_stop_clears_a_live_indicator_and_an_alternating_sequence_ends_cleared() {
        let mut directory = TypingDirectory::new(limits());
        let base = Instant::now();

        // The sequence a single Device produces: start, stop, start, stop. Each is
        // recorded in order, which is what the lock in `TypingTracker` guarantees
        // for signals arriving on one connection.
        directory.record(&key(), TypingState::Started, base);

        // A stop while the indicator is live is broadcast: that is what clears it
        // on every other Device.
        assert!(
            directory.decide(
                &key(),
                TypingState::Stopped,
                base + Duration::from_millis(1)
            ),
            "a stop clears an indicator that is currently showing"
        );
        directory.record(
            &key(),
            TypingState::Stopped,
            base + Duration::from_millis(1),
        );

        // A start after a cleared indicator is a fresh broadcast, not a throttled
        // refresh of something that is no longer showing.
        assert!(
            directory.decide(
                &key(),
                TypingState::Started,
                base + Duration::from_millis(2)
            ),
            "a start after a stop is a broadcast, never swallowed by the stop's throttle"
        );
        directory.record(
            &key(),
            TypingState::Started,
            base + Duration::from_millis(2),
        );

        // And it clears again. Alternating transitions never get stuck: the entry
        // carries the state each transition left behind, so "showing" and
        // "cleared" stay distinguishable across the whole sequence.
        assert!(
            directory.decide(
                &key(),
                TypingState::Stopped,
                base + Duration::from_millis(3)
            ),
            "the second stop clears the second indicator"
        );
        directory.record(
            &key(),
            TypingState::Stopped,
            base + Duration::from_millis(3),
        );

        // A stop with nothing showing is dropped rather than broadcast as a no-op.
        //
        // This is the boundary of what the rule buys. It drops a stop for an entry
        // that is *already* cleared or expired; it cannot drop a stop that arrives
        // after a newer start, because that entry *is* showing and arrival order is
        // the only order a two-connection stop has (see `decide`).
        assert!(
            !directory.decide(
                &key(),
                TypingState::Stopped,
                base + Duration::from_millis(4)
            ),
            "a stop for an already-cleared entry cancels nothing and is not broadcast"
        );
    }

    #[test]
    fn the_sweep_forgets_everything_that_is_not_actively_typing() {
        let mut directory = TypingDirectory::new(limits());
        let start = Instant::now();

        directory.record(&key(), TypingState::Started, start);
        let other = TypingKey::new("01JABC1234567890ABCDEFGHJ3", "01JABC1234567890ABCDEFGHJ4");
        directory.record(&other, TypingState::Started, start + Duration::from_secs(1));

        assert_eq!(directory.len(), 2);
        directory.sweep(start + TTL + Duration::from_secs(2));

        assert_eq!(
            directory.len(),
            0,
            "a typing entry is bounded by the TTL, so the map cannot grow without limit"
        );
    }

    #[test]
    fn forgetting_a_key_leaves_nothing_for_a_rejected_sender() {
        let mut directory = TypingDirectory::new(limits());
        let start = Instant::now();
        directory.record(&key(), TypingState::Started, start);

        directory.forget(&key());

        assert_eq!(directory.len(), 0);
    }
}
