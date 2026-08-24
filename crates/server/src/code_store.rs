//! In-memory verification-code store (dev/MVP scope per ticket 04).
//!
//! Codes live in process memory with a TTL — deliberately not Redis yet:
//! single-instance dev deployment, and the spec only demands 5-minute expiry.

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

type Key = (String, String); // (channel, normalized target)

#[derive(Clone)]
struct Entry {
    code: String,
    expires_at: Instant,
}

/// `Arc` matters: `AppState` is cloned per request by axum's `State`
/// extractor, and `DashMap`'s own `Clone` is a deep copy.
#[derive(Clone, Default)]
pub struct CodeStore {
    inner: Arc<DashMap<Key, Entry>>,
}

impl CodeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a fresh code for `(channel, target)`, replacing any previous one.
    pub fn issue(&self, channel: &str, target: &str, ttl: Duration, code: String) {
        self.sweep_expired();
        self.inner.insert(
            (channel.to_string(), target.to_string()),
            Entry {
                code,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    /// Checks a code without consuming it. Expired entries count as absent.
    /// `allow_dev_code` enables the well-known dev bypass code.
    pub fn verify(&self, channel: &str, target: &str, code: &str, allow_dev_code: bool) -> bool {
        let key = (channel.to_string(), target.to_string());
        match self.inner.get(&key) {
            Some(entry) => {
                if entry.expires_at <= Instant::now() {
                    return false;
                }
                entry.code == code || (allow_dev_code && code == DEV_CODE)
            }
            None => false,
        }
    }

    /// Removes the entry after successful use (codes are one-shot).
    pub fn consume(&self, channel: &str, target: &str) {
        self.inner
            .remove(&(channel.to_string(), target.to_string()));
    }

    /// Opportunistic sweep of expired entries (called on issue).
    fn sweep_expired(&self) {
        let now = Instant::now();
        self.inner.retain(|_, entry| entry.expires_at > now);
    }
}

/// Well-known development verification code, honored only under
/// `cfg!(debug_assertions)` — see handlers.
pub const DEV_CODE: &str = "000000";
