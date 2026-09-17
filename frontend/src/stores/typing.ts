import { defineStore } from "pinia";
import { ref } from "vue";
import type { Typing } from "../generated/Typing";
import type { TypingState } from "../generated/TypingState";
import { useRealtimeStore } from "./realtime";

/**
 * How long an indicator survives without a fresh signal, in milliseconds.
 *
 * This **mirrors the server's bound** (`jiuyue_realtime::TypingLimits::production`:
 * a 10 s TTL, a 5 s throttle). The client owns its own expiry because a sender that
 * closes mid-sentence never sends a stop, so an indicator that only cleared on a
 * stop would leak; the number must not be shorter than the server's refresh window
 * or a continuously typing Participant would blink.
 */
export const TYPING_TTL_MS = 10_000;

/**
 * How often a continuing typist re-signals, in milliseconds.
 *
 * Deliberately **below** the server's 5 s throttle: a refresh that landed inside
 * the server's window would be coalesced away, so the client sends often enough
 * that one always lands. This is the courtesy half of the throttle; the server's
 * window is the guarantee, and it holds even for a client that ignores this.
 */
export const TYPING_REFRESH_MS = 4_000;

/** How often the local expiry is re-evaluated while at least one indicator is live. */
const SWEEP_INTERVAL_MS = 1_000;

/**
 * Typing Indicators (CONTEXT.md: 正在输入) — who is composing, and for how long.
 *
 * # Two directions, one store
 *
 * - **Incoming** (`apply`): a per-Conversation map of User id to the instant their
 *   indicator expires, fed from the socket. It is **not** persisted anywhere and
 *   cannot be reconstructed from history: it exists only while a Participant is
 *   typing, which is exactly what CONTEXT.md means by "短暂信号，不持久化".
 * - **Outgoing** (`noteInput` / `stopTyping`): the client half of the throttle,
 *   which keeps a keystroke from spending a frame. The **server** owns the bound
 *   that matters (see `jiuyue_realtime::typing`); this side is politeness.
 *
 * # Expiry is local and bounded
 *
 * An entry is a deadline, not a flag. `typersFor` only reports entries whose
 * deadline is still in the future, and a one-second sweep re-evaluates `nowMs` so
 * the UI drops a stale indicator even if no event ever arrives. That is the
 * receiver's half of "a client that closes mid-sentence never sends a stop".
 *
 * # Ordering
 *
 * The server emits transitions for one (Conversation, Participant) in a single
 * total order and each connection is FIFO, so a `stopped` that arrives after a
 * `started` really is the later intent. This store therefore applies last-wins and
 * adds no version of its own; a stale stop cannot overtake a newer start because
 * the server never produces that order.
 */
export const useTypingStore = defineStore("typing", () => {
  /**
   * Live indicators, by Conversation then by User: the value is the instant the
   * entry expires, milliseconds since the Unix epoch.
   */
  const typers = ref<Record<string, Record<string, number>>>({});

  /**
   * The clock the expiry is evaluated against. Advanced by the sweep while an
   * indicator is live, and by every applied event, so a render reflects the latest
   * known time without reading `Date.now()` inside a computed.
   */
  const nowMs = ref(Date.now());

  /** Outgoing bookkeeping: when this client last signalled per Conversation. */
  const outgoing = new Map<string, number>();

  let timer: ReturnType<typeof setInterval> | null = null;

  function startSweep(): void {
    if (timer !== null) return;
    timer = setInterval(() => {
      nowMs.value = Date.now();
      prune();
    }, SWEEP_INTERVAL_MS);
  }

  function stopSweep(): void {
    if (timer === null) return;
    clearInterval(timer);
    timer = null;
  }

  /** Drop every entry whose deadline has passed, and stop sweeping when none are live. */
  function prune(): void {
    const now = nowMs.value;
    const next: Record<string, Record<string, number>> = {};
    let anyLive = false;

    for (const [conversationId, users] of Object.entries(typers.value)) {
      const live: Record<string, number> = {};
      for (const [userId, expiresAt] of Object.entries(users)) {
        if (expiresAt > now) {
          live[userId] = expiresAt;
          anyLive = true;
        }
      }
      if (Object.keys(live).length > 0) next[conversationId] = live;
    }

    typers.value = next;
    if (!anyLive) stopSweep();
  }

  /**
   * Adopt one Typing Indicator from the socket.
   *
   * `started` arms the deadline, `stopped` clears the entry immediately. Applying
   * is idempotent: a duplicate `started` only extends the deadline by at most the
   * TTL, and a duplicate `stopped` is a no-op on an absent entry.
   */
  function apply(typing: Typing): void {
    const now = Date.now();
    nowMs.value = now;

    const bucket = { ...(typers.value[typing.conversation_id] ?? {}) };

    if (typing.state === "started") {
      bucket[typing.user_id] = now + TYPING_TTL_MS;
      typers.value = { ...typers.value, [typing.conversation_id]: bucket };
      startSweep();
      return;
    }

    delete bucket[typing.user_id];
    const next = { ...typers.value };
    if (Object.keys(bucket).length === 0) delete next[typing.conversation_id];
    else next[typing.conversation_id] = bucket;
    typers.value = next;
    if (Object.keys(next).length === 0) stopSweep();
  }

  /** The User ids whose indicator is still live in a Conversation, in map order. */
  function typersFor(conversationId: string): string[] {
    const bucket = typers.value[conversationId];
    if (bucket === undefined) return [];

    const now = nowMs.value;
    return Object.entries(bucket)
      .filter(([, expiresAt]) => expiresAt > now)
      .map(([userId]) => userId);
  }

  /** Whether one User's indicator is live in a Conversation. */
  function isTyping(conversationId: string, userId: string): boolean {
    const bucket = typers.value[conversationId];
    if (bucket === undefined) return false;
    return (bucket[userId] ?? 0) > nowMs.value;
  }

  /**
   * Report a draft change from the composer, throttled on the client.
   *
   * A non-empty draft signals `started` once, then at most once per
   * {@link TYPING_REFRESH_MS}; an empty draft signals `stopped` once. Only the
   * transitions are sent, so holding a key down costs one frame, not one per
   * input event. The server coalesces whatever still gets through.
   */
  function noteInput(conversationId: string, hasText: boolean): void {
    if (conversationId === "") return;

    if (!hasText) {
      stopTyping(conversationId);
      return;
    }

    const now = Date.now();
    const lastSentAt = outgoing.get(conversationId);
    if (lastSentAt !== undefined && now - lastSentAt < TYPING_REFRESH_MS)
      return;

    outgoing.set(conversationId, now);
    send(conversationId, "started");
  }

  /** Stop signalling now — on send, on blur, or when leaving a Conversation. */
  function stopTyping(conversationId: string): void {
    if (!outgoing.has(conversationId)) return;
    outgoing.delete(conversationId);
    send(conversationId, "stopped");
  }

  function send(conversationId: string, state: TypingState): void {
    useRealtimeStore().send({
      t: "Typing",
      d: { conversation_id: conversationId, state },
    });
  }

  /** Forget one Conversation: incoming entries and outgoing bookkeeping. */
  function clearConversation(conversationId: string): void {
    const next = { ...typers.value };
    delete next[conversationId];
    typers.value = next;
    outgoing.delete(conversationId);
    if (Object.keys(next).length === 0) stopSweep();
  }

  /** Forget everything; used when the session ends so nothing leaks across accounts. */
  function clear(): void {
    typers.value = {};
    outgoing.clear();
    stopSweep();
  }

  // One subscription for the store's lifetime: the realtime store forwards only
  // typing changes here, and this store is what knows what a deadline means.
  useRealtimeStore().onTypingEvent(apply);

  return {
    typers,
    nowMs,
    apply,
    typersFor,
    isTyping,
    noteInput,
    stopTyping,
    clearConversation,
    clear,
  };
});
