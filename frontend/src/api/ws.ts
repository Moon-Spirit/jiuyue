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
 * on its own.
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
}

const DEFAULT_POLICY: ReconnectPolicy = {
  initialDelayMs: 500,
  maxDelayMs: 30_000,
  factor: 2,
};

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
  private attempt = 0;
  private closedByUser = false;
  private readonly policy: ReconnectPolicy;
  private readonly handlers: SocketHandlers;
  private readonly options: RealtimeOptions;

  constructor(handlers: SocketHandlers, options: RealtimeOptions) {
    this.handlers = handlers;
    this.options = options;
    this.policy = { ...DEFAULT_POLICY, ...options.policy };
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
    this.socket?.close();
    this.socket = null;
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

    this.handlers.onStatus(this.attempt === 0 ? "connecting" : "reconnecting");

    const socket = new WebSocket(realtimeUrl(token));
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
      if (this.token() === null) {
        this.handlers.onStatus("idle");
        return;
      }
      this.open();
    }, delay);
  }

  private clearReconnect(): void {
    if (this.reconnectTimer === null) return;
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
  }
}
