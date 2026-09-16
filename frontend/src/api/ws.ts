/**
 * Typed WebSocket client for the jiuyue realtime channel.
 *
 * The only contract types this module uses are the generated ones under
 * `src/generated/` — it never re-declares the wire shape. The generated file is
 * produced by `scripts/gen-contract.ps1` from the Rust contract crate and is
 * guarded against drift in CI.
 *
 * Authentication: `GET /ws` requires an access token, and the browser
 * `WebSocket` constructor cannot set an `Authorization` header, so the token
 * travels as a query parameter. The caller supplies a {@link TokenProvider} rather
 * than a fixed string, so every reconnect picks up a token that has been refreshed
 * in the meantime.
 *
 * Reconnection is built in, not bolted on: reloading a reverse proxy (Caddy)
 * forcibly drops every WebSocket, so the client must re-establish the connection
 * on its own. Three things make that robust rather than naive:
 *
 * - **Exponential backoff with jitter.** A Caddy reload drops every socket at
 *   once, so a fixed delay would produce a thundering herd. Each attempt waits
 *   `initial * factor ** attempt`, capped at `maxDelayMs`, minus a random slice
 *   bounded by {@link ReconnectPolicy.jitter}.
 * - **Immediate retry on `online` / visibility change.** When the platform says
 *   the network is back or the tab is visible again, the backoff is abandoned and
 *   a connection is attempted at once.
 * - **A staleness watchdog.** A half-open TCP connection looks healthy, so if no
 *   envelope arrives within {@link RealtimeOptions.staleAfterMs} the socket is
 *   closed and the reconnect path takes over. The server's periodic heartbeat is
 *   what keeps a healthy connection from tripping it.
 */

import type { ClientEnvelope } from "../generated/ClientEnvelope";
import type { ClientEvent } from "../generated/ClientEvent";
import type { ServerEnvelope } from "../generated/ServerEnvelope";

/**
 * Wire protocol version, mirroring `PROTOCOL_VERSION` in the Rust contract.
 *
 * `ts-rs` exports types, not constants, so this is the one value that is mirrored
 * by hand. It is only stamped on outgoing frames; the server ignores it today and
 * a mismatch would be a deliberate breaking change.
 */
const PROTOCOL_VERSION = 1;

/** `WebSocket.OPEN`; spelled out so the global can be substituted in tests. */
const READY_STATE_OPEN = 1;

/** Lifecycle of the realtime socket as the UI sees it. */
export type SocketStatus =
  "idle" | "connecting" | "open" | "reconnecting" | "closed";

/** Backoff policy for automatic reconnection. */
export interface ReconnectPolicy {
  readonly initialDelayMs: number;
  readonly maxDelayMs: number;
  readonly factor: number;
  /**
   * Fraction of each delay that jitter may remove, in `[0, 1]`.
   *
   * `0.5` (the default) shortens a delay by up to half, so a synchronized herd
   * spreads out instead of reconnecting in lockstep.
   */
  readonly jitter: number;
}

/** Callbacks the socket drives. */
export interface SocketHandlers {
  readonly onEnvelope: (envelope: ServerEnvelope) => void;
  readonly onStatus: (status: SocketStatus) => void;
}

/** Resolves the access token used for the next (re)connect. */
export type TokenProvider = () => string | null;

/** Construction options for {@link RealtimeSocket}. */
export interface RealtimeOptions {
  /** Called on every connect attempt; `null` means "not signed in, do not connect". */
  readonly token: TokenProvider;
  /** Backoff overrides, mainly for tests. */
  readonly policy?: Partial<ReconnectPolicy>;
  /**
   * Reconnect when no envelope arrives within this many milliseconds. `0`
   * disables the watchdog. Defaults to two-and-a-half server heartbeat periods.
   */
  readonly staleAfterMs?: number;
  /** Randomness source for jitter, so tests can pin the bounds deterministically. */
  readonly random?: () => number;
}

/** Production backoff: 0.5 s doubling to a 30 s ceiling, half of it jittered away. */
export const DEFAULT_RECONNECT_POLICY: ReconnectPolicy = {
  initialDelayMs: 500,
  maxDelayMs: 30_000,
  factor: 2,
  jitter: 0.5,
};

/** Server heartbeat is every 30 s; two missed beats plus slack is a dead link. */
export const DEFAULT_STALE_AFTER_MS = 75_000;

/**
 * The backoff delay before reconnect attempt number `attempt` (zero-based).
 *
 * Pure and injectable, so the bounds are a unit-testable property rather than a
 * behaviour inferred from timers: `delay ∈ [raw * (1 - jitter), raw]`, where
 * `raw = min(maxDelayMs, initialDelayMs * factor ** attempt)`. Jitter is
 * *subtractive*, so the ceiling is never exceeded and the delay is never negative.
 */
export function reconnectDelay(
  policy: ReconnectPolicy,
  attempt: number,
  random: () => number,
): number {
  const exponential = Math.min(
    policy.maxDelayMs,
    policy.initialDelayMs * policy.factor ** attempt,
  );
  const jittered = exponential * (1 - policy.jitter * random());
  return Math.round(Math.max(0, jittered));
}

/** Same-origin URL of the realtime endpoint, carrying the access token. */
export function realtimeUrl(token: string | null): string {
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  const base = `${scheme}//${window.location.host}/ws`;
  return token === null ? base : `${base}?token=${encodeURIComponent(token)}`;
}

/**
 * Parse a text frame into a `ServerEnvelope`, or `null` when it is not one.
 *
 * Unknown event types still parse: the envelope is accepted and the caller
 * decides what to do with the event, which is what keeps new server variants
 * backward compatible.
 */
export function parseServerEnvelope(text: string): ServerEnvelope | null {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  return isServerEnvelope(value) ? value : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isServerEnvelope(value: unknown): value is ServerEnvelope {
  if (!isRecord(value)) return false;

  const event = value["e"];
  return (
    typeof value["v"] === "number" &&
    typeof value["s"] === "number" &&
    typeof value["ts"] === "number" &&
    isRecord(event) &&
    typeof event["t"] === "string" &&
    "d" in event
  );
}

/**
 * Auto-reconnecting realtime socket.
 *
 * The socket is deliberately constructed through the global `WebSocket`, so the
 * network boundary (and only that boundary) can be substituted in tests.
 */
export class RealtimeSocket {
  private socket: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private staleTimer: ReturnType<typeof setTimeout> | null = null;
  private attempt = 0;
  private closedByUser = false;
  private lifecycleAttached = false;
  private readonly policy: ReconnectPolicy;
  private readonly staleAfterMs: number;
  private readonly random: () => number;
  private readonly handlers: SocketHandlers;
  private readonly options: RealtimeOptions;

  /** Bound so it can be added to and removed from `window` / `document`. */
  private readonly onOnline = (): void => {
    this.retryNow();
  };

  /** Bound so it can be added to and removed from `window` / `document`. */
  private readonly onVisibilityChange = (): void => {
    if (typeof document !== "undefined" && document.hidden) return;
    this.retryNow();
  };

  constructor(handlers: SocketHandlers, options: RealtimeOptions) {
    this.handlers = handlers;
    this.options = options;
    this.policy = { ...DEFAULT_RECONNECT_POLICY, ...options.policy };
    this.staleAfterMs = options.staleAfterMs ?? DEFAULT_STALE_AFTER_MS;
    this.random = options.random ?? Math.random;
  }

  /** Open the socket, or the first reconnect attempt. Idempotent. */
  connect(): void {
    if (this.socket !== null) return;
    this.closedByUser = false;

    if (this.token() === null) {
      // Not signed in: stay idle rather than opening a socket the server will
      // refuse. The caller retries once a session exists.
      this.handlers.onStatus("idle");
      return;
    }

    this.open();
  }

  /** Close the socket and stop reconnecting. */
  close(): void {
    this.closedByUser = true;
    this.clearReconnect();
    this.clearStaleness();
    this.detachLifecycle();

    const socket = this.socket;
    this.socket = null;
    socket?.close();

    this.handlers.onStatus("closed");
  }

  /**
   * Send one client event.
   *
   * Returns `false` when the socket is not open, so the caller can mark the
   * optimistic Message failed and offer a retry instead of losing it silently.
   */
  send(event: ClientEvent): boolean {
    if (this.socket === null || this.socket.readyState !== READY_STATE_OPEN) {
      return false;
    }

    const envelope: ClientEnvelope = { v: PROTOCOL_VERSION, e: event };
    this.socket.send(JSON.stringify(envelope));
    return true;
  }

  /** The token for this attempt, resolved fresh so a refresh is picked up. */
  private token(): string | null {
    return this.options.token();
  }

  private open(): void {
    const token = this.token();
    if (token === null) {
      this.handlers.onStatus("idle");
      return;
    }

    this.attachLifecycle();
    this.handlers.onStatus(this.attempt === 0 ? "connecting" : "reconnecting");

    const socket = new WebSocket(realtimeUrl(token));
    this.socket = socket;

    socket.onopen = () => {
      // Only this socket may reset the attempt counter; a late event from a
      // superseded socket must not corrupt the backoff of the live one.
      if (this.socket !== socket) return;
      this.attempt = 0;
      this.handlers.onStatus("open");
      this.armStaleness();
    };

    socket.onmessage = (event: MessageEvent) => {
      if (this.socket !== socket) return;
      // Any frame proves the link is alive, so the watchdog is re-armed before
      // parsing: a malformed frame still means the transport works.
      this.armStaleness();

      if (typeof event.data !== "string") return;
      const envelope = parseServerEnvelope(event.data);
      if (envelope !== null) this.handlers.onEnvelope(envelope);
    };

    // A failing socket always emits `onclose` afterwards, which schedules the
    // reconnect, so no separate `onerror` handling is needed. A superseded socket
    // closing late is ignored outright: otherwise it would schedule a reconnect on
    // top of the healthy socket that replaced it.
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.clearStaleness();
      if (this.closedByUser) return;
      this.scheduleReconnect();
    };
  }

  private scheduleReconnect(): void {
    this.clearReconnect();
    this.handlers.onStatus("reconnecting");

    const delay = this.nextDelay();
    this.attempt += 1;

    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (this.closedByUser) return;
      if (this.token() === null) {
        this.handlers.onStatus("idle");
        return;
      }
      this.open();
    }, delay);
  }

  /** The next backoff delay for the current attempt. */
  private nextDelay(): number {
    return reconnectDelay(this.policy, this.attempt, this.random);
  }

  /**
   * Retry at once, abandoning the backoff.
   *
   * Driven by `online` and by the tab becoming visible: the platform has told us
   * the reason we were waiting is gone, so waiting longer only delays recovery.
   */
  private retryNow(): void {
    if (this.closedByUser) return;
    if (this.socket !== null) return; // a connection is already live or in flight

    this.clearReconnect();
    this.attempt = 0;

    if (this.token() === null) {
      this.handlers.onStatus("idle");
      return;
    }

    this.open();
  }

  /** Start (or restart) the staleness watchdog for the live socket. */
  private armStaleness(): void {
    this.clearStaleness();
    if (this.staleAfterMs <= 0) return;

    this.staleTimer = setTimeout(() => {
      this.staleTimer = null;
      this.reconnectAfterStall();
    }, this.staleAfterMs);
  }

  /** A half-open link: tear the socket down and let the reconnect path run. */
  private reconnectAfterStall(): void {
    const socket = this.socket;
    if (socket === null) return;
    // Closing triggers `onclose`, which schedules the reconnect with backoff.
    socket.close();
  }

  private clearReconnect(): void {
    if (this.reconnectTimer === null) return;
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
  }

  private clearStaleness(): void {
    if (this.staleTimer === null) return;
    clearTimeout(this.staleTimer);
    this.staleTimer = null;
  }

  private attachLifecycle(): void {
    if (this.lifecycleAttached) return;

    if (
      typeof window !== "undefined" &&
      typeof window.addEventListener === "function"
    ) {
      window.addEventListener("online", this.onOnline);
    }
    if (
      typeof document !== "undefined" &&
      typeof document.addEventListener === "function"
    ) {
      document.addEventListener("visibilitychange", this.onVisibilityChange);
    }

    this.lifecycleAttached = true;
  }

  private detachLifecycle(): void {
    if (!this.lifecycleAttached) return;

    if (
      typeof window !== "undefined" &&
      typeof window.removeEventListener === "function"
    ) {
      window.removeEventListener("online", this.onOnline);
    }
    if (
      typeof document !== "undefined" &&
      typeof document.removeEventListener === "function"
    ) {
      document.removeEventListener("visibilitychange", this.onVisibilityChange);
    }

    this.lifecycleAttached = false;
  }
}
