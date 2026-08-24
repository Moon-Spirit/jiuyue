import { describe, expect, it } from "vitest";

// Same golden fixtures the Rust side pins against — single source of truth.
import authTicketReq from "../../../../crates/protocol/tests/golden/auth_ticket_req.json";
import authTicketRes from "../../../../crates/protocol/tests/golden/auth_ticket_res.json";
import msgSend from "../../../../crates/protocol/tests/golden/msg_send.json";
import msgAck from "../../../../crates/protocol/tests/golden/msg_ack.json";
import msgNew from "../../../../crates/protocol/tests/golden/msg_new.json";
import syncReq from "../../../../crates/protocol/tests/golden/sync_req.json";
import syncRes from "../../../../crates/protocol/tests/golden/sync_res.json";
import readUpdate from "../../../../crates/protocol/tests/golden/read_update.json";
import readReceipt from "../../../../crates/protocol/tests/golden/read_receipt.json";
import typing from "../../../../crates/protocol/tests/golden/typing.json";
import typingRelay from "../../../../crates/protocol/tests/golden/typing_relay.json";
import msgRecall from "../../../../crates/protocol/tests/golden/msg_recall.json";
import msgRecalled from "../../../../crates/protocol/tests/golden/msg_recalled.json";
import msgSendReply from "../../../../crates/protocol/tests/golden/msg_send_reply.json";
import msgNewExtended from "../../../../crates/protocol/tests/golden/msg_new_extended.json";
import msgNewRecalled from "../../../../crates/protocol/tests/golden/msg_new_recalled.json";
import errorFrame from "../../../../crates/protocol/tests/golden/error.json";
import unknownType from "../../../../crates/protocol/tests/golden/unknown_type.json";

import {
  isFrame,
  isMsgSend,
  isSyncCursor,
  parseFrame,
  parseFrameText,
  serializeFrame,
  type Frame,
} from "./frames";

const KNOWN_FIXTURES: [string, object][] = [
  ["auth_ticket_req", authTicketReq],
  ["auth_ticket_res", authTicketRes],
  ["msg_send", msgSend],
  ["msg_ack", msgAck],
  ["msg_new", msgNew],
  ["sync_req", syncReq],
  ["sync_res", syncRes],
  ["read_update", readUpdate],
  ["read_receipt", readReceipt],
  ["typing", typing],
  ["typing_relay", typingRelay],
  ["msg_recall", msgRecall],
  ["msg_recalled", msgRecalled],
  ["msg_send_reply", msgSendReply],
  ["msg_new_extended", msgNewExtended],
  ["msg_new_recalled", msgNewRecalled],
  ["error", errorFrame],
];

describe("golden fixture roundtrip", () => {
  it.each(KNOWN_FIXTURES)(
    "%s.json parses into the discriminated union",
    (_name, fixture) => {
      const frame = parseFrame(fixture);
      expect(isFrame(frame)).toBe(true);
      expect(frame.t).toBe((fixture as { t: Frame["t"] }).t);
    },
  );

  it.each(KNOWN_FIXTURES)(
    "%s.json serializes back byte-equal (key order aside)",
    (_name, fixture) => {
      expect(serializeFrame(parseFrame(fixture))).toEqual(fixture);
    },
  );

  it("covers all thirteen wire frame types exactly once", () => {
    const seen = new Set(KNOWN_FIXTURES.map(([, f]) => (f as { t: string }).t));
    expect(seen).toEqual(
      new Set([
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
        "error",
      ]),
    );
  });

  it("keeps M1 msg.new byte-identical when M2 metadata is absent", () => {
    // The frozen M1 fixture must not gain null/default keys after the M2
    // extension — null-absent optionals are a wire contract.
    expect(serializeFrame(parseFrame(msgNew))).toEqual(msgNew);
    const d = (parseFrame(msgNew) as Extract<Frame, { t: "msg.new" }>)
      .d as unknown as Record<string, unknown>;
    expect("reply_to_message_id" in d).toBe(false);
    expect("recalled" in d).toBe(false);
  });

  it("parses M2 reply/forward/recall metadata off extended msg.new", () => {
    const frame = parseFrame(msgNewExtended);
    expect(frame.t).toBe("msg.new");
    if (frame.t !== "msg.new") throw new Error("unreachable");
    expect(frame.d.reply_to_message_id).toBe(
      "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071",
    );
    expect(frame.d.reply_to_sender_id).toBe(
      "018f9999-3344-7006-9a2b-1c2d3e4f5a6b",
    );
    expect(frame.d.reply_to_body_preview).toBe("在吗？");
    expect(frame.d.forwarded_from_username).toBe("carol");
    expect(frame.d.recalled).toBeUndefined();
  });

  it("parses recall tombstones with an empty body and recalled flag", () => {
    const frame = parseFrame(msgNewRecalled);
    if (frame.t !== "msg.new") throw new Error("unreachable");
    expect(frame.d.recalled).toBe(true);
    expect(frame.d.body).toBe("");
  });
});

describe("forward compatibility", () => {
  it("parses an unknown frame type into a typed error frame instead of throwing", () => {
    const frame = parseFrame(unknownType);
    expect(frame.v).toBe(1);
    expect(frame.t).toBe("error");
    if (frame.t !== "error") throw new Error("unreachable");
    expect(frame.d.code).toBe("unknown_type");
    expect(frame.d.message).toContain("presence.milestone");
    expect(frame.d.retryable).toBe(false);
  });

  it("re-serializes the degraded unknown-type frame as a valid error envelope", () => {
    expect(serializeFrame(parseFrame(unknownType))).toEqual({
      v: 1,
      t: "error",
      d: {
        code: "unknown_type",
        message: "unknown frame type: presence.milestone",
        retryable: false,
      },
    });
  });

  it("keeps parsing neighbor frames after an unknown one (stream-level compat)", () => {
    const stream = [
      JSON.stringify(msgSend),
      JSON.stringify(unknownType),
      JSON.stringify(authTicketReq),
    ];
    const decoded = stream.map((line) => parseFrameText(line));
    expect(decoded[0].t).toBe("msg.send");
    expect(decoded[1].t).toBe("error");
    expect(decoded[2].t).toBe("auth.ticket.req");
  });
});

describe("malformed envelopes are rejected, not coerced", () => {
  it.each([
    ["null", null],
    ["array", []],
    ["empty object", {}],
    ["missing t", { v: 1, d: {} }],
    ["non-string t", { v: 1, t: 7, d: {} }],
    ["missing v", { t: "msg.send", d: {} }],
    ["unsupported version", { v: 2, t: "msg.send", d: {} }],
    [
      "payload shape mismatch",
      { v: 1, t: "msg.send", d: { conversation_id: "42" } },
    ],
  ])("throws on %s", (_label, raw) => {
    expect(() => parseFrame(raw)).toThrow();
  });
});

describe("type guards", () => {
  it("narrows payloads by shape", () => {
    expect(isMsgSend((msgSend as { d: unknown }).d)).toBe(true);
    expect(isMsgSend((errorFrame as { d: unknown }).d)).toBe(false);
    expect(
      isSyncCursor((syncReq as { d: { cursors: unknown[] } }).d.cursors[0]),
    ).toBe(true);
    expect(isSyncCursor(42)).toBe(false);
  });

  it("isFrame accepts every golden fixture and rejects junk", () => {
    for (const [, fixture] of KNOWN_FIXTURES)
      expect(isFrame(fixture)).toBe(true);
    expect(isFrame(unknownType)).toBe(true); // degrades to error frame, still a Frame
    expect(isFrame({ nope: true })).toBe(false);
  });
});
