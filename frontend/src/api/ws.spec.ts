import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "../testing/fake-websocket";
import {
  RealtimeSocket,
  reconnectDelay,
  type ReconnectPolicy,
  type SocketStatus,
} from "./ws";

/** A deterministic policy so the bounds are arithmetic, not vibes. */
const POLICY: ReconnectPolicy = {
  initialDelayMs: 1000,
  maxDelayMs: 8000,
  factor: 2,
  jitter: 0.5,
};

/** The raw (un-jittered) exponential delay for an attempt, capped by the ceiling. */
function rawDelay(policy: ReconnectPolicy, attempt: number): number {
  return Math.min(
    policy.maxDelayMs,
    policy.initialDelayMs * policy.factor ** attempt,
  );
}

/** Build a socket over the fake transport and record its status transitions. */
function build(options: {
  policy?: ReconnectPolicy;
  staleAfterMs?: number;
  random?: () => number;
}): { socket: RealtimeSocket; statuses: SocketStatus[] } {
  const statuses: SocketStatus[] = [];
  const socket = new RealtimeSocket(
    {
      onEnvelope: () => {},
      onStatus: (status) => statuses.push(status),
    },
    {
      token: () => "test-token",
      policy: options.policy ?? POLICY,
      staleAfterMs: options.staleAfterMs ?? 0,
      random: options.random ?? (() => 0),
    },
  );
  return { socket, statuses };
}

describe("reconnectDelay", () => {
  it("stays within the jittered exponential bounds for every attempt", () => {
    for (let attempt = 0; attempt < 8; attempt += 1) {
      const raw = rawDelay(POLICY, attempt);
      const floor = Math.round(raw * (1 - POLICY.jitter));
      const ceiling = Math.round(raw);

      // `random() === 0` leaves the delay whole; `random() === 1` removes the
      // maximum jitter slice. Neither may cross the cap or go negative.
      expect(reconnectDelay(POLICY, attempt, () => 0)).toBe(ceiling);
      expect(reconnectDelay(POLICY, attempt, () => 1)).toBe(floor);

      const middle = reconnectDelay(POLICY, attempt, () => 0.5);
      expect(middle).toBeGreaterThanOrEqual(floor);
      expect(middle).toBeLessThanOrEqual(ceiling);
    }
  });

  it("never exceeds the ceiling however many attempts fail", () => {
    expect(reconnectDelay(POLICY, 50, () => 0)).toBe(POLICY.maxDelayMs);
    expect(reconnectDelay(POLICY, 50, () => 1)).toBeLessThanOrEqual(
      POLICY.maxDelayMs,
    );
    expect(reconnectDelay(POLICY, 0, () => 2)).toBeGreaterThanOrEqual(0);
  });
});

describe("RealtimeSocket reconnection", () => {
  beforeEach(() => {
    FakeWebSocket.reset();
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("waits the full exponential delay when jitter removes nothing", () => {
    vi.useFakeTimers();
    const { socket } = build({ random: () => 0 });

    socket.connect();
    FakeWebSocket.latest().emitOpen();
    FakeWebSocket.latest().emitClose();

    vi.advanceTimersByTime(POLICY.initialDelayMs - 1);
    expect(FakeWebSocket.instances).toHaveLength(1);

    vi.advanceTimersByTime(1);
    expect(FakeWebSocket.instances).toHaveLength(2);

    // The second attempt doubles the delay; the second socket never opened, so the
    // attempt counter was not reset.
    FakeWebSocket.latest().emitClose();
    vi.advanceTimersByTime(POLICY.initialDelayMs * POLICY.factor - 1);
    expect(FakeWebSocket.instances).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(FakeWebSocket.instances).toHaveLength(3);
  });

  it("shortens the delay by up to the jitter fraction", () => {
    vi.useFakeTimers();
    const { socket } = build({ random: () => 1 });

    socket.connect();
    FakeWebSocket.latest().emitOpen();
    FakeWebSocket.latest().emitClose();

    // Full jitter removes half of the initial delay, so 500 ms is enough.
    vi.advanceTimersByTime(POLICY.initialDelayMs / 2 - 1);
    expect(FakeWebSocket.instances).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it("closes a stalled connection and reconnects", () => {
    vi.useFakeTimers();
    const { socket, statuses } = build({
      policy: { initialDelayMs: 100, maxDelayMs: 100, factor: 2, jitter: 0 },
      staleAfterMs: 1000,
    });

    socket.connect();
    FakeWebSocket.latest().emitOpen();

    // No envelope arrives: the watchdog must treat the half-open link as dead.
    vi.advanceTimersByTime(1000);
    expect(statuses).toContain("reconnecting");

    vi.advanceTimersByTime(100);
    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it("re-arms the watchdog whenever an envelope arrives", () => {
    vi.useFakeTimers();
    const { socket } = build({
      policy: { initialDelayMs: 100, maxDelayMs: 100, factor: 2, jitter: 0 },
      staleAfterMs: 1000,
    });

    socket.connect();
    const live = FakeWebSocket.latest();
    live.emitOpen();

    vi.advanceTimersByTime(900);
    // Any frame — even one that does not parse — proves the transport is alive.
    live.emitMessage("not json");
    vi.advanceTimersByTime(900);
    expect(FakeWebSocket.instances).toHaveLength(1);

    // The re-armed watchdog now fires.
    vi.advanceTimersByTime(200);
    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it("retries at once when the browser reports the network is back", () => {
    vi.useFakeTimers();
    const { socket } = build({});

    socket.connect();
    FakeWebSocket.latest().emitOpen();
    FakeWebSocket.latest().emitClose();
    expect(FakeWebSocket.instances).toHaveLength(1);

    window.dispatchEvent(new Event("online"));

    // No timer advance: the backoff was abandoned.
    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it("retries at once when the tab becomes visible again", () => {
    vi.useFakeTimers();
    const { socket } = build({});

    socket.connect();
    FakeWebSocket.latest().emitOpen();
    FakeWebSocket.latest().emitClose();
    expect(FakeWebSocket.instances).toHaveLength(1);

    document.dispatchEvent(new Event("visibilitychange"));

    expect(FakeWebSocket.instances).toHaveLength(2);
  });

  it("ignores a superseded socket closing late", () => {
    vi.useFakeTimers();
    const { socket } = build({});

    socket.connect();
    const first = FakeWebSocket.latest();
    first.emitOpen();
    first.emitClose();

    vi.advanceTimersByTime(POLICY.initialDelayMs);
    expect(FakeWebSocket.instances).toHaveLength(2);

    FakeWebSocket.latest().emitOpen();

    // The first socket reports a close after its replacement is live. That must
    // not schedule a reconnect on top of the healthy socket.
    first.onclose?.(new CloseEvent("close"));
    vi.advanceTimersByTime(POLICY.maxDelayMs);
    expect(FakeWebSocket.instances).toHaveLength(2);
  });
});
