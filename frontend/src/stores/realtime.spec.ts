import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "../testing/fake-websocket";
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

describe("useRealtimeStore", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    FakeWebSocket.reset();
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("opens a same-origin socket on the /ws path", () => {
    const store = useRealtimeStore();
    store.connect();

    expect(FakeWebSocket.instances).toHaveLength(1);

    const url = new URL(FakeWebSocket.latest().url);
    expect(url.pathname).toBe("/ws");
    expect(url.host).toBe(window.location.host);
    expect(store.status).toBe("connecting");
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
});
