import { defineStore } from "pinia";
import { ref } from "vue";
import { RealtimeSocket, type SocketStatus } from "../api/ws";
import type { ClientEvent } from "../generated/ClientEvent";
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
 */
export const useRealtimeStore = defineStore("realtime", () => {
  const status = ref<SocketStatus>("idle");
  const sequence = ref<number | null>(null);
  const serverTimeMs = ref<number | null>(null);
  const receivedAtMs = ref<number | null>(null);

  let socket: RealtimeSocket | null = null;
  const listeners = new Set<(event: ServerEvent) => void>();

  function applyEnvelope(envelope: ServerEnvelope): void {
    sequence.value = envelope.s;
    receivedAtMs.value = Date.now();

    const event = envelope.e;
    switch (event.t) {
      case "Ping":
        serverTimeMs.value = event.d.time_ms;
        break;
      case "MessageAck":
      case "NewMessage":
      case "MessageRejected":
      case "ConversationCreated":
        for (const listener of listeners) listener(event);
        break;
      default:
        assertExhaustive(event);
    }
  }

  /**
   * Subscribe to the chat events the socket carries. Returns an unsubscribe.
   */
  function onChatEvent(listener: (event: ServerEvent) => void): () => void {
    listeners.add(listener);
    return () => listeners.delete(listener);
  }

  /** Open the realtime channel, reusing the socket if it already exists. */
  function connect(): void {
    if (socket === null) {
      socket = new RealtimeSocket(
        {
          onEnvelope: applyEnvelope,
          onStatus: (next) => {
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
    connect,
    disconnect,
    send,
    onChatEvent,
  };
});
