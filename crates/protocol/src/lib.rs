//! JiuYue wire protocol v1.
//!
//! Versioned WebSocket frame envelope `{ "v": 1, "t": "<type>", "d": {...} }`
//! shared by client and server. Frame types are added per milestone; unknown
//! `t` values deserialize into a typed [`Payload::Error`] frame with
//! [`ErrorCode::UnknownType`] (forward compat), so a newer peer never breaks
//! an older connection mid-stream.
//!
//! # Frame-type table
//!
//! | `t`                | direction | `d` payload                                                                                          |
//! |--------------------|-----------|------------------------------------------------------------------------------------------------------|
//! | `auth.ticket.req`  | C → S     | `{}`                                                                                                 |
//! | `auth.ticket.res`  | S → C     | `{ ticket }`                                                                                         |
//! | `msg.send`         | C → S     | `{ conversation_id, client_msg_id, body, reply_to? }`                                                |
//! | `msg.ack`          | S → C     | `{ client_msg_id, message_id, seq, duplicate }`                                                      |
//! | `msg.new`          | S → C     | `{ message_id, conversation_id, seq, sender_id, body, sent_at, reply_to_message_id?, reply_to_sender_id?, reply_to_body_preview?, forwarded_from_username?, recalled? }` |
//! | `sync.req`         | C → S     | `{ cursors: [{ conversation_id, last_delivered_seq }] }`                                             |
//! | `sync.res`         | S → C     | `{ messages: [msg.new-shaped], complete }`                                                           |
//! | `read.update`      | C → S     | `{ conversation_id, last_read_seq }`                                                                 |
//! | `read.receipt`     | S → C     | `{ conversation_id, user_id, last_read_seq }`                                                        |
//! | `typing`           | C ⇄ S     | `{ conversation_id, state: "start"\|"stop", user_id? }` — `user_id` present only on the server relay   |
//! | `msg.recall`       | C → S     | `{ conversation_id, message_id }`                                                                    |
//! | `msg.recalled`     | S → C     | `{ conversation_id, message_id }`                                                                    |
//! | `error`            | S → C     | `{ code, message, retryable }`                                                                       |
//!
//! Optional `msg.*` metadata fields are null-absent: when absent they are
//! omitted from the wire entirely (never serialized as `null`), so M1-era
//! fixtures and peers stay byte-identical. Reserved namespaces (`e2ee.*`,
//! `signal.*`) follow in later milestones; until then they arrive here as
//! `unknown_type` errors.
//!
//! Field conventions: ids are UUIDv7 strings, `conversation_id`/`seq` are
//! int64, `sent_at` is an RFC 3339 string, all field names snake_case.
//! A TypeScript mirror lives at `app/src/lib/protocol/frames.ts`; both sides
//! are pinned together by the golden fixtures in `tests/golden/`.
//!
//! # Example session (A sends to B)
//!
//! ```text
//! A→S  {"v":1,"t":"auth.ticket.req","d":{}}
//! S→A  {"v":1,"t":"auth.ticket.res","d":{"ticket":"eyJhbGciOi…"}}
//! A→S  {"v":1,"t":"msg.send","d":{"conversation_id":42,"client_msg_id":"018f6d2a-…","body":"在吗？"}}
//! S→A  {"v":1,"t":"msg.ack","d":{"client_msg_id":"018f6d2a-…","message_id":"0190aabb-…","seq":128,"duplicate":false}}
//! S→B  {"v":1,"t":"msg.new","d":{"message_id":"0190aabb-…","conversation_id":42,"seq":128,"sender_id":"018e1122-…","body":"在吗？","sent_at":"2026-08-24T08:30:00.123Z"}}
//!      … B was offline, reconnects and catches up:
//! B→S  {"v":1,"t":"sync.req","d":{"cursors":[{"conversation_id":42,"last_delivered_seq":127}]}}
//! S→B  {"v":1,"t":"sync.res","d":{"messages":[{…seq:128…}],"complete":true}}
//!      … anything goes wrong:
//! S→X  {"v":1,"t":"error","d":{"code":"rate_limited","message":"slow down","retryable":true}}
//! ```

use serde::{
    de::{DeserializeOwned, Error as _},
    Deserialize, Deserializer, Serialize,
};
use serde_json::Value;
use uuid::Uuid;

/// Envelope version marker for all v1 frames.
pub const PROTOCOL_VERSION: u32 = 1;

/// Machine-readable error codes carried by [`Payload::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Unauthorized,
    NotFound,
    Conflict,
    RateLimited,
    Internal,
    /// The peer sent a frame type this build does not know (forward compat).
    UnknownType,
}

/// Delivery cursor: how far a client has consumed one conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCursor {
    pub conversation_id: i64,
    pub last_delivered_seq: i64,
}

/// Server-pushed message record; the shape of `msg.new` and of entries in
/// [`SyncRes::messages`].
///
/// `sent_at` is an RFC 3339 string passed through verbatim so heterogeneous
/// clients agree byte-for-byte on what the server sent.
///
/// M2 optional metadata is null-absent: absent fields are skipped during
/// serialization so a plain M1-shaped message produces the exact same bytes
/// as before (golden fixtures stay byte-identical). `recalled` marks a
/// tombstone — when `true` the server always sends an empty `body` and
/// clients render a localized placeholder instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgNew {
    pub message_id: Uuid,
    pub conversation_id: i64,
    pub seq: i64,
    pub sender_id: Uuid,
    pub body: String,
    pub sent_at: String,
    /// Id of the message this one replies to (same conversation only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<Uuid>,
    /// Sender of the replied-to message, resolved server-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_sender_id: Option<Uuid>,
    /// First 80 chars of the replied-to body (plaintext, decrypted
    /// server-side) so receivers can render the quote without extra fetches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_body_preview: Option<String>,
    /// Original author username when this message is known to be a forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarded_from_username: Option<String>,
    /// True for recall tombstones: `body` is then empty by contract.
    #[serde(default, skip_serializing_if = "is_false")]
    pub recalled: bool,
}

/// `skip_serializing_if` helper: omit `recalled` while it carries its
/// default (`false`) so M1-era frames stay byte-identical.
fn is_false(value: &bool) -> bool {
    !*value
}

/// Server-issued authentication ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTicketRes {
    pub ticket: String,
}

/// Client-to-server send request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgSend {
    pub conversation_id: i64,
    /// Client-generated idempotency key (UUIDv7).
    pub client_msg_id: Uuid,
    pub body: String,
    /// Optional reply target: must reference an existing message in the SAME
    /// conversation or the server rejects the send with `bad_request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<Uuid>,
}

/// Server acknowledgement of a [`MsgSend`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgAck {
    pub client_msg_id: Uuid,
    pub message_id: Uuid,
    pub seq: i64,
    /// True when this ack is for a duplicate delivery of the same
    /// `client_msg_id`.
    pub duplicate: bool,
}

/// Client-to-server catch-up request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReq {
    pub cursors: Vec<SyncCursor>,
}

/// Server catch-up response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRes {
    pub messages: Vec<SyncMessage>,
    /// True when no more messages remain beyond the requested cursors.
    pub complete: bool,
}

/// Typed error payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}

/// Client-to-server read cursor update: the sender has the conversation open
/// and consumed everything up to `last_read_seq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadUpdate {
    pub conversation_id: i64,
    pub last_read_seq: i64,
}

/// Server-to-peer read receipt echo: `user_id` advanced its persisted read
/// cursor to `last_read_seq`. Never sent back to the reader itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadReceipt {
    pub conversation_id: i64,
    pub user_id: Uuid,
    pub last_read_seq: i64,
}

/// Typing state change. Client→server frames carry only
/// `(conversation_id, state)`; the server relay to peers overwrites/adds
/// `user_id` with the authenticated sender (a client-supplied `user_id` is
/// never trusted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Typing {
    pub conversation_id: i64,
    pub state: TypingState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
}

/// Transient typing direction; never persisted anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypingState {
    Start,
    Stop,
}

/// Client-to-server recall request (sender-only, time-windowed — enforced
/// server-side by the domain crate's `RecallPolicy` + `MessageStateMachine`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgRecall {
    pub conversation_id: i64,
    pub message_id: Uuid,
}

/// Server broadcast after a successful recall: the message is now a
/// tombstone. Sent to ALL members including the requester (self-ack style
/// confirmation); content is gone forever — clients render a placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgRecalled {
    pub conversation_id: i64,
    pub message_id: Uuid,
}

/// Client-to-server end-to-end-encrypted send request (secret chats).
///
/// `ciphertext` is an opaque Olm message produced by the sender's device;
/// the server relays and stores it verbatim WITHOUT any ability to decrypt
/// it. `client_msg_id` carries the exact same idempotency contract as
/// [`MsgSend`]; `message_type` mirrors the libolm convention (`0` = pre-key,
/// `1` = normal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eeMsg {
    pub conversation_id: i64,
    /// Client-generated idempotency key (UUIDv7).
    pub client_msg_id: Uuid,
    /// Opaque Olm ciphertext (base64), relayed byte-for-byte.
    pub ciphertext: String,
    /// libolm message type: `0` = pre-key, `1` = normal.
    pub message_type: i32,
}

/// One entry of [`SyncRes::messages`].
///
/// Untagged so the plain variant serializes EXACTLY like the frozen M1/M2
/// `msg.new` shape (existing fixtures stay byte-identical); secret-chat rows
/// replay as `e2ee.msg` entries carrying the stored ciphertext verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SyncMessage {
    Plain(MsgNew),
    Encrypted(E2eeMsg),
}

/// Tagged payload of a [`Frame`] — serialized as `"t"` (type) plus `"d"`
/// (data) inside the envelope.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "t", content = "d")]
pub enum Payload {
    #[serde(rename = "auth.ticket.req")]
    AuthTicketReq {},
    #[serde(rename = "auth.ticket.res")]
    AuthTicketRes(AuthTicketRes),
    #[serde(rename = "msg.send")]
    MsgSend(MsgSend),
    #[serde(rename = "msg.ack")]
    MsgAck(MsgAck),
    #[serde(rename = "msg.new")]
    MsgNew(MsgNew),
    #[serde(rename = "sync.req")]
    SyncReq(SyncReq),
    #[serde(rename = "sync.res")]
    SyncRes(SyncRes),
    #[serde(rename = "read.update")]
    ReadUpdate(ReadUpdate),
    #[serde(rename = "read.receipt")]
    ReadReceipt(ReadReceipt),
    #[serde(rename = "typing")]
    Typing(Typing),
    #[serde(rename = "msg.recall")]
    MsgRecall(MsgRecall),
    #[serde(rename = "msg.recalled")]
    MsgRecalled(MsgRecalled),
    #[serde(rename = "e2ee.msg")]
    E2eeMsg(E2eeMsg),
    #[serde(rename = "error")]
    Error(ErrorPayload),
}

/// Versioned wire envelope: `{ "v": 1, "t": "<type>", "d": {...} }`.
///
/// Serialization is derived; deserialization is hand-written so that an
/// unrecognized `t` yields [`Payload::Error`] with [`ErrorCode::UnknownType`]
/// instead of failing the whole stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Frame {
    /// Envelope version; always [`PROTOCOL_VERSION`] for this model.
    pub v: u32,
    #[serde(flatten)]
    pub payload: Payload,
}

/// Errors produced while decoding a [`Frame`] from JSON.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame must be a JSON object")]
    NotAnObject,
    #[error("frame envelope field `v` must be an unsigned integer")]
    BadVersion,
    #[error("unsupported envelope version `{0}`; this build speaks v{PROTOCOL_VERSION}")]
    UnsupportedVersion(u64),
    #[error("frame envelope is missing string field `t`")]
    MissingType,
    #[error("payload `d` does not match frame type `{frame_type}`: {source}")]
    BadPayload {
        frame_type: String,
        #[source]
        source: serde_json::Error,
    },
}

impl TryFrom<Value> for Frame {
    type Error = FrameError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let Value::Object(mut map) = value else {
            return Err(FrameError::NotAnObject);
        };
        let version = match map.get("v") {
            Some(Value::Number(n)) => n.as_u64().ok_or(FrameError::BadVersion)?,
            _ => return Err(FrameError::BadVersion),
        };
        if version != u64::from(PROTOCOL_VERSION) {
            return Err(FrameError::UnsupportedVersion(version));
        }
        let t = match map.remove("t") {
            Some(Value::String(t)) => t,
            _ => return Err(FrameError::MissingType),
        };
        // A missing `d` behaves like an empty object (e.g. auth.ticket.req).
        let d = map
            .remove("d")
            .unwrap_or_else(|| Value::Object(Default::default()));
        let payload = decode_payload(&t, d)?;
        Ok(Frame {
            v: PROTOCOL_VERSION,
            payload,
        })
    }
}

impl<'de> Deserialize<'de> for Frame {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Buffer through `serde_json::Value` so the type tag can be inspected
        // before committing to a payload schema.
        let value = Value::deserialize(deserializer)?;
        Frame::try_from(value).map_err(D::Error::custom)
    }
}

fn decode_payload(t: &str, d: Value) -> Result<Payload, FrameError> {
    fn decode_as<T: DeserializeOwned>(t: &str, d: Value) -> Result<T, FrameError> {
        serde_json::from_value(d).map_err(|source| FrameError::BadPayload {
            frame_type: t.to_owned(),
            source,
        })
    }

    match t {
        "auth.ticket.req" => Ok(Payload::AuthTicketReq {}),
        "auth.ticket.res" => Ok(Payload::AuthTicketRes(decode_as(t, d)?)),
        "msg.send" => Ok(Payload::MsgSend(decode_as(t, d)?)),
        "msg.ack" => Ok(Payload::MsgAck(decode_as(t, d)?)),
        "msg.new" => Ok(Payload::MsgNew(decode_as(t, d)?)),
        "sync.req" => Ok(Payload::SyncReq(decode_as(t, d)?)),
        "sync.res" => Ok(Payload::SyncRes(decode_as(t, d)?)),
        "read.update" => Ok(Payload::ReadUpdate(decode_as(t, d)?)),
        "read.receipt" => Ok(Payload::ReadReceipt(decode_as(t, d)?)),
        "typing" => Ok(Payload::Typing(decode_as(t, d)?)),
        "msg.recall" => Ok(Payload::MsgRecall(decode_as(t, d)?)),
        "msg.recalled" => Ok(Payload::MsgRecalled(decode_as(t, d)?)),
        "e2ee.msg" => Ok(Payload::E2eeMsg(decode_as(t, d)?)),
        "error" => Ok(Payload::Error(decode_as(t, d)?)),
        other => Ok(Payload::Error(ErrorPayload {
            code: ErrorCode::UnknownType,
            message: format!("unknown frame type: {other}"),
            retryable: false,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn msg_send_frame_serializes_to_versioned_envelope_with_v_t_d_keys() {
        let frame = Frame {
            v: PROTOCOL_VERSION,
            payload: Payload::MsgSend(MsgSend {
                conversation_id: 42,
                client_msg_id: "018f6d2a-7b3c-7c05-9a2f-3d8f1e2b4c11".parse().unwrap(),
                body: "你好".to_owned(),
                reply_to: None,
            }),
        };

        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["v"], json!(1));
        assert_eq!(value["t"], json!("msg.send"));
        assert_eq!(value["d"]["conversation_id"], json!(42));
        assert_eq!(
            value["d"]["client_msg_id"],
            json!("018f6d2a-7b3c-7c05-9a2f-3d8f1e2b4c11")
        );
        assert_eq!(value["d"]["body"], json!("你好"));
        assert_eq!(
            value.as_object().unwrap().len(),
            3,
            "envelope carries exactly v, t, d"
        );
    }

    #[test]
    fn auth_ticket_req_serializes_empty_d_object() {
        let frame = Frame {
            v: 1,
            payload: Payload::AuthTicketReq {},
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value, json!({"v": 1, "t": "auth.ticket.req", "d": {}}));
    }

    #[test]
    fn unknown_frame_type_deserializes_into_error_frame_carrying_original_type() {
        let raw = json!({
            "v": 1,
            "t": "holo.render",
            "d": {"scene": "lobby"}
        });

        let frame: Frame = serde_json::from_value(raw).expect("unknown type must still parse");

        match frame.payload {
            Payload::Error(payload) => {
                assert_eq!(payload.code, ErrorCode::UnknownType);
                assert!(
                    payload.message.contains("holo.render"),
                    "message preserves original t: {}",
                    payload.message
                );
                assert!(!payload.retryable);
            }
            other => panic!("expected Error payload, got {other:?}"),
        }
    }

    #[test]
    fn mixed_stream_with_unknown_frame_keeps_parsing_neighbor_frames() {
        // Simulates a connection-level decode loop: one unrecognized frame in
        // the middle must not poison the frames before or after it.
        let stream = [
            r#"{"v":1,"t":"msg.send","d":{"conversation_id":1,"client_msg_id":"018f6d2a-7b3c-7c05-9a2f-3d8f1e2b4c11","body":"hi"}}"#,
            r#"{"v":1,"t":"future.thing","d":{"x":1}}"#,
            r#"{"v":1,"t":"auth.ticket.req","d":{}}"#,
        ];

        let decoded: Vec<Frame> = stream
            .iter()
            .map(|line| serde_json::from_str(line).expect("each line parses"))
            .collect();

        assert!(matches!(decoded[0].payload, Payload::MsgSend(_)));
        assert!(matches!(
            decoded[1].payload,
            Payload::Error(ErrorPayload {
                code: ErrorCode::UnknownType,
                ..
            })
        ));
        assert!(matches!(decoded[2].payload, Payload::AuthTicketReq {}));
    }

    #[test]
    fn malformed_frames_are_rejected_as_errors_not_panics() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (json!("just a string"), "not an object"),
            (json!({"v": 1, "d": {}}), "missing t"),
            (json!({"v": 1, "t": 7, "d": {}}), "non-string t"),
            (json!({"t": "msg.send", "d": {}}), "missing v"),
            (
                json!({"v": true, "t": "msg.send", "d": {}}),
                "non-numeric v",
            ),
            (
                json!({"v": 2, "t": "msg.send", "d": {}}),
                "unsupported version",
            ),
            (
                json!({"v": 1, "t": "msg.send", "d": {"conversation_id": "not-a-number"}}),
                "payload field type mismatch",
            ),
        ];

        for (raw, why) in cases {
            let result = serde_json::from_value::<Frame>(raw);
            assert!(result.is_err(), "expected rejection: {why}");
        }
    }

    #[test]
    fn m2_optional_fields_are_null_absent_on_plain_messages() {
        // M1 byte-compat guarantee: a message without reply/forward/recall
        // metadata serializes EXACTLY like the frozen M1 shape — no
        // `null`-valued or default-valued keys may appear on the wire.
        let frame = Frame {
            v: 1,
            payload: Payload::MsgNew(MsgNew {
                message_id: "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071".parse().unwrap(),
                conversation_id: 42,
                seq: 128,
                sender_id: "018e1122-3344-7006-9a2b-1c2d3e4f5a6b".parse().unwrap(),
                body: "plain".to_owned(),
                sent_at: "2026-08-24T08:30:00.123Z".to_owned(),
                reply_to_message_id: None,
                reply_to_sender_id: None,
                reply_to_body_preview: None,
                forwarded_from_username: None,
                recalled: false,
            }),
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(
            value,
            json!({
                "v": 1,
                "t": "msg.new",
                "d": {
                    "message_id": "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071",
                    "conversation_id": 42,
                    "seq": 128,
                    "sender_id": "018e1122-3344-7006-9a2b-1c2d3e4f5a6b",
                    "body": "plain",
                    "sent_at": "2026-08-24T08:30:00.123Z"
                }
            })
        );

        let send = Frame {
            v: 1,
            payload: Payload::MsgSend(MsgSend {
                conversation_id: 42,
                client_msg_id: "018f6d2a-7b3c-7c05-9a2f-3d8f1e2b4c11".parse().unwrap(),
                body: "plain".to_owned(),
                reply_to: None,
            }),
        };
        let value = serde_json::to_value(&send).unwrap();
        assert_eq!(value["d"].as_object().unwrap().len(), 3, "no reply_to key");
    }

    #[test]
    fn e2ee_msg_frame_roundtrips_with_opaque_ciphertext() {
        let frame = Frame {
            v: 1,
            payload: Payload::E2eeMsg(E2eeMsg {
                conversation_id: 42,
                client_msg_id: "018f6d2a-7b3c-7c05-9a2f-3d8f1e2b4c11".parse().unwrap(),
                ciphertext: "AwACBkF0ZXN0AQ".to_owned(),
                message_type: 0,
            }),
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["t"], json!("e2ee.msg"));
        assert_eq!(value["d"]["conversation_id"], json!(42));
        assert_eq!(value["d"]["message_type"], json!(0));
        let back: Frame = serde_json::from_value(value).unwrap();
        assert_eq!(frame, back);
    }

    #[test]
    fn sync_res_entries_are_untagged_plain_or_e2ee() {
        let res = Frame {
            v: 1,
            payload: Payload::SyncRes(SyncRes {
                messages: vec![
                    SyncMessage::Plain(MsgNew {
                        message_id: "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071".parse().unwrap(),
                        conversation_id: 42,
                        seq: 128,
                        sender_id: "018e1122-3344-7006-9a2b-1c2d3e4f5a6b".parse().unwrap(),
                        body: "plain".to_owned(),
                        sent_at: "2026-08-24T08:30:00.123Z".to_owned(),
                        reply_to_message_id: None,
                        reply_to_sender_id: None,
                        reply_to_body_preview: None,
                        forwarded_from_username: None,
                        recalled: false,
                    }),
                    SyncMessage::Encrypted(E2eeMsg {
                        conversation_id: 43,
                        client_msg_id: "018f6d2a-7b3c-7c05-9a2f-3d8f1e2b4c11".parse().unwrap(),
                        ciphertext: "Q0lQSEVU".to_owned(),
                        message_type: 1,
                    }),
                ],
                complete: true,
            }),
        };
        let value = serde_json::to_value(&res).unwrap();
        // Plain entry keeps the frozen msg.new shape (no discriminator key).
        assert_eq!(value["d"]["messages"][0]["body"], json!("plain"));
        assert!(value["d"]["messages"][0].get("kind").is_none());
        // Encrypted entry rides as a nested e2ee.msg-shaped object.
        assert_eq!(value["d"]["messages"][1]["ciphertext"], json!("Q0lQSEVU"));
        let back: Frame = serde_json::from_value(value).unwrap();
        assert_eq!(res, back);
    }

    #[test]
    fn every_payload_variant_roundtrips_through_json() {
        let frames = vec![
            Frame {
                v: 1,
                payload: Payload::AuthTicketReq {},
            },
            Frame {
                v: 1,
                payload: Payload::AuthTicketRes(AuthTicketRes {
                    ticket: "tok".to_owned(),
                }),
            },
            Frame {
                v: 1,
                payload: Payload::MsgSend(MsgSend {
                    conversation_id: i64::MAX,
                    client_msg_id: "018f6d2a-7b3c-7c05-9a2f-3d8f1e2b4c11".parse().unwrap(),
                    body: String::new(),
                    reply_to: None,
                }),
            },
            Frame {
                v: 1,
                payload: Payload::MsgAck(MsgAck {
                    client_msg_id: "018f6d2a-7b3c-7c05-9a2f-3d8f1e2b4c11".parse().unwrap(),
                    message_id: "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071".parse().unwrap(),
                    seq: -1,
                    duplicate: true,
                }),
            },
            Frame {
                v: 1,
                payload: Payload::MsgNew(MsgNew {
                    message_id: "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071".parse().unwrap(),
                    conversation_id: 42,
                    seq: 128,
                    sender_id: "018e1122-3344-7006-9a2b-1c2d3e4f5a6b".parse().unwrap(),
                    body: "body".to_owned(),
                    sent_at: "2026-08-24T08:30:00.123Z".to_owned(),
                    reply_to_message_id: None,
                    reply_to_sender_id: None,
                    reply_to_body_preview: None,
                    forwarded_from_username: None,
                    recalled: false,
                }),
            },
            Frame {
                v: 1,
                payload: Payload::SyncReq(SyncReq {
                    cursors: vec![SyncCursor {
                        conversation_id: 7,
                        last_delivered_seq: 0,
                    }],
                }),
            },
            Frame {
                v: 1,
                payload: Payload::SyncRes(SyncRes {
                    messages: Vec::new(),
                    complete: false,
                }),
            },
            Frame {
                v: 1,
                payload: Payload::ReadUpdate(ReadUpdate {
                    conversation_id: 42,
                    last_read_seq: 128,
                }),
            },
            Frame {
                v: 1,
                payload: Payload::ReadReceipt(ReadReceipt {
                    conversation_id: 42,
                    user_id: "018e1122-3344-7006-9a2b-1c2d3e4f5a6b".parse().unwrap(),
                    last_read_seq: 129,
                }),
            },
            Frame {
                v: 1,
                payload: Payload::Typing(Typing {
                    conversation_id: 42,
                    state: TypingState::Start,
                    user_id: None,
                }),
            },
            Frame {
                v: 1,
                payload: Payload::Typing(Typing {
                    conversation_id: 42,
                    state: TypingState::Stop,
                    user_id: Some("018e1122-3344-7006-9a2b-1c2d3e4f5a6b".parse().unwrap()),
                }),
            },
            Frame {
                v: 1,
                payload: Payload::MsgRecall(MsgRecall {
                    conversation_id: 42,
                    message_id: "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071".parse().unwrap(),
                }),
            },
            Frame {
                v: 1,
                payload: Payload::MsgRecalled(MsgRecalled {
                    conversation_id: 42,
                    message_id: "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071".parse().unwrap(),
                }),
            },
            Frame {
                v: 1,
                payload: Payload::Error(ErrorPayload {
                    code: ErrorCode::RateLimited,
                    message: "slow down".to_owned(),
                    retryable: true,
                }),
            },
        ];

        for frame in frames {
            let value = serde_json::to_value(&frame).unwrap();
            let back: Frame = serde_json::from_value(value).unwrap();
            assert_eq!(frame, back, "variant {:?} roundtrips", frame.payload);
        }
    }
}
