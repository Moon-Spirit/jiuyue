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
 * | msg.send         | C → S     | `{ conversation_id, client_msg_id, body }`                     |
 * | msg.ack          | S → C     | `{ client_msg_id, message_id, seq, duplicate }`                |
 * | msg.new          | S → C     | `{ message_id, conversation_id, seq, sender_id, body, sent_at }` |
 * | sync.req         | C → S     | `{ cursors: [{ conversation_id, last_delivered_seq }] }`       |
 * | sync.res         | S → C     | `{ messages: [msg.new], complete }`                            |
 * | error            | S → C     | `{ code, message, retryable }`                                 |
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
    hasString(d, "body")
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
    hasString(d, "sent_at")
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

const PAYLOAD_GUARDS: { [T in FrameType]: (d: unknown) => boolean } = {
  "auth.ticket.req": isAuthTicketReq,
  "auth.ticket.res": isAuthTicketRes,
  "msg.send": isMsgSend,
  "msg.ack": isMsgAck,
  "msg.new": isMsgNew,
  "sync.req": isSyncReq,
  "sync.res": isSyncRes,
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
