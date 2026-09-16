import { defineStore } from "pinia";
import { ref } from "vue";
import { RealtimeSocket, type SocketStatus } from "../api/ws";
import type { ClientEvent } from "../generated/ClientEvent";
import type { ResyncReason } from "../generated/ResyncReason";
import type { ServerEnvelope } from "../generated/ServerEnvelope";
import type { ServerEvent } from "../generated/ServerEvent";
import { useAuthStore } from "./auth";

/**
 * Compile-time exhaustiveness check for a discriminated union.
 *
 * Adding a new `ServerEvent` variant stops its tag being `never` here, so the
 * build fails until the new case is handled. At runtime this is a no-op: an
 * unrecognised variant from a newer server is ignored rather than thrown, which
 * is what makes additive variants backward compatible.
 */
function assertExhaustive(_variant: never): void {}

/**
 * Connection state, the socket's send handle, and the events the rest of the app
 * subscribes to.
 *
 * The store owns the socket lifecycle but not the transport: `RealtimeSocket`
 * talks to the global `WebSocket`, so tests can substitute that boundary. Chat
 * state does not live here — this store forwards the chat variants to whoever
 * subscribed through {@link useRealtimeStore}'s `onChatEvent`, so `chat` depends
 * on `realtime` and never the other way round.
 *
 * # Gap detection (ADR-0003)
 *
 * `s` is the **connection** sequence. The store keeps the highest one it has
 * applied, so an envelope whose `s` skips ahead means the wire dropped something.
 * That is not silently ignored: the store sends the `Resume` handshake to ask the
 * connection layer to replay, and fires {@link onResync} so the Conversation layer
 * repairs from its own cursor. Both are needed — replay is at-least-once and may
 * be unavailable, while the per-Conversation repair is exact.
 *
 * # Wake-up handshake
 *
 * Every time a socket opens, the store sends `Resume` naming the highest `s` it
 * ever consumed. On a reconnect that position belongs to a dead connection, so
 * the server answers `unavailable` — the explicit signal that drives the repair.
 * A first connection sends `0` and is answered `fresh`.
 */
export const useRealtimeStore = defineStore("realtime", () => {
  const status = ref<SocketStatus>("idle");
  const sequence = ref<number | null>(null);
  const serverTimeMs = ref<number | null>(null);
  const receivedAtMs = ref<number | null>(null);
  /** The server's id for the live connection, from its heartbeats. */
  const connectionId = ref<number | null>(null);

  let socket: RealtimeSocket | null = null;
  const listeners = new Set<(event: ServerEvent) => void>();
  const resyncListeners = new Set<(reason: ResyncReason) => void>();

  /** Highest `s` applied on the current connection; reset when a new one opens. */
  let lastSequence: number | null = null;
  /** Highest `s` ever applied, across connections; named in the handshake. */
  let consumedSequence: number | null = null;
  /** Whether the wake-up handshake has been sent on the current connection. */
  let handshaken = false;
  /** Whether a socket has already opened once, so a reopen is a reconnection. */
  let hasOpenedBefore = false;

  function notifyResync(reason: ResyncReason): void {
    for (const listener of resyncListeners) listener(reason);
  }

  function applyEnvelope(envelope: ServerEnvelope): void {
    receivedAtMs.value = Date.now();

    const previous = lastSequence;
    // The position consumed *before* this envelope, which is what the wake-up
    // handshake names: the first heartbeat of a connection must not hand the
    // server a position it has not actually consumed yet.
    const consumedBefore = consumedSequence ?? 0;
    // A gap is the wire skipping ahead. Replayed duplicates arrive *below* the
    // high-water mark and must not be mistaken for one.
    const gapFrom =
      previous !== null && envelope.s > previous + 1 ? previous : null;

    lastSequence =
      previous === null ? envelope.s : Math.max(previous, envelope.s);
    sequence.value = lastSequence;
    consumedSequence = lastSequence;

    const event = envelope.e;
    switch (event.t) {
      case "Ping":
        serverTimeMs.value = event.d.time_ms;
        connectionId.value = event.d.connection_id;
        // The opening heartbeat is the connection's identity: answer it with the
        // wake-up handshake. The position named is from a *previous* connection
        // (or zero on a first connect), so it carries no connection id — a
        // reconnect must be answered `unavailable`, never mistaken for a caught-up
        // live socket.
        if (!handshaken) {
          handshaken = true;
          socket?.send({
            t: "Resume",
            d: { last_seq: consumedBefore, connection_id: null },
          });

          // A socket that never delivered an envelope cannot name a non-zero
          // position, yet the client may still hold REST-loaded history: repair on
          // reopen so that case is not missed.
          if (hasOpenedBefore && consumedBefore === 0) {
            notifyResync("unavailable");
          }
          hasOpenedBefore = true;
        }
        break;
      case "Resync":
        // The server's explicit answer to our handshake. `unavailable` means it
        // cannot fill the connection-level hole, so the Conversations must be
        // repaired from their cursors. `fresh` and `replayed` need no repair.
        if (event.d.reason === "unavailable") notifyResync(event.d.reason);
        break;
      case "MessageAck":
      case "NewMessage":
      case "MessageRejected":
      case "ConversationCreated":
      case "SyncState":
        // `SyncState` is the Device's persisted Sync Cursors, pushed once per
        // connection. It is chat state, not connection state — this store only
        // forwards it, and the chat store decides how to resume from it.
        for (const listener of listeners) listener(event);
        break;
      default:
        assertExhaustive(event);
    }

    if (gapFrom !== null) {
      // Ask the connection layer to replay the hole (it may still be buffered)...
      socket?.send({
        t: "Resume",
        d: { last_seq: gapFrom, connection_id: connectionId.value },
      });
      // ...and repair the Conversation layer regardless: the replay is
      // at-least-once, and an evicted position cannot be answered with data.
      notifyResync("unavailable");
    }
  }

  /**
   * Subscribe to the chat events the socket carries. Returns an unsubscribe.
   */
  function onChatEvent(listener: (event: ServerEvent) => void): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  }

  /**
   * Subscribe to "repair needed" signals. Returns an unsubscribe.
   *
   * Fired after a reconnect the server cannot resume, after a connection-level
   * gap, and whenever the server says the position is `unavailable`. The
   * subscriber pulls every Conversation forward from its own cursor.
   */
  function onResync(listener: (reason: ResyncReason) => void): () => void {
    resyncListeners.add(listener);
    return () => resyncListeners.delete(listener);
  }

  /** Open the realtime channel, reusing the socket if it already exists. */
  function connect(): void {
    if (socket === null) {
      socket = new RealtimeSocket(
        {
          onEnvelope: applyEnvelope,
          onStatus: (next) => {
            if (next === "connecting" || next === "reconnecting") {
              // The connection sequence is per connection: a new socket starts
              // its own, so the high-water mark and connection id are forgotten.
              // The wake-up handshake is re-sent once the opening heartbeat names
              // the new connection.
              lastSequence = null;
              handshaken = false;
              connectionId.value = null;
            }

            status.value = next;
          },
        },
        // Resolved on every attempt, so a refreshed access token is picked up.
        { token: () => useAuthStore().accessToken },
      );
    }
    socket.connect();
  }

  /** Close the realtime channel and stop reconnecting. */
  function disconnect(): void {
    socket?.close();
    socket = null;
    // The consumed position survives an explicit disconnect: a later reconnect
    // still repairs, so Messages sent while the user was disconnected arrive.
    lastSequence = null;
    handshaken = false;
    connectionId.value = null;
  }

  /** Send one client event; `false` when the socket is not open. */
  function send(event: ClientEvent): boolean {
    return socket?.send(event) ?? false;
  }

  return {
    status,
    sequence,
    serverTimeMs,
    receivedAtMs,
    connectionId,
    connect,
    disconnect,
    send,
    onChatEvent,
    onResync,
  };
});
