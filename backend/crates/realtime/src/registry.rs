//! Live-connection registry.
//!
//! The registry is the fan-out point: it maps a User to every live connection
//! that authenticated as them, so a Message can be pushed to all of a
//! Participant's Devices — including the sender's own other Devices (ADR-0003 /
//! the multi-device requirement).
//!
//! # Bounded by construction
//!
//! Each connection owns a **bounded** control channel and the registry only ever
//! `try_send`s into it. A slow or wedged client therefore applies backpressure to
//! itself — its events are dropped and logged — instead of growing an unbounded
//! buffer on a 2 GB box. Recovery for a dropped event is the reconnect/gap-repair
//! path of ticket #12 (`ConnectionWriter`'s bounded replay buffer, then the
//! client's per-Conversation cursor repair), which is why dropping is safe here
//! and blocking is not.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use jiuyue_contract::ServerEvent;
use tokio::sync::{RwLock, mpsc};

/// Identifies one live connection. Unique for the lifetime of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// The raw id, as carried on the wire in a heartbeat's `connection_id`.
    ///
    /// A client echoes this back in its resume handshake, so the value must be
    /// stable and serialisable; the newtype stays private to keep it opaque to
    /// callers that only need a handle.
    pub fn value(self) -> u64 {
        self.0
    }
}

/// What a registration or removal did to a User's **reachability**.
///
/// Presence is per User (CONTEXT.md) and a User is online when *any* Device is
/// connected, so only the first and last connection of an account change anything.
/// The registry is the only place that knows which one this was, and deciding it
/// under the same lock that mutates the map is what makes the answer exact rather
/// than a race between two Devices connecting at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceChange {
    /// This was the User's first live Device: they became reachable.
    BecameOnline,
    /// This was the User's last live Device: they stopped being reachable.
    BecameOffline,
    /// Other Devices of the same User are still connected: nothing changed.
    Unchanged,
}

/// Every live connection, keyed by the authenticated User.
#[derive(Debug, Default)]
pub struct ConnectionRegistry {
    connections: RwLock<HashMap<String, HashMap<ConnectionId, mpsc::Sender<ServerEvent>>>>,
    next_id: AtomicU64,
}

impl ConnectionRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a live connection and report what it did to the User's presence.
    ///
    /// The count and the insertion happen under one lock, so two Devices of one
    /// account connecting at the same instant cannot both be told they were "the
    /// first" — exactly one of them is, and the other is [`PresenceChange::Unchanged`].
    pub async fn register(
        &self,
        user_id: String,
        sender: mpsc::Sender<ServerEvent>,
    ) -> (ConnectionId, PresenceChange) {
        let id = ConnectionId(self.next_id.fetch_add(1, Ordering::Relaxed));

        let mut connections = self.connections.write().await;
        let sessions = connections.entry(user_id).or_default();
        sessions.insert(id, sender);

        let change = if sessions.len() == 1 {
            PresenceChange::BecameOnline
        } else {
            PresenceChange::Unchanged
        };

        (id, change)
    }

    /// Forget a connection and report what it did to the User's presence.
    ///
    /// Idempotent, so a failed disconnect cannot corrupt the map: removing a
    /// connection that is not there changes nothing and reports
    /// [`PresenceChange::Unchanged`] rather than claiming the User went offline.
    pub async fn unregister(&self, user_id: &str, id: ConnectionId) -> PresenceChange {
        let mut connections = self.connections.write().await;

        let (removed, became_empty) = match connections.get_mut(user_id) {
            Some(sessions) => {
                let removed = sessions.remove(&id).is_some();
                (removed, sessions.is_empty())
            }
            None => (false, false),
        };

        if became_empty {
            connections.remove(user_id);
        }

        if removed && became_empty {
            PresenceChange::BecameOffline
        } else {
            PresenceChange::Unchanged
        }
    }

    /// Whether any Device of a User is connected right now.
    pub async fn is_online(&self, user_id: &str) -> bool {
        self.connections.read().await.contains_key(user_id)
    }

    /// Every User with at least one live connection.
    ///
    /// The heartbeat checkpoint stamps exactly this set, which is what makes a
    /// long-lived online User's last-seen instant stay fresh without a write per
    /// connection.
    pub async fn online_user_ids(&self) -> Vec<String> {
        self.connections.read().await.keys().cloned().collect()
    }

    /// Deliver an event to every live connection of every listed User.
    ///
    /// Returns how many connections accepted the event. A connection whose queue
    /// is full is skipped and logged, never awaited: one wedged client must not
    /// stall the sender's connection or the others in the fan-out.
    pub async fn deliver(&self, user_ids: &[String], event: &ServerEvent) -> usize {
        let connections = self.connections.read().await;
        let mut delivered = 0;

        for user_id in user_ids {
            let Some(sessions) = connections.get(user_id) else {
                continue;
            };

            for (id, sender) in sessions {
                match sender.try_send(event.clone()) {
                    Ok(()) => delivered += 1,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // The client is not draining its queue; dropping is the
                        // bounded-memory choice. #11 repairs the gap on reconnect.
                        tracing::warn!(
                            user_id = %user_id,
                            connection_id = id.0,
                            "dropping a realtime event for a client that is not keeping up"
                        );
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // The connection is tearing down but has not unregistered yet.
                    }
                }
            }
        }

        delivered
    }

    /// How many live connections a User has. Used by tests and diagnostics.
    pub async fn user_connection_count(&self, user_id: &str) -> usize {
        self.connections
            .read()
            .await
            .get(user_id)
            .map_or(0, HashMap::len)
    }
}
