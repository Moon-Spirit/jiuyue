import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "../testing/fake-websocket";
import { useRealtimeStore } from "./realtime";
import { TYPING_REFRESH_MS, TYPING_TTL_MS, useTypingStore } from "./typing";

const ACCESS_KEY = "jiuyue.auth.access_token";
const CONVERSATION_ID = "01JABC1234567890ABCDEFGHJ1";
const PEER_ID = "01JABC1234567890ABCDEFGHJ3";
const OTHER_ID = "01JABC1234567890ABCDEFGHJ4";

/** One Typing envelope as the server pushes it, produced from the contract. */
function typingEnvelope(
  userId: string,
  state: "started" | "stopped",
  sequence: number,
): string {
  return JSON.stringify({
    v: 1,
    s: sequence,
    ts: 1_750_000_000_000,
    e: {
      t: "Typing",
      d: { conversation_id: CONVERSATION_ID, user_id: userId, state },
    },
  });
}

/** Every outgoing `Typing` frame the fake socket captured, in order. */
function typingFrames(): Array<{ t: string; d: unknown }> {
  return FakeWebSocket.latest()
    .sent.map((text) => JSON.parse(text) as { e: { t: string; d: unknown } })
    .filter((envelope) => envelope.e.t === "Typing")
    .map((envelope) => envelope.e);
}

describe("useTypingStore", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    FakeWebSocket.reset();
    vi.stubGlobal("WebSocket", FakeWebSocket);
    vi.useFakeTimers();
    window.localStorage.setItem(ACCESS_KEY, "test-access");
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
    window.localStorage.clear();
  });

  it("knows nobody is typing before anything arrives", () => {
    expect(useTypingStore().typersFor(CONVERSATION_ID)).toEqual([]);
  });

  it("records a started indicator and clears it when the stop arrives", () => {
    const typing = useTypingStore();

    typing.apply({
      conversation_id: CONVERSATION_ID,
      user_id: PEER_ID,
      state: "started",
    });
    expect(typing.typersFor(CONVERSATION_ID)).toEqual([PEER_ID]);

    typing.apply({
      conversation_id: CONVERSATION_ID,
      user_id: PEER_ID,
      state: "stopped",
    });
    expect(typing.typersFor(CONVERSATION_ID)).toEqual([]);
    expect(typing.typers[CONVERSATION_ID]).toBeUndefined();
  });

  it("reports every Participant who is typing in a Group", () => {
    const typing = useTypingStore();

    typing.apply({
      conversation_id: CONVERSATION_ID,
      user_id: PEER_ID,
      state: "started",
    });
    typing.apply({
      conversation_id: CONVERSATION_ID,
      user_id: OTHER_ID,
      state: "started",
    });

    expect([...typing.typersFor(CONVERSATION_ID)].sort()).toEqual(
      [OTHER_ID, PEER_ID].sort(),
    );
  });

  it("expires an indicator on its own without a stop", () => {
    const typing = useTypingStore();
    typing.apply({
      conversation_id: CONVERSATION_ID,
      user_id: PEER_ID,
      state: "started",
    });
    expect(typing.isTyping(CONVERSATION_ID, PEER_ID)).toBe(true);

    // A client that closes mid-sentence never sends a stop; the receiver's own
    // bounded clock is what removes it.
    vi.advanceTimersByTime(TYPING_TTL_MS + 1_000);

    expect(typing.typersFor(CONVERSATION_ID)).toEqual([]);
    expect(typing.isTyping(CONVERSATION_ID, PEER_ID)).toBe(false);
  });

  it("adopts typing events pushed over the socket", () => {
    const typing = useTypingStore();
    useRealtimeStore().connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    socket.emitMessage(typingEnvelope(PEER_ID, "started", 1));
    expect(typing.isTyping(CONVERSATION_ID, PEER_ID)).toBe(true);

    socket.emitMessage(typingEnvelope(PEER_ID, "stopped", 2));
    expect(typing.isTyping(CONVERSATION_ID, PEER_ID)).toBe(false);
  });

  it("collapses a burst of input into one signal per refresh window", () => {
    useTypingStore();
    useRealtimeStore().connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    const typing = useTypingStore();
    for (let keystroke = 0; keystroke < 20; keystroke += 1) {
      typing.noteInput(CONVERSATION_ID, true);
    }

    expect(typingFrames()).toEqual([
      {
        t: "Typing",
        d: { conversation_id: CONVERSATION_ID, state: "started" },
      },
    ]);

    // Past the refresh window the client re-signals, so the server's own throttle
    // always has a fresh signal to coalesce.
    vi.advanceTimersByTime(TYPING_REFRESH_MS);
    typing.noteInput(CONVERSATION_ID, true);
    expect(typingFrames()).toHaveLength(2);

    typing.noteInput(CONVERSATION_ID, false);
    expect(typingFrames()).toHaveLength(3);
    expect(typingFrames()[2]?.d).toEqual({
      conversation_id: CONVERSATION_ID,
      state: "stopped",
    });

    // A second stop for the same draft is not sent again.
    typing.noteInput(CONVERSATION_ID, false);
    expect(typingFrames()).toHaveLength(3);
  });

  it("forgets one Conversation without touching the others", () => {
    const typing = useTypingStore();
    const otherConversation = "01JABC1234567890ABCDEFGHJ9";
    typing.apply({
      conversation_id: CONVERSATION_ID,
      user_id: PEER_ID,
      state: "started",
    });
    typing.apply({
      conversation_id: otherConversation,
      user_id: OTHER_ID,
      state: "started",
    });

    typing.clearConversation(CONVERSATION_ID);

    expect(typing.typersFor(CONVERSATION_ID)).toEqual([]);
    expect(typing.typersFor(otherConversation)).toEqual([OTHER_ID]);
  });

  it("forgets everyone when the session ends", () => {
    const typing = useTypingStore();
    typing.apply({
      conversation_id: CONVERSATION_ID,
      user_id: PEER_ID,
      state: "started",
    });

    typing.clear();

    expect(Object.keys(typing.typers)).toHaveLength(0);
    expect(typing.typersFor(CONVERSATION_ID)).toEqual([]);
  });
});
