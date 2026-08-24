/**
 * JiuYue wire protocol v1 — TypeScript mirror of the `jiuyue-protocol` crate.
 *
 * Envelope: `{ "v": 1, "t": "<type>", "d": {...} }`. This module is hand-written
 * on purpose (spec: 手写镜像，CI 校验一致性): the golden JSON fixtures under
 * `crates/protocol/tests/golden/` pin both sides together, and
 * `frames.test.ts` asserts Rust-authored fixtures parse into this union and
 * serialize back unchanged.
 *
 * | t                | direction | d                                                              |
 * |------------------|-----------|----------------------------------------------------------------|
 * | auth.ticket.req  | C → S     | `{}`                                                           |
 * | auth.ticket.res  | S → C     | `{ ticket }`                                                   |
 * | msg.send         | C → S     | `{ conversation_id, client_msg_id, body, reply_to? }`          |
 * | msg.ack          | S → C     | `{ client_msg_id, message_id, seq, duplicate }`                |
 * | msg.new          | S → C     | `{ message_id, conversation_id, seq, sender_id, body, sent_at, reply_to_message_id?, reply_to_sender_id?, reply_to_body_preview?, forwarded_from_username?, recalled? }` |
 * | sync.req         | C → S     | `{ cursors: [{ conversation_id, last_delivered_seq }] }`       |
 * | sync.res         | S → C     | `{ messages: [msg.new], complete }`                            |
 * | read.update      | C → S     | `{ conversation_id, last_read_seq }`                           |
 * | read.receipt     | S → C     | `{ conversation_id, user_id, last_read_seq }`                  |
 * | typing           | C ⇄ S     | `{ conversation_id, state: "start"\|"stop", user_id? }`        |
 * | msg.recall       | C → S     | `{ conversation_id, message_id }`                              |
 * | msg.recalled     | S → C     | `{ conversation_id, message_id }`                              |
 * | e2ee.msg         | C ⇄ S     | `{ conversation_id, ciphertext, message_type }`                |
 * | friend.requested | S → C     | `{ request_id, from: { user_id, username } }`                  |
 * | friend.accepted  | S → C     | `{ friend: { user_id, username } }`                            |
 * | error            | S → C     | `{ code, message, retryable }`                                 |
 *
 * M2 optional `msg.*` metadata fields are null-absent: absent on the wire
 * when unset (never serialized as `null`), keeping M1 frames byte-identical.
 * On `typing`, `user_id` is present only on the server→client relay (the
 * server stamps the authenticated sender; a client-supplied value is never
 * trusted).
 *
 * Parity notes with the Rust decoder:
 * - An unknown `t` does NOT throw; it degrades to an `error` frame with code
 *   `unknown_type`, echoing the original type in `message` (forward compat).
 * - A structurally broken envelope (bad version, missing/non-string `t`,
 *   payload not matching the declared type) throws {@link ProtocolParseError},
 *   mirroring the crate's `FrameError`.
 * - int64 fields (`conversation_id`, `seq`) are typed `number`; values beyond
 *   ±2^53 would lose precision in JS and are out of MVP scope.
 */

export const PROTOCOL_VERSION = 1;

export interface AuthTicketReq {}

export interface AuthTicketRes {
  ticket: string;
}

export interface MsgSend {
  conversation_id: number;
  /** Client-generated idempotency key (UUIDv7 string). */
  client_msg_id: string;
  body: string;
  /** Optional reply target; must be a message of the same conversation. */
  reply_to?: string;
}

export interface MsgAck {
  client_msg_id: string;
  message_id: string;
  seq: number;
  /** True when this ack is for a duplicate delivery of the same client_msg_id. */
  duplicate: boolean;
}

/** Server-pushed message record; shape of `msg.new` and of sync.res entries. */
export interface MsgNew {
  message_id: string;
  conversation_id: number;
  seq: number;
  sender_id: string;
  body: string;
  /** RFC 3339 timestamp string, passed through verbatim. */
  sent_at: string;
  /** Reply metadata (null-absent on plain messages). */
  reply_to_message_id?: string;
  reply_to_sender_id?: string;
  /** First 80 chars of the replied-to body, decrypted server-side. */
  reply_to_body_preview?: string;
  forwarded_from_username?: string;
  /** True for recall tombstones; `body` is then empty by contract. */
  recalled?: boolean;
}

export interface SyncCursor {
  conversation_id: number;
  last_delivered_seq: number;
}

export interface SyncReq {
  cursors: SyncCursor[];
}

export interface SyncRes {
  messages: MsgNew[];
  complete: boolean;
}

/** Client → server: reader advanced its read cursor (conversation open). */
export interface ReadUpdate {
  conversation_id: number;
  last_read_seq: number;
}

/** Server → peer: `user_id`'s persisted read cursor advanced. */
export interface ReadReceipt {
  conversation_id: number;
  user_id: string;
  last_read_seq: number;
}

export type TypingState = "start" | "stop";

/**
 * Typing signal. Client→server frames omit `user_id`; the server relay to
 * peers always carries it (stamped from the authenticated connection).
 */
export interface Typing {
  conversation_id: number;
  state: TypingState;
  user_id?: string;
}

/** Client → server: recall request (sender-only, 120s window). */
export interface MsgRecall {
  conversation_id: number;
  message_id: string;
}

/** Server → all members: message became a tombstone. */
export interface MsgRecalled {
  conversation_id: number;
  message_id: string;
}

/**
 * M3 secret chats: opaque end-to-end ciphertext relayed verbatim in both
 * directions (and replayed by sync). The server stores the bytes but can
 * never read them; `message_type` is 0 for a session-init message and 1 for
 * a normal ratchet message (see lib/crypto/olm-lite.ts).
 */
export interface E2eeMsg {
  conversation_id: number;
  /** base64 payload produced by lib/crypto/olm-lite. */
  ciphertext: string;
  /** 0 = session-init, 1 = normal ratchet message. */
  message_type: number;
}

/** Shared `{ user_id, username }` reference inside friend frames. */
export interface FriendUserRef {
  user_id: string;
  username: string;
}

/** Server → client: a peer sent me a friend request. */
export interface FriendRequested {
  request_id: string;
  from: FriendUserRef;
}

/** Server → client: one of my outgoing friend requests was accepted. */
export interface FriendAccepted {
  friend: FriendUserRef;
}

export type ErrorCode =
  | "bad_request"
  | "unauthorized"
  | "not_found"
  | "conflict"
  | "rate_limited"
  | "internal"
  | "unknown_type";

export interface ErrorPayload {
  code: ErrorCode;
  message: string;
  retryable: boolean;
}

export const FRAME_TYPES = [
  "auth.ticket.req",
  "auth.ticket.res",
  "msg.send",
  "msg.ack",
  "msg.new",
  "sync.req",
  "sync.res",
  "read.update",
  "read.receipt",
  "typing",
  "msg.recall",
  "msg.recalled",
  "e2ee.msg",
  "friend.requested",
  "friend.accepted",
  "error",
] as const;

export type FrameType = (typeof FRAME_TYPES)[number];

interface Envelope<T extends FrameType, D> {
  v: number;
  t: T;
  d: D;
}

export type Frame =
  | Envelope<"auth.ticket.req", AuthTicketReq>
  | Envelope<"auth.ticket.res", AuthTicketRes>
  | Envelope<"msg.send", MsgSend>
  | Envelope<"msg.ack", MsgAck>
  | Envelope<"msg.new", MsgNew>
  | Envelope<"sync.req", SyncReq>
  | Envelope<"sync.res", SyncRes>
  | Envelope<"read.update", ReadUpdate>
  | Envelope<"read.receipt", ReadReceipt>
  | Envelope<"typing", Typing>
  | Envelope<"msg.recall", MsgRecall>
  | Envelope<"msg.recalled", MsgRecalled>
  | Envelope<"e2ee.msg", E2eeMsg>
  | Envelope<"friend.requested", FriendRequested>
  | Envelope<"friend.accepted", FriendAccepted>
  | Envelope<"error", ErrorPayload>;

/** Thrown when a raw value cannot be interpreted as a v1 frame at all. */
export class ProtocolParseError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolParseError";
  }
}

function isRecord(x: unknown): x is Record<string, unknown> {
  return typeof x === "object" && x !== null && !Array.isArray(x);
}

function hasString(d: Record<string, unknown>, key: string): boolean {
  return typeof d[key] === "string";
}

function hasInt(d: Record<string, unknown>, key: string): boolean {
  const v = d[key];
  return typeof v === "number" && Number.isInteger(v);
}

function hasBool(d: Record<string, unknown>, key: string): boolean {
  return typeof d[key] === "boolean";
}

/** Optional string: absent (or undefined) is fine; present must be a string. */
function hasOptionalString(d: Record<string, unknown>, key: string): boolean {
  const v = d[key];
  return v === undefined || typeof v === "string";
}

export function isAuthTicketReq(d: unknown): d is AuthTicketReq {
  return isRecord(d) && Object.keys(d).length === 0;
}

export function isAuthTicketRes(d: unknown): d is AuthTicketRes {
  return isRecord(d) && hasString(d, "ticket");
}

export function isMsgSend(d: unknown): d is MsgSend {
  return (
    isRecord(d) &&
    hasInt(d, "conversation_id") &&
    hasString(d, "client_msg_id") &&
    hasString(d, "body") &&
    hasOptionalString(d, "reply_to")
  );
}

export function isMsgAck(d: unknown): d is MsgAck {
  return (
    isRecord(d) &&
    hasString(d, "client_msg_id") &&
    hasString(d, "message_id") &&
    hasInt(d, "seq") &&
    hasBool(d, "duplicate")
  );
}

export function isMsgNew(d: unknown): d is MsgNew {
  return (
    isRecord(d) &&
    hasString(d, "message_id") &&
    hasInt(d, "conversation_id") &&
    hasInt(d, "seq") &&
    hasString(d, "sender_id") &&
    hasString(d, "body") &&
    hasString(d, "sent_at") &&
    hasOptionalString(d, "reply_to_message_id") &&
    hasOptionalString(d, "reply_to_sender_id") &&
    hasOptionalString(d, "reply_to_body_preview") &&
    hasOptionalString(d, "forwarded_from_username") &&
    (d["recalled"] === undefined || typeof d["recalled"] === "boolean")
  );
}

export function isSyncCursor(d: unknown): d is SyncCursor {
  return (
    isRecord(d) &&
    hasInt(d, "conversation_id") &&
    hasInt(d, "last_delivered_seq")
  );
}

export function isSyncReq(d: unknown): d is SyncReq {
  if (!isRecord(d) || !Array.isArray(d["cursors"])) return false;
  return d["cursors"].every((c) => isSyncCursor(c));
}

export function isSyncRes(d: unknown): d is SyncRes {
  if (!isRecord(d) || !Array.isArray(d["messages"]) || !hasBool(d, "complete"))
    return false;
  return d["messages"].every((m) => isMsgNew(m));
}

export function isErrorPayload(d: unknown): d is ErrorPayload {
  if (!isRecord(d) || !hasString(d, "message") || !hasBool(d, "retryable"))
    return false;
  const code = d["code"];
  return (
    code === "bad_request" ||
    code === "unauthorized" ||
    code === "not_found" ||
    code === "conflict" ||
    code === "rate_limited" ||
    code === "internal" ||
    code === "unknown_type"
  );
}

export function isReadUpdate(d: unknown): d is ReadUpdate {
  return (
    isRecord(d) && hasInt(d, "conversation_id") && hasInt(d, "last_read_seq")
  );
}

export function isReadReceipt(d: unknown): d is ReadReceipt {
  return (
    isRecord(d) &&
    hasInt(d, "conversation_id") &&
    hasString(d, "user_id") &&
    hasInt(d, "last_read_seq")
  );
}

export function isTypingState(v: unknown): v is TypingState {
  return v === "start" || v === "stop";
}

export function isTyping(d: unknown): d is Typing {
  return (
    isRecord(d) &&
    hasInt(d, "conversation_id") &&
    isTypingState(d["state"]) &&
    hasOptionalString(d, "user_id")
  );
}

export function isMsgRecall(d: unknown): d is MsgRecall {
  return (
    isRecord(d) && hasInt(d, "conversation_id") && hasString(d, "message_id")
  );
}
export function isMsgRecalled(d: unknown): d is MsgRecalled {
  return (
    isRecord(d) && hasInt(d, "conversation_id") && hasString(d, "message_id")
  );
}

export function isE2eeMsg(d: unknown): d is E2eeMsg {
  return (
    isRecord(d) &&
    hasInt(d, "conversation_id") &&
    hasString(d, "ciphertext") &&
    hasInt(d, "message_type")
  );
}

function isFriendUserRef(v: unknown): v is FriendUserRef {
  return isRecord(v) && hasString(v, "user_id") && hasString(v, "username");
}

export function isFriendRequested(d: unknown): d is FriendRequested {
  return (
    isRecord(d) && hasString(d, "request_id") && isFriendUserRef(d["from"])
  );
}

export function isFriendAccepted(d: unknown): d is FriendAccepted {
  return isRecord(d) && isFriendUserRef(d["friend"]);
}

const PAYLOAD_GUARDS: { [T in FrameType]: (d: unknown) => boolean } = {
  "auth.ticket.req": isAuthTicketReq,
  "auth.ticket.res": isAuthTicketRes,
  "msg.send": isMsgSend,
  "msg.ack": isMsgAck,
  "msg.new": isMsgNew,
  "sync.req": isSyncReq,
  "sync.res": isSyncRes,
  "read.update": isReadUpdate,
  "read.receipt": isReadReceipt,
  typing: isTyping,
  "msg.recall": isMsgRecall,
  "msg.recalled": isMsgRecalled,
  "e2ee.msg": isE2eeMsg,
  "friend.requested": isFriendRequested,
  "friend.accepted": isFriendAccepted,
  error: isErrorPayload,
};

/**
 * Parse a raw decoded-JSON value into a {@link Frame}.
 *
 * Unknown frame types degrade to a typed `error` frame (forward compat);
 * structurally invalid envelopes or payloads throw {@link ProtocolParseError}.
 */
export function parseFrame(input: unknown): Frame {
  if (!isRecord(input)) {
    throw new ProtocolParseError("frame must be a JSON object");
  }
  const { v } = input;
  if (typeof v !== "number" || !Number.isInteger(v)) {
    throw new ProtocolParseError("frame envelope field `v` must be an integer");
  }
  if (v !== PROTOCOL_VERSION) {
    throw new ProtocolParseError(
      `unsupported envelope version \`${v}\`; this build speaks v${PROTOCOL_VERSION}`,
    );
  }
  const t = input["t"];
  if (typeof t !== "string") {
    throw new ProtocolParseError("frame envelope is missing string field `t`");
  }
  const d = input["d"] ?? {};

  const guard = (PAYLOAD_GUARDS as Record<string, (d: unknown) => boolean>)[t];
  if (guard === undefined) {
    // Forward compat: never fail the stream on an unrecognized type.
    return {
      v: PROTOCOL_VERSION,
      t: "error",
      d: {
        code: "unknown_type",
        message: `unknown frame type: ${t}`,
        retryable: false,
      },
    };
  }
  if (!guard(d)) {
    throw new ProtocolParseError(
      `payload \`d\` does not match frame type \`${t}\``,
    );
  }
  return { v, t, d } as Frame;
}

/** Parse a raw wire-text frame (one WebSocket text message). */
export function parseFrameText(raw: string): Frame {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (cause) {
    throw new ProtocolParseError(`frame is not valid JSON: ${String(cause)}`);
  }
  return parseFrame(parsed);
}

/** Serialize a frame back to its plain JSON envelope shape. */
export function serializeFrame(frame: Frame): {
  v: number;
  t: FrameType;
  d: unknown;
} {
  return { v: frame.v, t: frame.t, d: structuredClone(frame.d) };
}

/** Non-throwing check: is this value a well-formed v1 frame? */
export function isFrame(input: unknown): input is Frame {
  try {
    parseFrame(input);
    return true;
  } catch {
    return false;
  }
}
