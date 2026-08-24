//! In-process registry of live WebSocket connections.
//!
//! Layout: `user_id -> device_id -> bounded mpsc sender` carrying
//! pre-serialized protocol frames. One user may hold several devices; a
//! fanout hits all of them.
//!
//! # Delivery policy (drop-lag)
//!
//! Each device channel is bounded at [`OUTBOUND_CHANNEL_CAPACITY`] frames.
//! Producers use `try_send` and **never await**: a consumer that falls more
//! than the capacity behind loses the overflowing live frames (`Full`), and
//! sends into an already-dead channel are silently dropped (`Closed`). This
//! is deliberate — a slow client must never back-pressure another user's
//! send path. Lost live frames are recoverable by design: ticket 06's sync
//! protocol replays everything past `last_delivered_seq` on reconnect.

use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Per-device outbound queue depth before drop-lag kicks in.
pub const OUTBOUND_CHANNEL_CAPACITY: usize = 512;

/// A frame serialized once, shared by every recipient without copying.
pub type OutboundFrame = Arc<String>;

/// Bounded handle a connection registers so others can push frames to it.
pub type FrameTx = mpsc::Sender<OutboundFrame>;

/// Live-connection directory shared across all WS handler tasks.
#[derive(Debug, Default)]
pub struct ConnRegistry {
    conns: DashMap<Uuid, HashMap<Uuid, FrameTx>>,
}

impl ConnRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a connected device under its user.
    pub fn register(&self, user_id: Uuid, device_id: Uuid, tx: FrameTx) {
        let mut devices = self.conns.entry(user_id).or_default();
        devices.insert(device_id, tx);
    }

    /// Removes a disconnected device; prunes empty per-user maps.
    pub fn unregister(&self, user_id: Uuid, device_id: Uuid) {
        if let Some(mut devices) = self.conns.get_mut(&user_id) {
            devices.remove(&device_id);
            if devices.is_empty() {
                drop(devices);
                self.conns.remove(&user_id);
            }
        }
    }

    /// Pushes `frame` to every live device of `user_id` (drop-lag policy).
    ///
    /// Returns how many devices actually accepted the frame.
    pub fn deliver_to(&self, user_id: Uuid, frame: &OutboundFrame) -> usize {
        let Some(devices) = self.conns.get(&user_id) else {
            return 0;
        };
        devices
            .values()
            .filter(|tx| tx.try_send(Arc::clone(frame)).is_ok())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deliver_reaches_registered_device_and_cleans_up_on_unregister() {
        let registry = ConnRegistry::new();
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(OUTBOUND_CHANNEL_CAPACITY);
        let user = Uuid::now_v7();
        let device = Uuid::now_v7();

        assert_eq!(registry.deliver_to(user, &Arc::new("x".into())), 0);

        registry.register(user, device, tx);
        assert_eq!(
            registry.deliver_to(user, &Arc::new("hello".into())),
            1,
            "registered device must receive"
        );
        assert_eq!(rx.recv().await.expect("frame").as_str(), "hello");

        registry.unregister(user, device);
        assert_eq!(registry.deliver_to(user, &Arc::new("bye".into())), 0);
    }

    #[tokio::test]
    async fn full_channel_drops_frame_instead_of_blocking() {
        let registry = ConnRegistry::new();
        let (tx, mut rx) = mpsc::channel(1);
        let user = Uuid::now_v7();
        registry.register(user, Uuid::now_v7(), tx);

        // Fill the single slot...
        assert_eq!(registry.deliver_to(user, &Arc::new("1".into())), 1);
        // ...next one must be dropped, not awaited.
        assert_eq!(
            registry.deliver_to(user, &Arc::new("2".into())),
            0,
            "drop-lag: full channel must not accept"
        );
        assert_eq!(rx.recv().await.expect("frame").as_str(), "1");
    }
}
