//! Integration tests for the chat domain.
//!
//! These run against a **real PostgreSQL** and the **real Axum router**, with
//! **real WebSocket clients** — no mocks. The UNIQUE constraints, the atomic
//! Sequence Number allocation and the idempotent insert *are* the behaviour under
//! test, and none of them exist in a mock.
//!
//! One test binary split into modules: `support` holds the harness, `conversations`
//! the Conversation lifecycle, `messaging` the send path, `pagination` the
//! Sequence-Number cursor walk, `realtime` the socket behaviour, and `reconnect`
//! the acceptance scenarios for recovering Messages after a disconnection. Each
//! test creates its own schema, so the suite runs in parallel and repeatedly
//! without cleanup.

mod conversations;
mod messaging;
mod pagination;
mod realtime;
mod reconnect;
mod support;
