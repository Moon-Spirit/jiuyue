//! Versioned WebSocket envelopes.
//!
//! Every frame on the wire is exactly one envelope. The protocol version `v` is
//! stamped on each one so a version mismatch is detectable from the first frame
//! rather than as a mysterious deserialisation failure later.
//!
//! The `version` field is private: the only way to build an envelope is
//! [`ServerEnvelope::new`] / [`ClientEnvelope::new`], so an envelope can never
//! carry a version other than [`PROTOCOL_VERSION`].

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::events::{ClientEvent, ServerEvent};

/// Wire protocol version carried on every envelope.
///
/// Bump this only for a breaking change to the envelope or event shape.
/// Additive changes (a new [`ServerEvent`] variant) keep the same version,
/// because clients ignore event types they do not recognise.
pub const PROTOCOL_VERSION: u16 = 1;

/// Server-to-client envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ServerEnvelope {
    /// Protocol version; always [`PROTOCOL_VERSION`].
    #[serde(rename = "v")]
    version: u16,
    /// Per-connection, monotonically increasing sequence number.
    ///
    /// This is the authority for gap detection and replay: a client that sees a
    /// hole immediately resynchronises instead of silently skipping events
    /// (ADR-0003).
    #[serde(rename = "s")]
    #[ts(type = "number")]
    sequence: u64,
    /// Server wall-clock time (milliseconds since the Unix epoch).
    #[serde(rename = "ts")]
    #[ts(type = "number")]
    timestamp_ms: i64,
    /// The event carried by this envelope.
    #[serde(rename = "e")]
    event: ServerEvent,
}

impl ServerEnvelope {
    /// Build an envelope stamped with the current [`PROTOCOL_VERSION`].
    pub fn new(sequence: u64, timestamp_ms: i64, event: ServerEvent) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            sequence,
            timestamp_ms,
            event,
        }
    }

    /// Protocol version carried by this envelope.
    pub fn version(&self) -> u16 {
        self.version
    }

    /// Per-connection sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Server wall-clock time in milliseconds since the Unix epoch.
    pub fn timestamp_ms(&self) -> i64 {
        self.timestamp_ms
    }

    /// Event carried by this envelope.
    pub fn event(&self) -> &ServerEvent {
        &self.event
    }
}

/// Client-to-server envelope.
///
/// Unlike [`ServerEnvelope`] this carries no sequence or timestamp: the client
/// does not own ordering, and attaching a client clock would invite code that
/// trusts it (ADR-0003 rejects untrusted client time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ClientEnvelope {
    /// Protocol version; always [`PROTOCOL_VERSION`].
    #[serde(rename = "v")]
    version: u16,
    /// The event carried by this envelope.
    #[serde(rename = "e")]
    event: ClientEvent,
}

impl ClientEnvelope {
    /// Build an envelope stamped with the current [`PROTOCOL_VERSION`].
    pub fn new(event: ClientEvent) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            event,
        }
    }

    /// Protocol version carried by this envelope.
    pub fn version(&self) -> u16 {
        self.version
    }

    /// Event carried by this envelope.
    pub fn event(&self) -> &ClientEvent {
        &self.event
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientEnvelope, PROTOCOL_VERSION, ServerEnvelope};
    use crate::events::{ClientEvent, Ping, ServerEvent};

    #[test]
    fn server_envelope_uses_the_short_wire_keys() {
        let envelope = ServerEnvelope::new(
            7,
            1_750_000_000_000,
            ServerEvent::Ping(Ping {
                seq: 7,
                time_ms: 1_750_000_000_000,
            }),
        );

        let wire = serde_json::to_value(&envelope).expect("envelope must serialise");

        assert_eq!(wire["v"], PROTOCOL_VERSION);
        assert_eq!(wire["s"], 7);
        assert_eq!(wire["ts"], 1_750_000_000_000_i64);
        assert_eq!(wire["e"]["t"], "Ping");
        assert_eq!(wire["e"]["d"]["seq"], 7);
        assert_eq!(wire["e"]["d"]["time_ms"], 1_750_000_000_000_i64);
    }

    #[test]
    fn server_envelope_round_trips_through_json() {
        let envelope = ServerEnvelope::new(
            1,
            42,
            ServerEvent::Ping(Ping {
                seq: 1,
                time_ms: 42,
            }),
        );

        let text = serde_json::to_string(&envelope).expect("envelope must serialise");
        let decoded: ServerEnvelope =
            serde_json::from_str(&text).expect("envelope must deserialise");

        assert_eq!(decoded, envelope);
        assert_eq!(decoded.version(), PROTOCOL_VERSION);
    }

    #[test]
    fn client_envelope_uses_the_short_wire_keys() {
        let envelope = ClientEnvelope::new(ClientEvent::Ping(Ping {
            seq: 3,
            time_ms: 99,
        }));

        let wire = serde_json::to_value(&envelope).expect("envelope must serialise");

        assert_eq!(wire["v"], PROTOCOL_VERSION);
        assert_eq!(wire["e"]["t"], "Ping");
        assert_eq!(wire["e"]["d"]["seq"], 3);
        assert!(
            wire.get("s").is_none(),
            "client envelopes carry no sequence"
        );
        assert!(
            wire.get("ts").is_none(),
            "client envelopes carry no timestamp"
        );
    }
}
