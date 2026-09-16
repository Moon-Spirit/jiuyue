//! Login attempt limiting: throttle, then lockout, with bounded memory.
//!
//! A login endpoint that always answers "wrong password" is an unlimited oracle:
//! an attacker can grind passwords forever, and each attempt costs the server a
//! full Argon2id verification (~19 MiB and tens of milliseconds). This module
//! counts consecutive failures **per source**, so the sixth wrong password from
//! one address is refused before any hashing happens.
//!
//! # Two shapes, both honest
//!
//! - **Throttle** — `max_failures` consecutive failures inside `window` earn a
//!   short backoff ([`LoginAttempt::Throttled`], HTTP 429).
//! - **Lockout** — the source keeps failing after its backoff, so after
//!   `max_throttles` throttles it earns an exponentially longer penalty
//!   ([`LoginAttempt::LockedOut`], HTTP 423).
//!
//! Both decay on their own: a throttle or lockout elapses with the clock, and a
//! failure window that passes with no attempt resets the count. A *success*
//! clears the source entirely, so a user who mistypes twice and then remembers
//! their password is not punished for it.
//!
//! # Why this is a trait
//!
//! [`LoginAttemptStore`] is the seam. Today the only implementation is the
//! in-process [`InProcessLoginAttemptStore`]; when there is more than one node,
//! a Redis-backed implementation is written against the same trait and passed to
//! [`crate::AuthService::with_login_store`] instead. The methods are async and the
//! trait is object-safe (`Arc<dyn LoginAttemptStore>`) precisely so that swap is a
//! constructor argument change, not a rewrite of the login use case.
//!
//! # Bounded memory (not optional)
//!
//! A map keyed by source with no ceiling is a denial-of-service we built
//! ourselves: an attacker with many addresses fills it until the 2 GB box dies.
//! [`LoginAttemptPolicy::max_tracked_sources`] is a hard cap; when a new source
//! arrives at the cap, fully-expired entries are pruned first and the coldest
//! (least recently seen) is evicted if that frees nothing. See the arithmetic on
//! [`LoginAttemptPolicy::max_tracked_sources`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A boxed, sendable future.
///
/// The store seam must be usable from async code and implementable against a
/// network store later, but must not drag an async-trait dependency into a crate
/// that otherwise has none. Returning a boxed future is dyn-compatible and costs
/// one allocation per call — negligible beside an Argon2id verification.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Longest a source can be locked out, however often it reoffends (24 hours).
pub const MAX_LOCKOUT_SECS: u64 = 24 * 60 * 60;

/// How far the lockout doubles before it stops growing (`2^16 × base`).
const MAX_LOCKOUT_DOUBLINGS: u32 = 16;

/// What the limiter decided about one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginAttempt {
    /// Nothing stands in the way; verify the credentials.
    Allowed,
    /// Too many recent failures: refuse and make the caller wait.
    Throttled {
        /// Length of the imposed backoff, in whole seconds (never zero).
        retry_after_seconds: u64,
    },
    /// Repeated throttling escalated to a lockout: refuse for longer.
    LockedOut {
        /// Length of the imposed penalty, in whole seconds (never zero).
        retry_after_seconds: u64,
    },
}

/// The thresholds a deployment runs with.
///
/// The defaults are the ones a real deployment keeps; tests override them with
/// [`LoginAttemptPolicy::builder`] so production numbers are never weakened to
/// make a test fast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginAttemptPolicy {
    /// Consecutive failures inside one window before the source is throttled.
    pub max_failures: u32,
    /// The failure-counting window. It also bounds how long escalation memory
    /// lasts: once this much time passes with no failure, the count resets.
    pub window: Duration,
    /// How long the first throttle lasts.
    pub throttle: Duration,
    /// Throttles — earned in the same window — before a lockout replaces them.
    pub max_throttles: u32,
    /// Base lockout length; each further lockout doubles it up to
    /// [`MAX_LOCKOUT_SECS`].
    pub lockout: Duration,
    /// Hard ceiling on tracked sources.
    ///
    /// Arithmetic for the default of 20 000 entries: a source key is an IP (≤ 45
    /// bytes of text, ~48 bytes of heap once rounded) plus the `String` header
    /// (24 B); a value is [`AttemptRecord`], 7 fields ≈ 80 bytes; a `HashMap`
    /// slot adds ~10 bytes of control and slack at its load factor. Budgeting a
    /// generous 256 bytes per entry: 20 000 × 256 B ≈ **5 MiB**, about 0.25 % of
    /// the 2 GB box — and it cannot grow beyond that, by construction.
    pub max_tracked_sources: usize,
}

impl Default for LoginAttemptPolicy {
    fn default() -> Self {
        Self {
            max_failures: 5,
            window: Duration::from_secs(15 * 60),
            throttle: Duration::from_secs(60),
            max_throttles: 3,
            lockout: Duration::from_secs(15 * 60),
            max_tracked_sources: 20_000,
        }
    }
}

impl LoginAttemptPolicy {
    /// Start from the production defaults and override what a test needs.
    pub fn builder() -> LoginAttemptPolicyBuilder {
        LoginAttemptPolicyBuilder {
            policy: Self::default(),
        }
    }

    /// The wait a throttle reports: the imposed backoff length, never zero.
    ///
    /// It is deliberately *not* the remaining time. A countdown would differ by a
    /// second depending on when the response was assembled, and the throttled
    /// response must be byte-identical for an existing and a non-existing account
    /// — a live number would leak the timing of the two requests instead of the
    /// existence of the account.
    fn throttle_secs(&self) -> u64 {
        self.throttle.as_secs().max(1)
    }

    /// The length of the `lockouts`-th lockout: `lockout × 2^(n−1)`, capped.
    pub fn lockout_seconds(&self, lockouts: u32) -> u64 {
        let exponent = lockouts.saturating_sub(1).min(MAX_LOCKOUT_DOUBLINGS);
        let factor = 1u64 << exponent;
        self.lockout
            .as_secs()
            .saturating_mul(factor)
            .clamp(1, MAX_LOCKOUT_SECS)
    }

    /// Clamp a policy to values that can actually trip.
    ///
    /// A zero window or zero cap would silently disable the limiter; rejecting
    /// them at construction beats debugging a limiter that never limits.
    fn sanitized(mut self) -> Self {
        self.max_failures = self.max_failures.max(1);
        self.max_throttles = self.max_throttles.max(1);
        self.max_tracked_sources = self.max_tracked_sources.max(1);
        let floor = Duration::from_millis(1);
        self.window = self.window.max(floor);
        self.throttle = self.throttle.max(floor);
        self.lockout = self.lockout.max(floor);
        self
    }
}

/// Fluent overrides for [`LoginAttemptPolicy`].
#[derive(Debug, Clone)]
pub struct LoginAttemptPolicyBuilder {
    policy: LoginAttemptPolicy,
}

impl LoginAttemptPolicyBuilder {
    /// Failures in a window before a throttle.
    pub fn max_failures(mut self, max_failures: u32) -> Self {
        self.policy.max_failures = max_failures;
        self
    }

    /// The failure-counting window.
    pub fn window(mut self, window: Duration) -> Self {
        self.policy.window = window;
        self
    }

    /// How long a throttle lasts.
    pub fn throttle(mut self, throttle: Duration) -> Self {
        self.policy.throttle = throttle;
        self
    }

    /// Throttles before a lockout.
    pub fn max_throttles(mut self, max_throttles: u32) -> Self {
        self.policy.max_throttles = max_throttles;
        self
    }

    /// Base lockout length.
    pub fn lockout(mut self, lockout: Duration) -> Self {
        self.policy.lockout = lockout;
        self
    }

    /// Hard ceiling on tracked sources.
    pub fn max_tracked_sources(mut self, max_tracked_sources: usize) -> Self {
        self.policy.max_tracked_sources = max_tracked_sources;
        self
    }

    /// Freeze the policy, clamped to values that can trip.
    pub fn build(self) -> LoginAttemptPolicy {
        self.policy.sanitized()
    }
}

/// Where login attempts are counted, keyed by source.
///
/// The key must identify the *source*, never the account: keying on the email
/// would make a throttled response reveal that the account exists. See
/// [`crate::SessionContext::source`].
pub trait LoginAttemptStore: Send + Sync {
    /// What the current state says about this source, without recording anything.
    fn verdict<'a>(&'a self, source: &'a str) -> BoxFuture<'a, LoginAttempt>;

    /// Record a failed credential check and return the state it produced.
    ///
    /// The returned verdict is the state *now*: a caller that already told the
    /// client "wrong password" should not switch that answer mid-request, it
    /// should let the *next* attempt be the one that is refused.
    fn record_failure<'a>(&'a self, source: &'a str) -> BoxFuture<'a, LoginAttempt>;

    /// Clear the source after a successful sign-in.
    fn record_success<'a>(&'a self, source: &'a str) -> BoxFuture<'a, ()>;
}

/// The counters kept for one source.
///
/// Private: this is the in-process representation, with `Instant` values that a
/// shared store would have to serialise differently. The seam that matters is
/// [`LoginAttemptStore`], not this struct.
#[derive(Debug, Clone, Copy)]
struct AttemptRecord {
    /// Failures inside the current window.
    failures: u32,
    /// Throttles earned inside the current window.
    throttles: u32,
    /// Lockouts earned by this source, kept across windows so an unrepentant
    /// source escalates instead of starting over.
    lockouts: u32,
    /// When the current failure window began.
    window_started: Instant,
    /// Until when the source is throttled, if it is.
    throttled_until: Option<Instant>,
    /// Until when the source is locked out, if it is.
    lockout_until: Option<Instant>,
    /// Last time this source was seen, for coldest-first eviction.
    last_seen: Instant,
}

impl AttemptRecord {
    fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            throttles: 0,
            lockouts: 0,
            window_started: now,
            throttled_until: None,
            lockout_until: None,
            last_seen: now,
        }
    }

    /// Drop the failure count once the window has fully elapsed.
    fn decay_window(&mut self, now: Instant, policy: &LoginAttemptPolicy) {
        if now.saturating_duration_since(self.window_started) > policy.window {
            self.failures = 0;
            self.throttles = 0;
            self.window_started = now;
        }
    }

    fn verdict(&self, now: Instant, policy: &LoginAttemptPolicy) -> LoginAttempt {
        if self.lockout_until.is_some_and(|until| until > now) {
            return LoginAttempt::LockedOut {
                retry_after_seconds: policy.lockout_seconds(self.lockouts),
            };
        }
        if self.throttled_until.is_some_and(|until| until > now) {
            return LoginAttempt::Throttled {
                retry_after_seconds: policy.throttle_secs(),
            };
        }
        LoginAttempt::Allowed
    }

    fn record_failure(&mut self, now: Instant, policy: &LoginAttemptPolicy) -> LoginAttempt {
        self.decay_window(now, policy);
        self.last_seen = now;
        self.failures = self.failures.saturating_add(1);

        if self.failures < policy.max_failures {
            return LoginAttempt::Allowed;
        }

        self.throttles = self.throttles.saturating_add(1);
        if self.throttles >= policy.max_throttles {
            self.lockouts = self.lockouts.saturating_add(1);
            let seconds = policy.lockout_seconds(self.lockouts);
            self.lockout_until = Some(now + Duration::from_secs(seconds));
            self.throttled_until = None;
            return LoginAttempt::LockedOut {
                retry_after_seconds: seconds,
            };
        }

        self.throttled_until = Some(now + policy.throttle);
        LoginAttempt::Throttled {
            retry_after_seconds: policy.throttle_secs(),
        }
    }

    /// Whether this record carries nothing a fresh one would not: every penalty
    /// has expired and the failure window has passed. Such a record can be freed
    /// without losing a single observable decision.
    fn is_stale(&self, now: Instant, policy: &LoginAttemptPolicy) -> bool {
        let window_over = now.saturating_duration_since(self.window_started) > policy.window;
        let unlocked = self.lockout_until.is_none_or(|until| until <= now);
        let unthrottled = self.throttled_until.is_none_or(|until| until <= now);

        window_over && unlocked && unthrottled
    }
}

/// The single-node implementation: a bounded map of source → counters.
///
/// A [`Mutex`] is enough. Every critical section is a handful of map operations
/// and an `Instant::now()` — there is no await inside one — and on one node this
/// is the whole contention surface.
pub struct InProcessLoginAttemptStore {
    policy: LoginAttemptPolicy,
    sources: Mutex<HashMap<String, AttemptRecord>>,
}

impl Default for InProcessLoginAttemptStore {
    fn default() -> Self {
        Self::new(LoginAttemptPolicy::default())
    }
}

impl InProcessLoginAttemptStore {
    /// Build the store with the given thresholds.
    pub fn new(policy: LoginAttemptPolicy) -> Self {
        Self {
            policy: policy.sanitized(),
            sources: Mutex::new(HashMap::new()),
        }
    }

    /// The thresholds in force.
    pub fn policy(&self) -> &LoginAttemptPolicy {
        &self.policy
    }

    /// How many sources are tracked right now.
    ///
    /// Exposed so the memory bound is assertable — a ceiling nobody measures is a
    /// ceiling nobody has.
    pub fn tracked_sources(&self) -> usize {
        self.lock().len()
    }

    /// Lock the map, recovering from poisoning rather than panicking.
    ///
    /// Nothing here can panic while holding the lock, so poisoning means a panic
    /// elsewhere; the counters are still consistent, and a login limiter must not
    /// take the process down.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, AttemptRecord>> {
        self.sources
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Make room for one new source, if the cap is reached.
    fn make_room(
        sources: &mut HashMap<String, AttemptRecord>,
        now: Instant,
        policy: &LoginAttemptPolicy,
    ) {
        if sources.len() < policy.max_tracked_sources {
            return;
        }

        // 1. Free records that carry no state at all. A source whose penalties
        //    have expired is indistinguishable from one never seen, so dropping it
        //    loses nothing and leaves genuinely active sources alone.
        sources.retain(|_, record| !record.is_stale(now, policy));

        // 2. Still full: evict the coldest source. This costs one linear scan per
        //    *new* source, and a new source only appears after a failed Argon2id
        //    verification — so the scan is dwarfed by the hashing it follows.
        while sources.len() >= policy.max_tracked_sources {
            let coldest = sources
                .iter()
                .min_by_key(|(_, record)| record.last_seen)
                .map(|(source, _)| source.clone());

            match coldest {
                Some(source) => {
                    sources.remove(&source);
                }
                None => break,
            }
        }
    }
}

impl LoginAttemptStore for InProcessLoginAttemptStore {
    fn verdict<'a>(&'a self, source: &'a str) -> BoxFuture<'a, LoginAttempt> {
        let now = Instant::now();
        let policy = self.policy;
        let verdict = {
            let mut sources = self.lock();
            match sources.get_mut(source) {
                Some(record) => {
                    record.last_seen = now;
                    record.verdict(now, &policy)
                }
                None => LoginAttempt::Allowed,
            }
        };

        Box::pin(std::future::ready(verdict))
    }

    fn record_failure<'a>(&'a self, source: &'a str) -> BoxFuture<'a, LoginAttempt> {
        let now = Instant::now();
        let policy = self.policy;
        let verdict = {
            let mut sources = self.lock();
            if let Some(record) = sources.get_mut(source) {
                record.record_failure(now, &policy)
            } else {
                Self::make_room(&mut sources, now, &policy);
                let record = sources
                    .entry(source.to_owned())
                    .or_insert_with(|| AttemptRecord::new(now));
                record.record_failure(now, &policy)
            }
        };

        Box::pin(std::future::ready(verdict))
    }

    fn record_success<'a>(&'a self, source: &'a str) -> BoxFuture<'a, ()> {
        self.lock().remove(source);
        Box::pin(std::future::ready(()))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        InProcessLoginAttemptStore, LoginAttempt, LoginAttemptPolicy, LoginAttemptStore,
        MAX_LOCKOUT_SECS,
    };

    /// A tiny window and backoff, so the decay tests are fast without touching
    /// the production defaults.
    fn fast_policy() -> LoginAttemptPolicy {
        LoginAttemptPolicy::builder()
            .max_failures(3)
            .window(Duration::from_millis(30))
            .throttle(Duration::from_millis(40))
            .max_throttles(2)
            .lockout(Duration::from_millis(50))
            .build()
    }

    fn store() -> InProcessLoginAttemptStore {
        InProcessLoginAttemptStore::new(fast_policy())
    }

    #[test]
    fn the_default_policy_is_the_documented_one() {
        let policy = LoginAttemptPolicy::default();

        assert_eq!(policy.max_failures, 5);
        assert_eq!(policy.window, Duration::from_secs(900));
        assert_eq!(policy.throttle, Duration::from_secs(60));
        assert_eq!(policy.max_throttles, 3);
        assert_eq!(policy.lockout, Duration::from_secs(900));
        assert_eq!(policy.max_tracked_sources, 20_000);
    }

    #[test]
    fn lockouts_escalate_and_are_capped() {
        let policy = LoginAttemptPolicy::default();

        assert_eq!(policy.lockout_seconds(1), 900);
        assert_eq!(policy.lockout_seconds(2), 1_800);
        assert_eq!(policy.lockout_seconds(3), 3_600);
        assert_eq!(policy.lockout_seconds(1_000), MAX_LOCKOUT_SECS);
    }

    #[test]
    fn a_builder_clamps_values_that_could_never_trip() {
        let policy = LoginAttemptPolicy::builder()
            .max_failures(0)
            .max_throttles(0)
            .max_tracked_sources(0)
            .window(Duration::ZERO)
            .build();

        assert_eq!(policy.max_failures, 1);
        assert_eq!(policy.max_throttles, 1);
        assert_eq!(policy.max_tracked_sources, 1);
        assert_eq!(policy.window, Duration::from_millis(1));
    }

    #[tokio::test]
    async fn failures_below_the_threshold_are_allowed_and_the_next_is_throttled() {
        let store = store();

        for attempt in 1..=2 {
            assert_eq!(
                store.record_failure("203.0.113.7").await,
                LoginAttempt::Allowed,
                "failure {attempt} is below max_failures"
            );
            assert_eq!(store.verdict("203.0.113.7").await, LoginAttempt::Allowed);
        }

        store.record_failure("203.0.113.7").await;

        match store.verdict("203.0.113.7").await {
            LoginAttempt::Throttled {
                retry_after_seconds,
            } => assert_eq!(
                retry_after_seconds, 1,
                "a sub-second backoff still reports 1"
            ),
            other => panic!("expected a throttle, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_throttle_decays_on_its_own() {
        let store = store();

        for _ in 0..3 {
            store.record_failure("203.0.113.7").await;
        }
        assert!(matches!(
            store.verdict("203.0.113.7").await,
            LoginAttempt::Throttled { .. }
        ));

        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(
            store.verdict("203.0.113.7").await,
            LoginAttempt::Allowed,
            "an elapsed throttle must stop refusing attempts"
        );
    }

    #[tokio::test]
    async fn an_elapsed_window_resets_the_failure_count() {
        let store = store();

        store.record_failure("203.0.113.7").await;
        store.record_failure("203.0.113.7").await;

        tokio::time::sleep(Duration::from_millis(60)).await;

        assert_eq!(
            store.record_failure("203.0.113.7").await,
            LoginAttempt::Allowed,
            "the window elapsed, so this is failure one, not three"
        );
    }

    #[tokio::test]
    async fn a_success_clears_the_counter() {
        let store = store();

        store.record_failure("203.0.113.7").await;
        store.record_failure("203.0.113.7").await;
        store.record_success("203.0.113.7").await;

        assert_eq!(store.tracked_sources(), 0, "a success forgets the source");
        assert_eq!(
            store.record_failure("203.0.113.7").await,
            LoginAttempt::Allowed
        );
    }

    #[tokio::test]
    async fn repeated_throttling_escalates_to_a_lockout() {
        let store = InProcessLoginAttemptStore::new(
            LoginAttemptPolicy::builder()
                .max_failures(1)
                .window(Duration::from_secs(60))
                .throttle(Duration::from_millis(10))
                .max_throttles(2)
                .lockout(Duration::from_millis(200))
                .build(),
        );

        assert!(matches!(
            store.record_failure("203.0.113.7").await,
            LoginAttempt::Throttled { .. }
        ));

        tokio::time::sleep(Duration::from_millis(40)).await;

        assert!(matches!(
            store.record_failure("203.0.113.7").await,
            LoginAttempt::LockedOut { .. }
        ));
        assert!(
            matches!(
                store.verdict("203.0.113.7").await,
                LoginAttempt::LockedOut { .. }
            ),
            "the lockout must outlive the request that tripped it"
        );
    }

    #[tokio::test]
    async fn sources_do_not_affect_each_other() {
        let store = store();

        for _ in 0..3 {
            store.record_failure("203.0.113.7").await;
        }

        assert!(matches!(
            store.verdict("203.0.113.7").await,
            LoginAttempt::Throttled { .. }
        ));
        assert_eq!(
            store.verdict("198.51.100.9").await,
            LoginAttempt::Allowed,
            "one source exhausting its budget must not touch another"
        );
    }

    #[tokio::test]
    async fn the_tracked_source_cap_holds_when_many_sources_arrive() {
        let store = InProcessLoginAttemptStore::new(
            LoginAttemptPolicy::builder().max_tracked_sources(8).build(),
        );

        for index in 0..500 {
            store.record_failure(&format!("203.0.113.{index}")).await;

            assert!(
                store.tracked_sources() <= 8,
                "the map held {} entries, above the cap of 8",
                store.tracked_sources()
            );
        }
    }

    #[tokio::test]
    async fn a_stale_source_is_evicted_before_an_active_one() {
        let store = InProcessLoginAttemptStore::new(
            LoginAttemptPolicy::builder()
                .max_failures(1)
                .window(Duration::from_millis(20))
                .throttle(Duration::from_millis(20))
                .max_tracked_sources(1)
                .build(),
        );

        // A fails, then its window passes entirely, leaving a record that carries
        // nothing. B then fails at the cap: the stale record must make way for it
        // rather than A and B fighting for the single slot.
        store.record_failure("203.0.113.1").await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        store.verdict("203.0.113.1").await; // touches last_seen; still stale
        store.record_failure("203.0.113.2").await;

        assert_eq!(store.tracked_sources(), 1);
        assert_eq!(
            store.verdict("203.0.113.2").await,
            LoginAttempt::Throttled {
                retry_after_seconds: 1
            },
            "the new source kept its behaviour after eviction of the stale one"
        );
    }
}
