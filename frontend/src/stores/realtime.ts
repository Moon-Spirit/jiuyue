import { defineStore } from "pinia";
import { ref } from "vue";
import { RealtimeSocket, type SocketStatus } from "../api/ws";
import type { ServerEnvelope } from "../generated/ServerEnvelope";

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
 * Connection state and the last heartbeat received over the realtime channel.
 *
 * The store owns the socket lifecycle but not the transport: `RealtimeSocket`
 * talks to the global `WebSocket`, so tests can substitute that boundary.
 */
export const useRealtimeStore = defineStore("realtime", () => {
  const status = ref<SocketStatus>("idle");
  const sequence = ref<number | null>(null);
  const serverTimeMs = ref<number | null>(null);
  const receivedAtMs = ref<number | null>(null);

  let socket: RealtimeSocket | null = null;

  function applyEnvelope(envelope: ServerEnvelope): void {
    sequence.value = envelope.s;
    receivedAtMs.value = Date.now();

    const event = envelope.e;
    switch (event.t) {
      case "Ping":
        serverTimeMs.value = event.d.time_ms;
        break;
      default:
        assertExhaustive(event.t);
    }
  }

  /** Open the realtime channel, reusing the socket if it already exists. */
  function connect(): void {
    if (socket === null) {
      socket = new RealtimeSocket({
        onEnvelope: applyEnvelope,
        onStatus: (next) => {
          status.value = next;
        },
      });
    }
    socket.connect();
  }

  /** Close the realtime channel and stop reconnecting. */
  function disconnect(): void {
    socket?.close();
    socket = null;
  }

  return { status, sequence, serverTimeMs, receivedAtMs, connect, disconnect };
});
