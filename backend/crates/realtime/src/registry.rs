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
//! path of ticket #11, which is why dropping is safe here and blocking is not.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use jiuyue_contract::ServerEvent;
use tokio::sync::{RwLock, mpsc};

/// Identifies one live connection. Unique for the lifetime of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

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

    /// Record a live connection and return its id.
    pub async fn register(
        &self,
        user_id: String,
        sender: mpsc::Sender<ServerEvent>,
    ) -> ConnectionId {
        let id = ConnectionId(self.next_id.fetch_add(1, Ordering::Relaxed));

        let mut connections = self.connections.write().await;
        connections.entry(user_id).or_default().insert(id, sender);

        id
    }

    /// Forget a connection. Idempotent, so a failed disconnect cannot corrupt the map.
    pub async fn unregister(&self, user_id: &str, id: ConnectionId) {
        let mut connections = self.connections.write().await;

        if let Some(sessions) = connections.get_mut(user_id) {
            sessions.remove(&id);
            if sessions.is_empty() {
                connections.remove(user_id);
            }
        }
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
