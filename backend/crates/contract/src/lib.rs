//! jiuyue wire contract — the single source of truth for every external boundary.
//!
//! This crate owns every type that crosses out of the backend: the versioned
//! WebSocket envelopes and events, and the REST request/response types of the
//! identity module. It has no runtime dependencies beyond `serde`, so it stays
//! cheap to compile on the 2 vCPU / 2 GB production box.
//!
//! The contract is one-way: the Rust types below generate the frontend's
//! TypeScript via `ts-rs`, and the frontend imports the generated files rather
//! than re-declaring them. See `README.md` for the generation command and the
//! drift guard.
//!
//! Shape rules that later tickets rely on:
//!
//! - Every envelope carries the protocol version `v`.
//! - `ServerEvent` / `ClientEvent` are adjacently tagged (`{"t": ..., "d": ...}`)
//!   so new variants can be added without breaking existing clients, which
//!   ignore event types they do not know.
//! - The per-connection sequence `s` is the authority for gap detection and
//!   replay (see `docs/adr/0003-message-delivery-protocol.md`).

#![forbid(unsafe_code)]

pub mod auth;
pub mod envelope;
pub mod events;

pub use auth::{
    AuthSession, ErrorBody, ErrorCode, ErrorDetail, FieldError, FieldErrorCode, LoginRequest,
    RefreshRequest, RegisterRequest, TokenPair, UserProfile, WhoAmI,
};
pub use envelope::{ClientEnvelope, PROTOCOL_VERSION, ServerEnvelope};
pub use events::{ClientEvent, Ping, ServerEvent};
