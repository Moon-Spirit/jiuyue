//! Integration tests for the chat domain.
//!
//! These run against a **real PostgreSQL** and the **real Axum router**, with
//! **real WebSocket clients** — no mocks. The UNIQUE constraints, the atomic
//! Sequence Number allocation and the idempotent insert *are* the behaviour under
//! test, and none of them exist in a mock.
//!
//! One test binary split into modules: `support` holds the harness, `conversations`
//! the Conversation lifecycle, `messaging` the send path, `pagination` the
//! Sequence-Number cursor walk, `realtime` the socket behaviour, `reconnect`
//! the acceptance scenarios for recovering Messages after a disconnection, `sync`
//! the per-Device Sync Cursor: offline catch-up and multi-device convergence,
//! `read_state` the private Read Marker / public Read Receipt / Unread Count,
//! `presence` who is reachable and when they last were, `typing` the ephemeral
//! Typing Indicator and its server-side throttle, and `verification_gate`
//! the one action that requires a verified email.
//! Each test creates its own schema, so the suite runs in parallel and repeatedly
//! without cleanup.

mod conversations;
mod groups;
mod messaging;
mod pagination;
mod presence;
mod read_state;
mod realtime;
mod reconnect;
mod support;
mod sync;
mod typing;
mod verification_gate;
