import { describe, expect, it } from "vitest";

// Same golden fixtures the Rust side pins against — single source of truth.
import authTicketReq from "../../../../crates/protocol/tests/golden/auth_ticket_req.json";
import authTicketRes from "../../../../crates/protocol/tests/golden/auth_ticket_res.json";
import msgSend from "../../../../crates/protocol/tests/golden/msg_send.json";
import msgSendMedia from "../../../../crates/protocol/tests/golden/msg_send_media.json";
import msgSendAudio from "../../../../crates/protocol/tests/golden/msg_send_audio.json";
import msgSendForward from "../../../../crates/protocol/tests/golden/msg_send_forward.json";
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
  isE2eeMsg,
  isFrame,
  isMediaRef,
  isMsgNew,
  isMsgSend,
  isSyncCursor,
  isSyncRes,
  parseFrame,
  parseFrameText,
  serializeFrame,
  type Frame,
} from "./frames";

const KNOWN_FIXTURES: [string, object][] = [
  ["auth_ticket_req", authTicketReq],
  ["auth_ticket_res", authTicketRes],
  ["msg_send", msgSend],
  ["msg_send_media", msgSendMedia],
  ["msg_send_audio", msgSendAudio],
  ["msg_send_forward", msgSendForward],
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

describe("M8 media references (additive, null-absent)", () => {
  const validMedia = {
    media_id: "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071",
    kind: "image",
    mime: "image/png",
    bytes: 12345,
    file_name: "photo.png",
    width: 800,
    height: 600,
  };

  it("accepts a well-formed MediaRef (with and without dimensions)", () => {
    expect(isMediaRef(validMedia)).toBe(true);
    expect(
      isMediaRef({
        media_id: validMedia.media_id,
        kind: validMedia.kind,
        mime: validMedia.mime,
        bytes: validMedia.bytes,
        file_name: validMedia.file_name,
      }),
    ).toBe(true);
  });

  it("rejects malformed MediaRefs", () => {
    expect(isMediaRef(null)).toBe(false);
    expect(isMediaRef("x")).toBe(false);
    expect(isMediaRef({ ...validMedia, media_id: undefined })).toBe(false);
    expect(isMediaRef({ ...validMedia, kind: "sticker" })).toBe(false);
    expect(isMediaRef({ ...validMedia, bytes: "12" })).toBe(false);
    expect(isMediaRef({ ...validMedia, width: "800" })).toBe(false);
    expect(isMediaRef({ ...validMedia, file_name: 7 })).toBe(false);
  });

  it("accepts an audio MediaRef carrying an optional duration_ms", () => {
    const audio = {
      media_id: validMedia.media_id,
      kind: "audio",
      mime: "audio/webm",
      bytes: 4096,
      file_name: "voice.webm",
      duration_ms: 4200,
    };
    expect(isMediaRef(audio)).toBe(true);
    // duration_ms is optional and must be an int when present.
    const { duration_ms: _drop, ...withoutDuration } = audio;
    expect(isMediaRef(withoutDuration)).toBe(true);
    expect(isMediaRef({ ...audio, duration_ms: 1.5 })).toBe(false);
  });

  it("accepts audio media refs with an optional duration_ms", () => {
    const audio = {
      media_id: validMedia.media_id,
      kind: "audio",
      mime: "audio/webm",
      bytes: 4321,
      file_name: "voice.webm",
    };
    expect(isMediaRef(audio)).toBe(true);
    expect(isMediaRef({ ...audio, duration_ms: 3_500 })).toBe(true);
    expect(isMediaRef({ ...audio, duration_ms: "3500" })).toBe(false);
  });

  it("keeps msg.send valid when media is absent (M1 byte-identical)", () => {
    expect(isMsgSend((msgSend as { d: unknown }).d)).toBe(true);
    expect(serializeFrame(parseFrame(msgSend))).toEqual(msgSend);
  });

  it("parses msg.send carrying a media ref and round-trips unchanged", () => {
    const fixture = {
      v: 1,
      t: "msg.send",
      d: {
        conversation_id: 42,
        client_msg_id: "0190aabb-ccdd-7e01-8a1b-2c3d4e5f6071",
        body: "",
        media: validMedia,
      },
    };
    const frame = parseFrame(fixture);
    expect(frame.t).toBe("msg.send");
    if (frame.t !== "msg.send") throw new Error("unreachable");
    expect(frame.d.body).toBe("");
    expect(frame.d.media?.media_id).toBe(validMedia.media_id);
    expect(serializeFrame(frame)).toEqual(fixture);
  });

  it("parses msg.new carrying a media ref and exposes it on the union", () => {
    const fixture = {
      v: 1,
      t: "msg.new",
      d: {
        message_id: "m-media",
        conversation_id: 42,
        seq: 7,
        sender_id: "peer-1",
        body: "",
        sent_at: "2026-09-11T10:00:00Z",
        media: { ...validMedia, kind: "video", mime: "video/mp4" },
      },
    };
    const frame = parseFrame(fixture);
    expect(frame.t).toBe("msg.new");
    if (frame.t !== "msg.new") throw new Error("unreachable");
    expect(frame.d.media?.kind).toBe("video");
    expect(serializeFrame(frame)).toEqual(fixture);
  });

  it("rejects msg.send/msg.new whose media is present but malformed", () => {
    expect(() =>
      parseFrame({
        v: 1,
        t: "msg.send",
        d: {
          conversation_id: 1,
          client_msg_id: "c1",
          body: "",
          media: { kind: "image" },
        },
      }),
    ).toThrow();
    expect(
      isMsgNew({
        message_id: "m",
        conversation_id: 1,
        seq: 1,
        sender_id: "s",
        body: "",
        sent_at: "t",
        media: { ...validMedia, bytes: 1.5 },
      }),
    ).toBe(false);
  });
});

describe("isSyncRes - untagged plain/secret union", () => {
  it("accepts batches mixing plain replays and secret-chat ciphertext", () => {
    expect(
      isSyncRes({
        messages: [
          {
            message_id: "m1",
            conversation_id: 1,
            seq: 1,
            sender_id: "s",
            body: "hi",
            sent_at: "t",
          },
          { conversation_id: 1, client_msg_id: "c1", ciphertext: "opaque", message_type: 1 },
        ],
        complete: true,
      }),
    ).toBe(true);
  });

  it("rejects entries that are neither plain nor ciphertext", () => {
    expect(isSyncRes({ messages: [{ hello: 1 }], complete: true })).toBe(false);
    expect(isSyncRes({ messages: [], complete: "yes" })).toBe(false);
    expect(isE2eeMsg({ conversation_id: 1, client_msg_id: "c1", ciphertext: "c", message_type: 1 })).toBe(true);
  });
});