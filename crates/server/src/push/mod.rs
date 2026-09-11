//! M4 offline-push abstraction layer (server-side only).
//!
//! Three moving parts:
//!
//! 1. [`PushChannel`] — an enum-dispatched async delivery surface with three
//!    implementations: [`MockChannel`] (in-memory ring buffer, always
//!    available), [`ApnsChannel`] and [`FcmChannel`] (structurally complete,
//!    transport-injected). No new external crates this milestone: the live
//!    HTTP/2 transports land together with real credentials in the
//!    deployment phase (see `docs/research/2026-08-23-push-infra-rust.md`,
//!    which recommends `apns-h2` for APNs token-auth).
//! 2. [`PushService`] — region-aware channel selection (`JIUYUE_REGION` =
//!    `cn` | `global`, default `global`). `global` tries the configured real
//!    channels (APNs, then FCM); `cn` has no real channel yet (China-native
//!    push is a later milestone). The Mock channel is ALWAYS present as the
//!    last resort so a push is never silently dropped.
//! 3. The dispatch hook lives in `ws::fanout_msg_new`: after a successful
//!    fanout, every OFFLINE member (no registry entry) whose device row
//!    carries a non-null `push_token` gets one fire-and-forget
//!    [`PushEnvelope`] per device. Failures are logged, never panicked.
//!
//! Plaintext policy: envelopes are built ONLY on the plaintext `msg.new`
//! path. Secret chats (`e2ee.msg`) never reach this module — the server
//! cannot decrypt them and must not learn preview text.

use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Notification preview length cap (plaintext chars, char-boundary safe).
pub const PREVIEW_MAX_CHARS: usize = 40;

/// How many characters of a push token the Mock ring buffer retains.
///
/// Full tokens never enter logs or snapshots — prefix only.
pub const TOKEN_PREFIX_LEN: usize = 8;

/// Mock ring-buffer capacity; oldest entries evicted first.
const MOCK_RING_CAPACITY: usize = 500;

// ---------------------------------------------------------------------------
// Platform + envelope
// ---------------------------------------------------------------------------

/// Device platform as seen by the push layer, derived from the
/// `devices.platform` column values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PushPlatform {
    /// `platform = 'ios'` — Apple Push Notification service.
    Ios,
    /// `platform = 'android-fcm'` — Android outside China (Firebase).
    AndroidFcm,
    /// `platform = 'android-cn'` — Android inside China (China-native
    /// channels are a later milestone; today these ride the Mock fallback).
    AndroidChina,
}

impl PushPlatform {
    /// Maps the raw `devices.platform` column value; anything unrecognized
    /// (notably `web`) yields `None` and is ignored by dispatch.
    pub fn parse(platform: &str) -> Option<Self> {
        match platform {
            "ios" => Some(Self::Ios),
            "android-fcm" => Some(Self::AndroidFcm),
            "android-cn" => Some(Self::AndroidChina),
            _ => None,
        }
    }
}

impl std::fmt::Display for PushPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Round-trips the column values so tracing fields stay greppable.
        match self {
            Self::Ios => write!(f, "ios"),
            Self::AndroidFcm => write!(f, "android-fcm"),
            Self::AndroidChina => write!(f, "android-cn"),
        }
    }
}

/// What triggered the push. M4 scope: fresh plaintext messages only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PushKind {
    /// A fresh `msg.new` delivered while the recipient was offline.
    Message,
}

/// One offline-push unit of work: who to wake, about what.
///
/// `sender_username_preview` carries at most [`PREVIEW_MAX_CHARS`] chars of
/// the message body preview (plaintext allowed here because envelopes are
/// only ever built on the normal-chat `msg.new` path — see module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushEnvelope {
    pub to_user_id: Uuid,
    pub device_id: Uuid,
    pub conversation_id: i64,
    pub sender_username_preview: String,
    pub kind: PushKind,
}

impl PushEnvelope {
    /// Builds an envelope with the preview clamped to [`PREVIEW_MAX_CHARS`]
    /// (char-boundary safe, so multi-byte CJK bodies never split mid-char).
    pub fn new(
        to_user_id: Uuid,
        device_id: Uuid,
        conversation_id: i64,
        body_preview: &str,
        kind: PushKind,
    ) -> Self {
        Self {
            to_user_id,
            device_id,
            conversation_id,
            sender_username_preview: truncate_chars(body_preview, PREVIEW_MAX_CHARS),
            kind,
        }
    }
}

/// Cuts a string to at most `max` chars without panicking on multibyte input.
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("push channel not configured")]
    NotConfigured,
    #[error("push transport failed: {0}")]
    Transport(String),
    #[error("push payload rejected: {0}")]
    BadPayload(String),
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// Injected outbound transport: receives the fully-rendered JSON payload and
/// returns the delivery future. Production wiring injects HTTP transports in
/// the deployment phase; tests inject capture closures.
pub type PushFuture = Pin<Box<dyn Future<Output = Result<(), PushError>> + Send>>;
pub type TransportFn =
    Arc<dyn Fn(PushPlatform, String, serde_json::Value) -> PushFuture + Send + Sync>;

/// In-memory ring buffer of dispatched pushes — test oracle and debug
/// surface. Shared via `Arc`; a plain mutex-guarded Vec is deliberate here
/// (single-producer contention is irrelevant at notification volumes).
#[derive(Debug, Default)]
pub struct MockChannel {
    entries: Mutex<Vec<MockEntry>>,
}

/// One recorded mock dispatch. Only a push-token PREFIX is retained.
#[derive(Debug, Clone, Serialize)]
pub struct MockEntry {
    pub dispatched_at_rfc3339: String,
    pub platform: PushPlatform,
    pub token_prefix: String,
    pub envelope: PushEnvelope,
}

impl MockChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one dispatch into the bounded ring buffer (oldest evicted).
    pub async fn deliver(
        &self,
        platform: PushPlatform,
        push_token: &str,
        envelope: PushEnvelope,
    ) -> Result<(), PushError> {
        let mut entries = self.entries.lock().expect("mock lock poisoned");
        if entries.len() >= MOCK_RING_CAPACITY {
            let overflow = entries.len() + 1 - MOCK_RING_CAPACITY;
            entries.drain(..overflow);
        }
        entries.push(MockEntry {
            dispatched_at_rfc3339: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default(),
            platform,
            token_prefix: truncate_chars(push_token, TOKEN_PREFIX_LEN),
            envelope,
        });
        Ok(())
    }

    /// Snapshot for tests and the dev-only `/api/dev/push-log` endpoint.
    pub fn snapshot(&self) -> Vec<MockEntry> {
        self.entries.lock().expect("mock lock poisoned").clone()
    }
}

/// Apple Push Notification service channel — structurally complete,
/// transport-injected.
///
/// Token-auth (JWT) credentials arrive via env:
/// `JIUYUE_APNS_TOKEN_B64` / `JIUYUE_APNS_KEY_ID` / `JIUYUE_APNS_TEAM_ID`.
/// Only non-secret identifiers (`key_id`, `team_id`) are stored; the signed
/// token itself never outlives the constructor.
#[derive(Clone)]
pub struct ApnsChannel {
    key_id: String,
    team_id: String,
    transport: TransportFn,
}

impl std::fmt::Debug for ApnsChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApnsChannel")
            .field("key_id", &self.key_id)
            .field("team_id", &self.team_id)
            .finish_non_exhaustive()
    }
}

impl ApnsChannel {
    /// Test/debug constructor with an injected transport.
    pub fn new(
        key_id: impl Into<String>,
        team_id: impl Into<String>,
        transport: TransportFn,
    ) -> Self {
        Self {
            key_id: key_id.into(),
            team_id: team_id.into(),
            transport,
        }
    }

    /// Reads the token-auth credential trio from the environment. Absent or
    /// incomplete configuration yields `Err("not configured")` — prod wiring
    /// then skips this channel entirely (the Mock fallback still catches
    /// everything).
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Pure core of [`Self::from_env`] so tests stay deterministic without
    /// mutating process-global env state.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let token_b64 = lookup("JIUYUE_APNS_TOKEN_B64").filter(|v| !v.trim().is_empty());
        let key_id = lookup("JIUYUE_APNS_KEY_ID").filter(|v| !v.trim().is_empty());
        let team_id = lookup("JIUYUE_APNS_TEAM_ID").filter(|v| !v.trim().is_empty());
        let (Some(token_b64), Some(key_id), Some(team_id)) = (token_b64, key_id, team_id) else {
            return Err("not configured".to_owned());
        };
        // Shape-check only: the base64 token must decode. The decoded bytes
        // are dropped — signing happens in the deployment-phase transport.
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(token_b64.trim())
            .map_err(|err| format!("JIUYUE_APNS_TOKEN_B64 is not valid base64: {err}"))?;
        tracing::info!(%key_id, %team_id, "APNs channel configured (token-auth)");
        Ok(Self::new(key_id, team_id, pending_http_transport("apns")))
    }

    /// Renders the APNs JSON payload shape:
    /// `{aps:{alert:{title,body},badge}, conversation_id}`.
    fn payload(envelope: &PushEnvelope) -> serde_json::Value {
        serde_json::json!({
            "aps": {
                "alert": { "title": "JiuYue", "body": envelope.sender_username_preview },
                "badge": 1,
            },
            "conversation_id": envelope.conversation_id,
        })
    }

    pub async fn deliver(
        &self,
        platform: PushPlatform,
        push_token: &str,
        envelope: PushEnvelope,
    ) -> Result<(), PushError> {
        let payload = Self::payload(&envelope);
        (self.transport)(platform, push_token.to_owned(), payload).await
    }
}

/// Firebase Cloud Messaging channel (legacy `to`-addressed HTTP shape) —
/// structurally complete, transport-injected. Credential:
/// `JIUYUE_FCM_SERVER_KEY`. The key is validated for presence only and NEVER
/// stored or logged.
#[derive(Clone)]
pub struct FcmChannel {
    transport: TransportFn,
}

impl std::fmt::Debug for FcmChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FcmChannel").finish_non_exhaustive()
    }
}

impl FcmChannel {
    pub fn new(transport: TransportFn) -> Self {
        Self { transport }
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let server_key = lookup("JIUYUE_FCM_SERVER_KEY").filter(|v| !v.trim().is_empty());
        let Some(server_key) = server_key else {
            return Err("not configured".to_owned());
        };
        tracing::info!(
            key_bytes = server_key.len(),
            "FCM channel configured (legacy server-key auth)"
        );
        Ok(Self::new(pending_http_transport("fcm")))
    }

    /// Renders the FCM JSON payload shape:
    /// `{to, notification:{title,body}, data:{...}}`.
    fn payload(push_token: &str, envelope: &PushEnvelope) -> serde_json::Value {
        serde_json::json!({
            "to": push_token,
            "notification": {
                "title": "JiuYue",
                "body": envelope.sender_username_preview,
            },
            "data": {
                "conversation_id": envelope.conversation_id.to_string(),
                "device_id": envelope.device_id.to_string(),
                "kind": "message",
            },
        })
    }

    pub async fn deliver(
        &self,
        platform: PushPlatform,
        push_token: &str,
        envelope: PushEnvelope,
    ) -> Result<(), PushError> {
        let payload = Self::payload(push_token, &envelope);
        (self.transport)(platform, push_token.to_owned(), payload).await
    }
}

/// Placeholder transport used when env credentials exist but the live HTTP
/// client has not shipped yet (deployment phase; `apns-h2` recommended by
/// docs/research/2026-08-23-push-infra-rust.md). Renders payloads and counts
/// bytes — content is deliberately NOT logged (message previews are user
/// data).
fn pending_http_transport(label: &'static str) -> TransportFn {
    Arc::new(move |_platform, _token, payload| {
        Box::pin(async move {
            let bytes = payload.to_string().len();
            tracing::info!(
                channel = label,
                bytes,
                "push payload rendered; live HTTP transport lands in the deployment phase"
            );
            Ok(())
        })
    })
}

/// Enum dispatch over the three channels (no dyn-async-trait needed).
#[derive(Debug, Clone)]
pub enum PushChannel {
    Mock(Arc<MockChannel>),
    Apns(Arc<ApnsChannel>),
    Fcm(Arc<FcmChannel>),
}

impl PushChannel {
    pub async fn deliver(
        &self,
        platform: PushPlatform,
        push_token: &str,
        envelope: PushEnvelope,
    ) -> Result<(), PushError> {
        match self {
            PushChannel::Mock(channel) => channel.deliver(platform, push_token, envelope).await,
            PushChannel::Apns(channel) => channel.deliver(platform, push_token, envelope).await,
            PushChannel::Fcm(channel) => channel.deliver(platform, push_token, envelope).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Region-aware service
// ---------------------------------------------------------------------------

/// Region-aware push facade held in [`AppState`].
///
/// Selection policy: `global` tries APNs then FCM as configured; `cn` has no
/// real channel yet. The Mock channel is ALWAYS the last resort so a push is
/// never silently dropped.
#[derive(Debug, Clone)]
pub struct PushService {
    region: String,
    apns: Option<Arc<ApnsChannel>>,
    fcm: Option<Arc<FcmChannel>>,
    mock: Arc<MockChannel>,
}

impl PushService {
    /// Builds from the environment (`JIUYUE_REGION` + channel credentials).
    /// Unconfigured real channels simply don't appear in the candidate list.
    pub fn from_env() -> Self {
        Self::from_parts(
            region_from_env(),
            ApnsChannel::from_env().ok(),
            FcmChannel::from_env().ok(),
        )
    }

    pub fn from_parts(region: String, apns: Option<ApnsChannel>, fcm: Option<FcmChannel>) -> Self {
        Self {
            region,
            apns: apns.map(Arc::new),
            fcm: fcm.map(Arc::new),
            mock: Arc::new(MockChannel::new()),
        }
    }

    /// Shared Mock handle (dev endpoint / tests).
    pub fn mock(&self) -> Arc<MockChannel> {
        Arc::clone(&self.mock)
    }

    /// Ordered candidate channels for the configured region. Exposed for
    /// diagnostics; [`Self::deliver`] is the actual entry point.
    pub fn candidates(&self) -> Vec<PushChannel> {
        let mut channels = Vec::new();
        if self.region != "cn" {
            if let Some(apns) = &self.apns {
                channels.push(PushChannel::Apns(Arc::clone(apns)));
            }
            if let Some(fcm) = &self.fcm {
                channels.push(PushChannel::Fcm(Arc::clone(fcm)));
            }
        }
        // Last resort, ALWAYS present: nothing silently drops.
        channels.push(PushChannel::Mock(Arc::clone(&self.mock)));
        channels
    }

    /// Delivers through the first channel that accepts; every real-channel
    /// failure is logged and falls through to the Mock record.
    pub async fn deliver(
        &self,
        platform: PushPlatform,
        push_token: &str,
        envelope: PushEnvelope,
    ) -> Result<(), PushError> {
        for channel in self.candidates() {
            match channel
                .deliver(platform, push_token, envelope.clone())
                .await
            {
                Ok(()) => return Ok(()),
                Err(err) => {
                    tracing::warn!(
                        %platform,
                        user_id = %envelope.to_user_id,
                        device_id = %envelope.device_id,
                        error = %err,
                        "push channel failed; falling through"
                    );
                }
            }
        }
        unreachable!("candidate list always ends with the infallible Mock channel")
    }
}

/// Parses `JIUYUE_REGION`; unknown values fall back to `global` with a warn.
pub fn region_from_env() -> String {
    region_from_raw(std::env::var("JIUYUE_REGION").ok())
}

/// Pure core of [`region_from_env`]: `"cn"` | `"global"` (default), anything
/// else warns and degrades to `global`.
pub fn region_from_raw(raw: Option<String>) -> String {
    match raw.as_deref().map(str::trim) {
        None | Some("") | Some("global") => "global".to_owned(),
        Some("cn") => "cn".to_owned(),
        Some(other) => {
            tracing::warn!(value = %other, "unknown JIUYUE_REGION; defaulting to global");
            "global".to_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// Dev-only debug surface
// ---------------------------------------------------------------------------

/// `GET /api/dev/push-log` — Mock-channel snapshot as JSON. Compiled only in
/// dev builds (`cfg!(debug_assertions)` gating happens at route registration
/// in `app.rs`); release builds never expose the route.
pub async fn push_log(State(state): State<AppState>) -> Json<serde_json::Value> {
    let snapshot = state.push.mock().snapshot();
    Json(serde_json::to_value(snapshot).unwrap_or_else(|_| serde_json::Value::Array(vec![])))
}

// ---------------------------------------------------------------------------
// Tests: payload shapes + config parsing (transport captured, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures rendered payloads for shape assertions.
    #[allow(clippy::type_complexity)]
    fn capture_transport() -> (
        TransportFn,
        Arc<Mutex<Vec<(PushPlatform, String, serde_json::Value)>>>,
    ) {
        let captured: Arc<Mutex<Vec<(PushPlatform, String, serde_json::Value)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let transport: TransportFn = Arc::new(move |platform, token, payload| {
            sink.lock()
                .expect("capture lock")
                .push((platform, token, payload));
            Box::pin(async { Ok(()) })
        });
        (transport, captured)
    }

    fn sample_envelope() -> PushEnvelope {
        PushEnvelope::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            42,
            "在吗？这是一条用于载荷形状断言的预览文本",
            PushKind::Message,
        )
    }

    #[tokio::test]
    async fn apns_payload_shape_matches_spec() {
        let (transport, captured) = capture_transport();
        let channel = ApnsChannel::new("KEY123", "TEAM456", transport);
        let envelope = sample_envelope();
        channel
            .deliver(PushPlatform::Ios, "ios-device-token", envelope)
            .await
            .expect("deliver");

        let captured = captured.lock().expect("capture lock");
        let (platform, token, payload) = &captured[0];
        assert_eq!(*platform, PushPlatform::Ios);
        assert_eq!(token, "ios-device-token");
        assert!(payload["aps"]["alert"]["title"].is_string());
        assert!(payload["aps"]["alert"]["body"].is_string());
        assert_eq!(payload["aps"]["badge"], 1);
        assert_eq!(payload["conversation_id"], 42);
        assert!(
            payload.get("to").is_none(),
            "APNs shape must not carry FCM fields"
        );
    }

    #[tokio::test]
    async fn fcm_payload_shape_matches_spec() {
        let (transport, captured) = capture_transport();
        let channel = FcmChannel::new(transport);
        let envelope = sample_envelope();
        channel
            .deliver(PushPlatform::AndroidFcm, "fcm-token-xyz", envelope)
            .await
            .expect("deliver");

        let captured = captured.lock().expect("capture lock");
        let (platform, token, payload) = &captured[0];
        assert_eq!(*platform, PushPlatform::AndroidFcm);
        assert_eq!(token, "fcm-token-xyz");
        assert_eq!(payload["to"], "fcm-token-xyz");
        assert!(payload["notification"]["title"].is_string());
        assert!(payload["notification"]["body"].is_string());
        assert_eq!(payload["data"]["conversation_id"], "42");
        assert_eq!(payload["data"]["kind"], "message");
        assert!(
            payload.get("aps").is_none(),
            "FCM shape must not carry APNs fields"
        );
    }

    #[tokio::test]
    async fn mock_channel_records_and_evicts_at_capacity() {
        let mock = MockChannel::new();
        let token = "abcdefgh-rest-is-dropped";
        mock.deliver(PushPlatform::AndroidChina, token, sample_envelope())
            .await
            .expect("deliver");
        let snapshot = mock.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].token_prefix, "abcdefgh");
        assert_eq!(snapshot[0].platform, PushPlatform::AndroidChina);
        assert_eq!(snapshot[0].envelope.kind, PushKind::Message);

        // Ring buffer eviction: fill past capacity, oldest entries vanish.
        for _ in 0..MOCK_RING_CAPACITY {
            mock.deliver(PushPlatform::Ios, "t", sample_envelope())
                .await
                .expect("deliver");
        }
        let snapshot = mock.snapshot();
        assert_eq!(snapshot.len(), MOCK_RING_CAPACITY);
        assert_ne!(
            snapshot[0].token_prefix, "abcdefgh",
            "oldest entry must be evicted"
        );
    }

    #[test]
    fn envelope_preview_is_char_truncated() {
        let long_body = "x".repeat(200);
        let envelope = PushEnvelope::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            1,
            &long_body,
            PushKind::Message,
        );
        assert_eq!(
            envelope.sender_username_preview.chars().count(),
            PREVIEW_MAX_CHARS
        );

        // Multibyte safety: 50 CJK chars clamp to exactly 40 whole chars.
        let cjk = "字".repeat(50);
        let envelope =
            PushEnvelope::new(Uuid::now_v7(), Uuid::now_v7(), 1, &cjk, PushKind::Message);
        assert_eq!(envelope.sender_username_preview.chars().count(), 40);
        assert_eq!(envelope.sender_username_preview, "字".repeat(40));
    }

    #[test]
    fn platform_parse_covers_column_values_and_ignores_web() {
        assert_eq!(PushPlatform::parse("ios"), Some(PushPlatform::Ios));
        assert_eq!(
            PushPlatform::parse("android-fcm"),
            Some(PushPlatform::AndroidFcm)
        );
        assert_eq!(
            PushPlatform::parse("android-cn"),
            Some(PushPlatform::AndroidChina)
        );
        assert_eq!(PushPlatform::parse("web"), None);
        assert_eq!(PushPlatform::parse(""), None);
    }

    #[test]
    fn apns_from_lookup_requires_full_credential_trio() {
        use base64::Engine as _;
        let missing = ApnsChannel::from_lookup(|_| None);
        assert_eq!(missing.unwrap_err(), "not configured");

        let partial = ApnsChannel::from_lookup(|name| match name {
            "JIUYUE_APNS_KEY_ID" => Some("key".into()),
            _ => None,
        });
        assert_eq!(partial.unwrap_err(), "not configured");

        let bad_b64 = ApnsChannel::from_lookup(|name| match name {
            "JIUYUE_APNS_TOKEN_B64" => Some("!!!not-base64!!!".into()),
            "JIUYUE_APNS_KEY_ID" => Some("key".into()),
            "JIUYUE_APNS_TEAM_ID" => Some("team".into()),
            _ => None,
        });
        assert!(bad_b64.err().unwrap().contains("base64"));

        let ok = ApnsChannel::from_lookup(|name| match name {
            "JIUYUE_APNS_TOKEN_B64" => {
                Some(base64::engine::general_purpose::STANDARD.encode(b"secret"))
            }
            "JIUYUE_APNS_KEY_ID" => Some("key".into()),
            "JIUYUE_APNS_TEAM_ID" => Some("team".into()),
            _ => None,
        });
        assert!(ok.is_ok());
    }

    #[test]
    fn fcm_from_lookup_requires_server_key() {
        let missing = FcmChannel::from_lookup(|_| None);
        assert_eq!(missing.unwrap_err(), "not configured");

        let blank = FcmChannel::from_lookup(|name| match name {
            "JIUYUE_FCM_SERVER_KEY" => Some("   ".into()),
            _ => None,
        });
        assert_eq!(blank.unwrap_err(), "not configured");

        let ok = FcmChannel::from_lookup(|name| match name {
            "JIUYUE_FCM_SERVER_KEY" => Some("server-key".into()),
            _ => None,
        });
        assert!(ok.is_ok());
    }

    #[tokio::test]
    async fn service_falls_through_to_mock_when_no_real_channel_configured() {
        let service = PushService::from_parts("global".into(), None, None);
        assert_eq!(
            service.candidates().len(),
            1,
            "mock-only when nothing configured"
        );

        let envelope = sample_envelope();
        service
            .deliver(PushPlatform::Ios, "token-1234567890", envelope)
            .await
            .expect("mock last resort must succeed");
        assert_eq!(service.mock().snapshot().len(), 1);
    }

    #[tokio::test]
    async fn service_region_cn_skips_real_channels_but_keeps_mock() {
        let (transport, captured) = capture_transport();
        let apns = ApnsChannel::new("k", "t", transport);
        let service = PushService::from_parts("cn".into(), Some(apns), None);
        assert_eq!(
            service.candidates().len(),
            1,
            "cn region has no real channel this milestone"
        );
        service
            .deliver(PushPlatform::AndroidChina, "cn-token", sample_envelope())
            .await
            .expect("deliver");
        assert!(
            captured.lock().expect("capture lock").is_empty(),
            "real channel must be skipped in cn"
        );
        assert_eq!(service.mock().snapshot().len(), 1);
    }

    #[tokio::test]
    async fn service_global_prefers_apns_then_fcm_then_mock() {
        let (apns_transport, apns_captured) = capture_transport();
        let failing: TransportFn = Arc::new(|_p, _t, _payload| {
            Box::pin(async { Err(PushError::Transport("boom".into())) })
        });

        // APNs healthy → wins, FCM untouched, Mock untouched.
        let service = PushService::from_parts(
            "global".into(),
            Some(ApnsChannel::new("k", "t", apns_transport)),
            Some(FcmChannel::new(Arc::clone(&failing))),
        );
        assert_eq!(service.candidates().len(), 3);
        service
            .deliver(PushPlatform::Ios, "tok", sample_envelope())
            .await
            .expect("deliver");
        assert_eq!(apns_captured.lock().expect("lock").len(), 1);
        assert!(service.mock().snapshot().is_empty());

        // Both real channels fail → Mock records (nothing silently drops).
        let service = PushService::from_parts(
            "global".into(),
            Some(ApnsChannel::new("k", "t", Arc::clone(&failing))),
            Some(FcmChannel::new(failing)),
        );
        service
            .deliver(PushPlatform::AndroidFcm, "tok", sample_envelope())
            .await
            .expect("mock fallback succeeds");
        assert_eq!(service.mock().snapshot().len(), 1);
    }

    #[test]
    fn region_parsing_defaults_to_global() {
        assert_eq!(region_from_raw(None), "global");
        assert_eq!(region_from_raw(Some(String::new())), "global");
        assert_eq!(region_from_raw(Some("cn".into())), "cn");
        assert_eq!(region_from_raw(Some(" global ".into())), "global");
        assert_eq!(
            region_from_raw(Some("mars".into())),
            "global",
            "unknown values degrade"
        );
    }
}
