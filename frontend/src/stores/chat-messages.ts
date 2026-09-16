import type { MessageView } from "../generated/MessageView";

/**
 * Pure Message shapes and helpers.
 *
 * Separated from the Pinia store so they can be read, tested and reused without
 * touching reactive state. Everything here is a value or a pure function: no
 * store, no network, no socket.
 */

/** Longest body the server accepts, mirrored so the limit is felt locally. */
export const MESSAGE_MAX_CHARS = 4000;

/** Where a Message sits in its delivery lifecycle. */
export type DeliveryState = "sending" | "sent" | "failed";

/**
 * One Message as the UI holds it.
 *
 * A Message exists before the server has seen it: an optimistic entry has a local
 * `id`, no `seq` and state `sending`. The `clientMsgId` is assigned once, at the
 * moment of the first send, and is **reused by every retry** — that is what lets
 * the server de-duplicate a resend instead of writing a second Message.
 */
export interface ChatMessage {
  /** Server ULID once stored; a local placeholder while sending. */
  id: string;
  /** The idempotency key (CONTEXT.md: Client Message ID). Stable across retries. */
  clientMsgId: string;
  conversationId: string;
  senderId: string;
  body: string;
  /** Server Sequence Number; `null` until acknowledged. */
  seq: number | null;
  /** Server-assigned time; `null` until acknowledged. */
  createdAtMs: number | null;
  state: DeliveryState;
  /** Why the send failed, when it did. */
  failure: string | null;
}

/** Order Messages by Sequence Number, keeping not-yet-sent ones last. */
export function compareMessages(left: ChatMessage, right: ChatMessage): number {
  if (left.seq === null && right.seq === null) return 0;
  if (left.seq === null) return 1;
  if (right.seq === null) return -1;
  return left.seq - right.seq;
}

/** The highest Sequence Number in a bucket, ignoring not-yet-acknowledged bubbles. */
export function maxSeq(list: readonly ChatMessage[]): number | null {
  let highest: number | null = null;
  for (const message of list) {
    if (message.seq === null) continue;
    highest = highest === null ? message.seq : Math.max(highest, message.seq);
  }
  return highest;
}

/**
 * Where a forward repair must start.
 *
 * The Sequence Numbers a client holds after a repair are contiguous, so a jump of
 * more than one is a hole — the Conversation-level gap the store must fill. The
 * repair starts from the last Sequence Number *before* the first hole, so the
 * forward walk re-delivers the missing Message and everything after it (which is
 * harmless: application is idempotent by Message ID). With no hole, the start is
 * simply the highest Sequence Number held, so only genuinely new Messages arrive.
 */
export function repairStartSeq(list: readonly ChatMessage[]): number | null {
  const seqs = list
    .filter((message) => message.seq !== null)
    .map((message) => message.seq as number)
    .sort((left, right) => left - right);

  for (let index = 1; index < seqs.length; index += 1) {
    const current = seqs[index];
    const previous = seqs[index - 1];
    if (current === undefined || previous === undefined) continue;
    if (current - previous > 1) return previous;
  }

  return seqs[seqs.length - 1] ?? null;
}

/** A fresh idempotency key. */
export function newClientMsgId(): string {
  if (
    typeof crypto !== "undefined" &&
    typeof crypto.randomUUID === "function"
  ) {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/** Project a stored Message onto the UI shape. */
export function fromView(view: MessageView, state: DeliveryState): ChatMessage {
  return {
    id: view.id,
    clientMsgId: view.client_msg_id,
    conversationId: view.conversation_id,
    senderId: view.sender_id,
    body: view.body,
    seq: view.seq,
    createdAtMs: view.created_at_ms,
    state,
    failure: null,
  };
}

/**
 * Replace the matching entry (by idempotency key, then by server id) or append.
 *
 * This is the single merge point, and it is what makes applying a duplicate
 * harmless: a Message is keyed by id, so re-delivering one the client already
 * holds replaces its entry rather than adding a second bubble.
 */
export function upsert(list: ChatMessage[], message: ChatMessage): void {
  const byClientId = list.findIndex(
    (entry) => entry.clientMsgId === message.clientMsgId,
  );
  if (byClientId >= 0) {
    list.splice(byClientId, 1, message);
    return;
  }

  const byServerId = list.findIndex((entry) => entry.id === message.id);
  if (byServerId >= 0) {
    list.splice(byServerId, 1, message);
    return;
  }

  list.push(message);
}
