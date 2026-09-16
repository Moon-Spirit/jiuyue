/**
 * Typed WebSocket client for the jiuyue realtime channel.
 *
 * The only contract types this module uses are the generated ones under
 * `src/generated/` — it never re-declares the wire shape. The generated file is
 * produced by `scripts/gen-contract.ps1` from the Rust contract crate and is
 * guarded against drift in CI.
 *
 * Reconnection is built in, not bolted on: reloading a reverse proxy (Caddy)
 * forcibly drops every WebSocket, so the client must re-establish the connection
 * on its own.
 */

import type { ServerEnvelope } from "../generated/ServerEnvelope";

/** Lifecycle of the realtime socket as the UI sees it. */
export type SocketStatus =
  "idle" | "connecting" | "open" | "reconnecting" | "closed";

/** Backoff policy for automatic reconnection. */
export interface ReconnectPolicy {
  readonly initialDelayMs: number;
  readonly maxDelayMs: number;
  readonly factor: number;
}

/** Callbacks the socket drives. */
export interface SocketHandlers {
  readonly onEnvelope: (envelope: ServerEnvelope) => void;
  readonly onStatus: (status: SocketStatus) => void;
}

const DEFAULT_POLICY: ReconnectPolicy = {
  initialDelayMs: 500,
  maxDelayMs: 30_000,
  factor: 2,
};

/** Same-origin URL of the realtime endpoint, proxied to the backend in dev. */
export function realtimeUrl(): string {
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${window.location.host}/ws`;
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
  private attempt = 0;
  private closedByUser = false;
  private readonly policy: ReconnectPolicy;
  private readonly handlers: SocketHandlers;

  constructor(handlers: SocketHandlers, policy: Partial<ReconnectPolicy> = {}) {
    this.handlers = handlers;
    this.policy = { ...DEFAULT_POLICY, ...policy };
  }

  /** Open the socket, or the first reconnect attempt. Idempotent. */
  connect(): void {
    if (this.socket !== null) return;
    this.closedByUser = false;
    this.open();
  }

  /** Close the socket and stop reconnecting. */
  close(): void {
    this.closedByUser = true;
    this.clearReconnect();
    this.socket?.close();
    this.socket = null;
    this.handlers.onStatus("closed");
  }

  private open(): void {
    this.handlers.onStatus(this.attempt === 0 ? "connecting" : "reconnecting");

    const socket = new WebSocket(realtimeUrl());
    this.socket = socket;

    socket.onopen = () => {
      this.attempt = 0;
      this.handlers.onStatus("open");
    };

    socket.onmessage = (event: MessageEvent) => {
      if (typeof event.data !== "string") return;
      const envelope = parseServerEnvelope(event.data);
      if (envelope !== null) this.handlers.onEnvelope(envelope);
    };

    // A failing socket always emits `onclose` afterwards, which schedules the
    // reconnect, so no separate `onerror` handling is needed.
    socket.onclose = () => {
      this.socket = null;
      if (this.closedByUser) return;
      this.scheduleReconnect();
    };
  }

  private scheduleReconnect(): void {
    this.handlers.onStatus("reconnecting");

    const delay = Math.min(
      this.policy.maxDelayMs,
      this.policy.initialDelayMs * this.policy.factor ** this.attempt,
    );
    this.attempt += 1;

    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.open();
    }, delay);
  }

  private clearReconnect(): void {
    if (this.reconnectTimer === null) return;
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
  }
}
