import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "../testing/fake-websocket";
import type { ResyncReason } from "../generated/ResyncReason";
import { useRealtimeStore } from "./realtime";

/** A server envelope as it appears on the wire, produced from the contract. */
function pingEnvelope(seq: number, timeMs: number): string {
  return JSON.stringify({
    v: 1,
    s: seq,
    ts: timeMs,
    e: { t: "Ping", d: { seq, time_ms: timeMs } },
  });
}

/** A heartbeat that names the connection it belongs to, as the server sends it. */
function connectionPing(
  seq: number,
  timeMs: number,
  connectionId: number,
): string {
  return JSON.stringify({
    v: 1,
    s: seq,
    ts: timeMs,
    e: {
      t: "Ping",
      d: { seq, time_ms: timeMs, connection_id: connectionId },
    },
  });
}

/** The server's explicit answer to a resume handshake. */
function resyncEnvelope(seq: number, reason: ResyncReason): string {
  return JSON.stringify({
    v: 1,
    s: seq,
    ts: 2_000,
    e: { t: "Resync", d: { reason, replayed: 0 } },
  });
}

/** A `NewMessage` at an arbitrary connection sequence, to poke holes in `s`. */
function newMessageAt(seq: number, messageSeq: number): string {
  return JSON.stringify({
    v: 1,
    s: seq,
    ts: 3_000,
    e: {
      t: "NewMessage",
      d: {
        message: {
          id: `M${messageSeq}`,
          conversation_id: "01JABC1234567890ABCDEFGHJ2",
          seq: messageSeq,
          sender_id: "01JABC1234567890ABCDEFGHJ3",
          client_msg_id: `k${messageSeq}`,
          body: "hello",
          created_at_ms: 3_000,
        },
      },
    },
  });
}

/** The parsed client frames a fake socket sent, in order. */
function sentFrames(socket: FakeWebSocket): Record<string, unknown>[] {
  return socket.sent.map((text) => JSON.parse(text) as Record<string, unknown>);
}

const ACCESS_KEY = "jiuyue.auth.access_token";

describe("useRealtimeStore", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    FakeWebSocket.reset();
    vi.stubGlobal("WebSocket", FakeWebSocket);
    // The socket authenticates with the session's access token.
    window.localStorage.setItem(ACCESS_KEY, "test-access");
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
    window.localStorage.clear();
  });

  it("opens a same-origin socket on the /ws path, carrying the access token", () => {
    const store = useRealtimeStore();
    store.connect();

    expect(FakeWebSocket.instances).toHaveLength(1);

    const url = new URL(FakeWebSocket.latest().url);
    expect(url.pathname).toBe("/ws");
    expect(url.host).toBe(window.location.host);
    expect(url.searchParams.get("token")).toBe("test-access");
    expect(store.status).toBe("connecting");
  });

  it("stays idle when signed out instead of opening a socket", () => {
    window.localStorage.clear();

    const store = useRealtimeStore();
    store.connect();

    expect(FakeWebSocket.instances).toHaveLength(0);
    expect(store.status).toBe("idle");
  });

  it("records the ping from an incoming server envelope", () => {
    const store = useRealtimeStore();
    store.connect();

    const socket = FakeWebSocket.latest();
    socket.emitOpen();
    expect(store.status).toBe("open");

    socket.emitMessage(pingEnvelope(1, 1_750_000_000_000));

    expect(store.sequence).toBe(1);
    expect(store.serverTimeMs).toBe(1_750_000_000_000);
    expect(store.receivedAtMs).not.toBeNull();
  });

  it("tracks the newest sequence across successive heartbeats", () => {
    const store = useRealtimeStore();
    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    socket.emitMessage(pingEnvelope(1, 1_000));
    socket.emitMessage(pingEnvelope(2, 2_000));

    expect(store.sequence).toBe(2);
    expect(store.serverTimeMs).toBe(2_000);
  });

  it("ignores malformed frames without disturbing the connection", () => {
    const store = useRealtimeStore();
    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    socket.emitMessage("not json at all");
    socket.emitMessage(JSON.stringify({ v: 1, s: 1 }));

    expect(store.status).toBe("open");
    expect(store.sequence).toBeNull();
    expect(store.serverTimeMs).toBeNull();
  });

  it("reconnects after the socket closes on its own", () => {
    vi.useFakeTimers();

    const store = useRealtimeStore();
    store.connect();
    const first = FakeWebSocket.latest();
    first.emitOpen();

    first.emitClose();
    expect(store.status).toBe("reconnecting");

    vi.advanceTimersByTime(600);

    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it("stops reconnecting after an explicit disconnect", () => {
    vi.useFakeTimers();

    const store = useRealtimeStore();
    store.connect();
    FakeWebSocket.latest().emitOpen();

    store.disconnect();
    vi.advanceTimersByTime(60_000);

    expect(store.status).toBe("closed");
    expect(FakeWebSocket.instances).toHaveLength(1);
  });

  it("sends a client event as a versioned envelope", () => {
    const store = useRealtimeStore();
    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    const sent = store.send({
      t: "SendMessage",
      d: { conversation_id: "c1", client_msg_id: "m1", body: "hi" },
    });

    expect(sent).toBe(true);
    expect(JSON.parse(socket.sent[0] ?? "{}")).toEqual({
      v: 1,
      e: {
        t: "SendMessage",
        d: { conversation_id: "c1", client_msg_id: "m1", body: "hi" },
      },
    });
  });

  it("reports a failed send when the socket is not open", () => {
    const store = useRealtimeStore();
    store.connect();

    const sent = store.send({
      t: "SendMessage",
      d: { conversation_id: "c1", client_msg_id: "m1", body: "hi" },
    });

    expect(sent).toBe(false);
  });

  it("forwards chat events to subscribers and keeps heartbeats to itself", () => {
    const store = useRealtimeStore();
    const received: string[] = [];
    store.onChatEvent((event) => {
      received.push(event.t);
    });

    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    socket.emitMessage(pingEnvelope(1, 1_000));
    socket.emitMessage(
      JSON.stringify({
        v: 1,
        s: 2,
        ts: 2_000,
        e: {
          t: "NewMessage",
          d: {
            message: {
              id: "01JABC1234567890ABCDEFGHJ1",
              conversation_id: "01JABC1234567890ABCDEFGHJ2",
              seq: 1,
              sender_id: "01JABC1234567890ABCDEFGHJ3",
              client_msg_id: "key",
              body: "hello",
              created_at_ms: 2_000,
            },
          },
        },
      }),
    );

    expect(received).toEqual(["NewMessage"]);
    expect(store.serverTimeMs).toBe(1_000);
    expect(store.sequence).toBe(2);
  });

  it("answers the first heartbeat with the wake-up handshake", () => {
    const store = useRealtimeStore();
    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    // Nothing is sent until the heartbeat names the connection.
    expect(socket.sent).toHaveLength(0);

    socket.emitMessage(connectionPing(1, 1_000, 7));

    expect(store.connectionId).toBe(7);
    expect(sentFrames(socket)).toEqual([
      { v: 1, e: { t: "Resume", d: { last_seq: 0, connection_id: null } } },
    ]);
  });

  it("detects a hole in the connection sequence and asks to resume", () => {
    const store = useRealtimeStore();
    const resyncs: ResyncReason[] = [];
    store.onResync((reason) => resyncs.push(reason));

    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();
    socket.emitMessage(connectionPing(1, 1_000, 7));

    // s=2 is skipped: the wire dropped an envelope.
    socket.emitMessage(newMessageAt(3, 1));

    expect(store.sequence).toBe(3);
    expect(resyncs).toEqual(["unavailable"]);
    expect(sentFrames(socket)[1]).toEqual({
      v: 1,
      e: { t: "Resume", d: { last_seq: 1, connection_id: 7 } },
    });
  });

  it("does not mistake a new connection's sequence for a gap", () => {
    const store = useRealtimeStore();
    const resyncs: ResyncReason[] = [];
    store.onResync((reason) => resyncs.push(reason));

    store.connect();
    const first = FakeWebSocket.latest();
    first.emitOpen();
    first.emitMessage(connectionPing(1, 1_000, 7));
    expect(store.sequence).toBe(1);

    // Reconnect: the sequence restarts at 1, which is not a hole.
    store.disconnect();
    store.connect();
    const second = FakeWebSocket.latest();
    second.emitOpen();
    second.emitMessage(connectionPing(1, 2_000, 8));

    expect(store.connectionId).toBe(8);
    expect(store.sequence).toBe(1);
    expect(resyncs).toEqual([]);
    // The handshake names the position consumed on the dead connection so the
    // server can answer `unavailable` (it does) rather than assume continuity.
    expect(sentFrames(second)).toEqual([
      { v: 1, e: { t: "Resume", d: { last_seq: 1, connection_id: null } } },
    ]);
  });

  it("repairs when the server says the resume position is unavailable", () => {
    const store = useRealtimeStore();
    const resyncs: ResyncReason[] = [];
    store.onResync((reason) => resyncs.push(reason));

    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();
    socket.emitMessage(connectionPing(1, 1_000, 7));
    expect(resyncs).toEqual([]);

    socket.emitMessage(resyncEnvelope(2, "unavailable"));

    expect(resyncs).toEqual(["unavailable"]);
  });

  it("keeps a replayed resume quiet", () => {
    const store = useRealtimeStore();
    const resyncs: ResyncReason[] = [];
    store.onResync((reason) => resyncs.push(reason));

    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();
    socket.emitMessage(connectionPing(1, 1_000, 7));

    socket.emitMessage(resyncEnvelope(2, "replayed"));

    expect(resyncs).toEqual([]);
  });

  it("forwards the Device's sync state to chat subscribers", () => {
    const store = useRealtimeStore();
    const received: string[] = [];
    store.onChatEvent((event) => {
      received.push(event.t);
    });

    store.connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    socket.emitMessage(
      JSON.stringify({
        v: 1,
        s: 1,
        ts: 1_000,
        e: {
          t: "SyncState",
          d: {
            cursors: [
              {
                conversation_id: "01JABC1234567890ABCDEFGHJ2",
                last_seq: 5,
              },
            ],
          },
        },
      }),
    );

    // SyncState is chat state carried by the socket: the connection store forwards
    // it and keeps no opinion about how to resume.
    expect(received).toEqual(["SyncState"]);
    expect(store.connectionId).toBeNull();
  });
});
