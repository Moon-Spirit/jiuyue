import { defineStore } from "pinia";
import { toRaw } from "vue";
import { requestWsTicket } from "../lib/api/auth";
import { wsBase } from "../lib/apiConfig";
import { ApiError } from "../lib/api/client";
import {
  createConversation as apiCreateConversation,
  listConversations,
} from "../lib/api/messages";
import type { ConversationKind } from "../lib/api/messages";
import * as olm from "../lib/crypto/olm-lite";
import type {
  E2eeMsg,
  ErrorCode,
  ErrorPayload,
  Frame,
  MsgAck,
  MsgNew,
  MsgRecalled,
  ReadReceipt,
  SyncRes,
  Typing,
} from "../lib/protocol/frames";
import {
  parseFrame,
  ProtocolParseError,
  serializeFrame,
} from "../lib/protocol/frames";
import { useAuthStore } from "./auth";
import { useFriendsStore } from "./friends";

/**
 * Realtime chat store: owns the WebSocket lifecycle, optimistic sending with
 * `client_msg_id` idempotency, cursor-based offline sync, and the local
 * conversation/message cache persisted to localStorage.
 *
 * Wire contract (frozen, see lib/protocol/frames.ts):
 * - connect: POST /api/auth/ws-ticket 鈫?GET /ws?ticket=鈥?platform=web
 * - on open: sync.req{cursors:[{conversation_id,last_delivered_seq}]} for ALL
 *   known conversations; cursor = highest seq the client has seen locally.
 * - msg.ack resolves the matching optimistic entry (duplicate:true merges 鈥? *   never a second bubble); msg.new appends + bumps preview/unread/cursor;
 *   sync.res merges idempotently by message_id and (conversation_id, seq).
 * - error frames carry no client_msg_id, so on failure every in-flight
 *   "sending" entry flips to "failed"; retry() reuses the SAME client_msg_id,
 *   which server-side dedupe makes safe.
 *
 * Reconnect: exponential backoff 500ms路2^n capped at 15s with 卤20% jitter.
 * Staleness: a socket silent for STALE_AFTER_MS is closed locally and re-
 * connected through the normal path (server pings count as activity).
 */

export type WsStatus =
  "idle" | "connecting" | "open" | "reconnecting" | "closed";

/** M2: `read` = the peer's read receipt covered this message. */
export type MessageStatus = "sending" | "delivered" | "read" | "failed";

export interface ChatMessage {
  /** Idempotency key; empty for messages that arrived from the wire only. */
  clientMsgId: string;
  messageId: string | null;
  conversationId: number;
  /** Server-assigned per-conversation sequence; null while unacked. */
  seq: number | null;
  senderId: string;
  body: string;
  sentAt: string;
  mine: boolean;
  status: MessageStatus;
  /** Recall tombstone: render a placeholder instead of the (empty) body. */
  recalled: boolean;
  /** Quote metadata resolved server-side on msg.new (null when not a reply). */
  replyToMessageId: string | null;
  replyToSenderId: string | null;
  replyToBodyPreview: string | null;
  forwardedFromUsername: string | null;
  /**
   * M3 secret chats: true when the ciphertext could not be decrypted
   * (missing session / tampered payload); renders a localized placeholder.
   */
  undecryptable?: boolean;
}

export interface Conversation {
  conversationId: number;
  peerUserId: string;
  peerUsername: string;
  lastMessagePreview: string | null;
  lastActivityAt: string;
  unread: number;
  /** Highest seq marked as read (opening the conversation advances this). */
  lastSeenSeq: number;
  /** Highest seq seen locally — sent as the sync cursor on reconnect. */
  maxSeq: number;
  /**
   * Epoch-ms deadline while the peer's typing indicator is visible
   * (transient, never meaningful across reloads).
   */
  peerTypingUntil: number | null;
  /**
   * M3: "secret" conversations carry end-to-end encrypted traffic over
   * e2ee.msg frames; absent/undefined means "direct".
   */
  kind?: ConversationKind;
}

/** Composer-level reply context shown above the input until cancelled. */
export interface ReplyContext {
  messageId: string;
  senderId: string;
  bodyPreview: string;
}

const CONVOS_KEY = "jiuyue.convos";
const MSGS_KEY_PREFIX = "jiuyue.msgs.";

export const BACKOFF_BASE_MS = 500;
export const BACKOFF_CAP_MS = 15_000;
export const MAX_MESSAGES_PER_CONV = 500;
export const STALE_AFTER_MS = 90_000;
export const STALE_CHECK_INTERVAL_MS = 15_000;

// --- M2 message-experience constants -------------------------------------
/** How long a peer typing indicator stays visible without a refresh. */
export const TYPING_VISIBLE_MS = 6_000;
/** Minimum gap between outbound typing{start} frames. */
export const TYPING_SEND_THROTTLE_MS = 3_000;
/** Debounce for auto read.update frames after new messages arrive. */
export const READ_UPDATE_DEBOUNCE_MS = 300;
/** Sender-only recall window (mirrors the server-side RecallPolicy). */
export const RECALL_WINDOW_MS = 120_000;
/**
 * DECISION (forwarding): forward is a pure client-side compose of a NEW
 * normal message whose body is prefixed with this marker — zero wire or
 * schema change, visible to every receiver regardless of client.
 */
export const FORWARD_BODY_PREFIX = "[转发] ";

/**
 * Backoff schedule for reconnect attempt `n` (0-based): 500ms路2^n capped at
 * 15s, multiplied by a jitter factor in [0.8, 1.2), then clamped to the cap.
 * `rand` is injectable for deterministic tests.
 */
export function computeBackoffDelay(
  attempt: number,
  rand: () => number = Math.random,
): number {
  const raw = Math.min(BACKOFF_CAP_MS, BACKOFF_BASE_MS * 2 ** attempt);
  const jittered = raw * (0.8 + 0.4 * rand());
  return Math.round(Math.min(BACKOFF_CAP_MS, jittered));
}

function newClientMsgId(): string {
  if (
    typeof crypto !== "undefined" &&
    typeof crypto.randomUUID === "function"
  ) {
    return crypto.randomUUID();
  }
  // Deterministic-enough fallback for environments without web crypto.
  return `c-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function loadPersistedConversations(): Conversation[] {
  try {
    const raw = localStorage.getItem(CONVOS_KEY);
    if (raw === null) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    // Defensive shape upgrade: M2 adds peerTypingUntil; older caches lack
    // it (and M1 caches lack nothing else) — every field defaults.
    return parsed.filter(isRecord).map((c) => ({
      conversationId: Number(c["conversationId"]),
      peerUserId: String(c["peerUserId"] ?? ""),
      peerUsername: String(c["peerUsername"] ?? ""),
      lastMessagePreview:
        typeof c["lastMessagePreview"] === "string"
          ? c["lastMessagePreview"]
          : null,
      lastActivityAt: String(c["lastActivityAt"] ?? ""),
      unread: Number(c["unread"] ?? 0),
      lastSeenSeq: Number(c["lastSeenSeq"] ?? 0),
      maxSeq: Number(c["maxSeq"] ?? 0),
      peerTypingUntil:
        typeof c["peerTypingUntil"] === "number" ? c["peerTypingUntil"] : null,
      kind: c["kind"] === "secret" ? "secret" : "direct",
    }));
  } catch {
    // Corrupted cache degrades to an empty list; sync will repopulate.
    return [];
  }
}

function loadPersistedMessages(): Record<number, ChatMessage[]> {
  const out: Record<number, ChatMessage[]> = {};
  try {
    for (let i = 0; i < localStorage.length; i += 1) {
      const key = localStorage.key(i);
      if (key === null || !key.startsWith(MSGS_KEY_PREFIX)) continue;
      const conversationId = Number(key.slice(MSGS_KEY_PREFIX.length));
      if (!Number.isInteger(conversationId)) continue;
      const parsed: unknown = JSON.parse(localStorage.getItem(key) ?? "null");
      if (!Array.isArray(parsed)) continue;
      // Defensive shape upgrade: M2 adds recalled/reply/forward fields;
      // entries persisted by older builds load with neutral defaults.
      out[conversationId] = parsed.filter(isRecord).map((m) => ({
        clientMsgId: String(m["clientMsgId"] ?? ""),
        messageId: typeof m["messageId"] === "string" ? m["messageId"] : null,
        conversationId,
        seq: typeof m["seq"] === "number" ? m["seq"] : null,
        senderId: String(m["senderId"] ?? ""),
        body: String(m["body"] ?? ""),
        sentAt: String(m["sentAt"] ?? ""),
        mine: m["mine"] === true,
        status:
          m["status"] === "sending" || m["status"] === "failed"
            ? m["status"]
            : m["status"] === "read"
              ? "read"
              : "delivered",
        recalled: m["recalled"] === true,
        replyToMessageId:
          typeof m["replyToMessageId"] === "string"
            ? m["replyToMessageId"]
            : null,
        replyToSenderId:
          typeof m["replyToSenderId"] === "string"
            ? m["replyToSenderId"]
            : null,
        replyToBodyPreview:
          typeof m["replyToBodyPreview"] === "string"
            ? m["replyToBodyPreview"]
            : null,
        forwardedFromUsername:
          typeof m["forwardedFromUsername"] === "string"
            ? m["forwardedFromUsername"]
            : null,
        undecryptable: m["undecryptable"] === true,
      }));
    }
  } catch {
    // Corrupted cache degrades to empty history; sync will fill gaps.
  }
  return out;
}

/** Module-level connection handles 鈥?one live socket per tab. */
let socket: WebSocket | null = null;
let intentionalClose = false;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let stalenessTimer: ReturnType<typeof setInterval> | null = null;
let lastFrameAt = 0;

// --- M2 ephemeral state (never persisted) --------------------------------
/** Per-conversation expiry timers for peer typing indicators. */
const typingExpiryTimers = new Map<number, ReturnType<typeof setTimeout>>();
/** Epoch ms of the last outbound typing{start}; throttles repeats. */
let lastTypingSentAt = 0;
/** True between a sent typing{start} and its matching stop. */
let typingActive = false;
/** Per-conversation debounced read.update timers. */
const readUpdateTimers = new Map<number, ReturnType<typeof setTimeout>>();
/** Highest seq already reported via read.update per conversation. */
const lastSentReadSeq = new Map<number, number>();
/**
 * M3: this tab already uploaded its key bundle (the endpoint is an
 * idempotent upsert, so re-publishing would be harmless — the flag just
 * avoids redundant round-trips).
 */
let identityPublished = false;

/**
 * Deterministic local id for e2ee bubbles (djb2 over the ciphertext): the
 * server assigns no message_id/seq to opaque ciphertext, and sync replays
 * carry identical bytes, so hashing gives stable cross-replay dedupe.
 */
function pseudoE2eeId(ciphertext: string): string {
  let hash = 5381;
  for (let i = 0; i < ciphertext.length; i += 1) {
    hash = ((hash * 33) ^ ciphertext.charCodeAt(i)) >>> 0;
  }
  return `e2ee-${hash.toString(16).padStart(8, "0")}-${ciphertext.length}`;
}

function clearTypingExpiry(conversationId: number): void {
  const timer = typingExpiryTimers.get(conversationId);
  if (timer !== undefined) {
    clearTimeout(timer);
    typingExpiryTimers.delete(conversationId);
  }
}

function clearReadUpdateTimer(conversationId: number): void {
  const timer = readUpdateTimers.get(conversationId);
  if (timer !== undefined) {
    clearTimeout(timer);
    readUpdateTimers.delete(conversationId);
  }
}

function clearReconnectTimer(): void {
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
}

function stopStalenessWatch(): void {
  if (stalenessTimer !== null) {
    clearInterval(stalenessTimer);
    stalenessTimer = null;
  }
}

export const useWsStore = defineStore("ws", {
  state: () => ({
    status: "idle" as WsStatus,
    conversations: loadPersistedConversations(),
    messagesByConversation: loadPersistedMessages(),
    activeConversationId: null as number | null,
    /** Outbound frames queued in memory while the socket is not open. */
    queuedFrames: [] as Frame[],
    lastError: null as {
      code: ErrorCode;
      message: string;
      retryable: boolean;
    } | null,
    reconnectAttempt: 0,
    /** Composer reply context (quote block above the input); null = off. */
    replyContext: null as ReplyContext | null,
  }),

  getters: {
    isConnected: (state): boolean => state.status === "open",
    sortedConversations(state): Conversation[] {
      return [...state.conversations].sort((a, b) => {
        if (a.lastActivityAt !== b.lastActivityAt) {
          return a.lastActivityAt < b.lastActivityAt ? 1 : -1;
        }
        return b.conversationId - a.conversationId;
      });
    },
    activeConversation(state): Conversation | null {
      if (state.activeConversationId === null) return null;
      return (
        state.conversations.find(
          (c) => c.conversationId === state.activeConversationId,
        ) ?? null
      );
    },
    activeMessages(state): ChatMessage[] {
      if (state.activeConversationId === null) return [];
      return state.messagesByConversation[state.activeConversationId] ?? [];
    },
  },

  actions: {
    // ------------------------------------------------------------------
    // Connection lifecycle
    // ------------------------------------------------------------------

    async connect(): Promise<void> {
      if (this.status === "open" || this.status === "connecting") return;
      this.status = "connecting";
      clearReconnectTimer();

      const auth = useAuthStore();
      const token = await auth.ensureAccessToken();
      if (token === null) {
        this.status = "closed";
        return;
      }

      let ticket: string;
      try {
        ticket = (await requestWsTicket(token)).ticket;
      } catch {
        // Ticket endpoint unreachable 鈥?back off and try again.
        this.scheduleReconnect();
        return;
      }

      intentionalClose = false;
      const ws = new WebSocket(
        `${wsBase()}/ws?ticket=${encodeURIComponent(ticket)}&platform=web`,
      );
      socket = ws;

      ws.onopen = () => {
        if (socket !== ws) return;
        this.status = "open";
        this.reconnectAttempt = 0;
        lastFrameAt = Date.now();
        this.onSocketOpen();
        this.startStalenessWatch();
      };
      ws.onmessage = (ev: MessageEvent) => {
        if (socket !== ws) return;
        lastFrameAt = Date.now();
        this.handleFrameData(ev.data);
      };
      ws.onclose = () => {
        if (socket !== ws) return;
        socket = null;
        stopStalenessWatch();
        if (intentionalClose) {
          this.status = "closed";
          return;
        }
        this.status = "reconnecting";
        this.scheduleReconnect();
      };
      ws.onerror = () => {
        // The close event always follows an error; nothing to do here.
      };
    },

    /** User-intent disconnect: no reconnect scheduling afterwards. */
    disconnect(): void {
      intentionalClose = true;
      clearReconnectTimer();
      stopStalenessWatch();
      if (socket !== null) socket.close();
      socket = null;
      this.status = "closed";
    },

    /** Full teardown (logout / test hygiene): clears queues and flags too. */
    dispose(): void {
      this.disconnect();
      this.queuedFrames = [];
      this.lastError = null;
      this.reconnectAttempt = 0;
      this.replyContext = null;
      for (const conversation of this.conversations) {
        conversation.peerTypingUntil = null;
      }
      for (const timer of typingExpiryTimers.values()) clearTimeout(timer);
      typingExpiryTimers.clear();
      for (const timer of readUpdateTimers.values()) clearTimeout(timer);
      readUpdateTimers.clear();
      lastTypingSentAt = 0;
      typingActive = false;
      lastSentReadSeq.clear();
    },

    scheduleReconnect(): void {
      clearReconnectTimer();
      const delay = computeBackoffDelay(this.reconnectAttempt);
      this.reconnectAttempt += 1;
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        void this.connect();
      }, delay);
    },

    startStalenessWatch(): void {
      stopStalenessWatch();
      stalenessTimer = setInterval(() => {
        // Server pings arrive as frames and refresh lastFrameAt, so a dead
        // TCP socket (no pings) is detected and force-closed into reconnect.
        if (Date.now() - lastFrameAt > STALE_AFTER_MS) {
          socket?.close();
        }
      }, STALE_CHECK_INTERVAL_MS);
    },

    onSocketOpen(): void {
      const ws = socket;
      if (ws === null) return;
      // Bootstrap conversation discovery, then cursor-sync. Fresh sessions
      // (new browser/device) know no conversation ids locally; without the
      // server listing their sync.req would cover nothing (user story 7:
      // identical history on every device).
      const bootstrap = async () => {
        try {
          const auth = useAuthStore();
          const token = await auth.ensureAccessToken();
          if (token === null) return;
          const remote = await listConversations(token);
          for (const item of remote) {
            if (
              this.conversations.some(
                (c) => c.conversationId === item.conversation_id,
              )
            ) {
              continue;
            }
            this.conversations.push({
              conversationId: item.conversation_id,
              peerUserId: item.peer?.user_id ?? "",
              peerUsername: item.peer?.username ?? "",
              lastMessagePreview: null,
              lastActivityAt: "",
              // Device-local unseen count: sync arrivals increment it; the
              // server-side approximation would double-count after bootstrap.
              unread: 0,
              // Server cursors describe what the USER has received elsewhere;
              // THIS device's cache starts empty, so sync pulls full history
              // (user story 7) instead of trusting last_seq as local truth.
              lastSeenSeq: 0,
              maxSeq: 0,
              peerTypingUntil: null,
              kind: item.kind === "secret" ? "secret" : "direct",
            });
          }
          this.persistConversations();
        } catch (error) {
          // Bootstrap is best-effort: local-only sync still works for
          // sessions that already know their conversations.
          console.warn("[ws] conversation bootstrap failed", error);
        }
        if (socket !== ws || ws.readyState !== 1) return; // 1 = OPEN (no WebSocket global under mocked envs)
        // Cursor sync first: gap-fill before anything else flows.
        const syncReq: Frame = {
          v: 1,
          t: "sync.req",
          d: {
            cursors: this.conversations.map((c) => ({
              conversation_id: c.conversationId,
              last_delivered_seq: c.maxSeq,
            })),
          },
        };
        ws.send(JSON.stringify(serializeFrame(syncReq)));
        // Then flush everything queued while offline.
        const stillQueued: Frame[] = [];
        for (const frame of this.queuedFrames) {
          if (this.transmitFrame(frame) !== "sent") stillQueued.push(frame);
        }
        this.queuedFrames = stillQueued;
      };
      void bootstrap();
    },

    // ------------------------------------------------------------------
    // Inbound frame handling
    // ------------------------------------------------------------------

    handleFrameData(data: unknown): void {
      if (typeof data !== "string") return;
      let frame: Frame;
      try {
        frame = parseFrame(JSON.parse(data));
      } catch (error) {
        // Malformed wire garbage must not kill the stream; log and move on.
        if (error instanceof ProtocolParseError) {
          console.warn(`[ws] dropping malformed frame: ${error.message}`);
          return;
        }
        throw error;
      }
      switch (frame.t) {
        case "msg.ack":
          this.handleAck(frame.d);
          break;
        case "msg.new":
          this.ingestMessage(frame.d);
          break;
        case "sync.res":
          this.handleSyncRes(frame.d);
          break;
        case "read.receipt":
          this.handleReadReceipt(frame.d);
          break;
        case "typing":
          this.handleTypingNotice(frame.d);
          break;
        case "msg.recalled":
          this.handleMsgRecalled(frame.d);
          break;
        case "e2ee.msg":
          this.handleE2eeMsg(frame.d);
          break;
        case "friend.requested":
          // Friends store instantiated lazily inside the handler (same
          // store-to-store pattern as useAuthStore) to avoid circular init.
          useFriendsStore().onFriendRequested(frame.d);
          break;
        case "friend.accepted":
          useFriendsStore().onFriendAccepted(frame.d);
          break;
        case "error":
          this.handleErrorFrame(frame.d);
          break;
        default:
          // auth.ticket.* / sync.req / msg.send / read.update / typing (C→S
          // shape) / msg.recall never come inbound; unknown types already
          // degrade to error frames inside parseFrame.
          break;
      }
    },

    handleAck(ack: MsgAck): void {
      for (const conversation of this.conversations) {
        const messages =
          this.messagesByConversation[conversation.conversationId];
        if (messages === undefined) continue;
        const entry = messages.find((m) => m.clientMsgId === ack.client_msg_id);
        if (entry === undefined) continue;

        // Duplicate ack for an already-resolved entry: merge, never duplicate.
        if (
          entry.status === "delivered" &&
          entry.messageId === ack.message_id
        ) {
          return;
        }
        entry.messageId = ack.message_id;
        entry.seq = ack.seq;
        entry.status = "delivered";

        const isNewest = ack.seq > conversation.maxSeq;
        conversation.maxSeq = Math.max(conversation.maxSeq, ack.seq);
        if (isNewest) {
          conversation.lastActivityAt = entry.sentAt;
          conversation.lastMessagePreview = entry.body;
        }
        if (this.activeConversationId === conversation.conversationId) {
          conversation.lastSeenSeq = conversation.maxSeq;
          this.scheduleReadUpdate(conversation.conversationId);
        }
        this.persistConversations();
        this.persistMessages(conversation.conversationId);
        return;
      }
      // Unknown client_msg_id (e.g. pending entry lost): nothing to attach
      // the ack to 鈥?the message body would be unrecoverable, so ignore.
    },

    ingestMessage(m: MsgNew): void {
      let conversation = this.conversations.find(
        (c) => c.conversationId === m.conversation_id,
      );
      if (conversation === undefined) {
        // Sync can surface conversations this client has never opened.
        conversation = {
          conversationId: m.conversation_id,
          peerUserId: "",
          peerUsername: "",
          lastMessagePreview: null,
          lastActivityAt: "",
          unread: 0,
          lastSeenSeq: 0,
          maxSeq: 0,
          peerTypingUntil: null,
          kind: "direct",
        };
        this.conversations.push(conversation);
      }

      const mine = m.sender_id === this.mySenderId();
      const messages = this.messagesByConversation[m.conversation_id] ?? [];

      // Idempotent re-delivery: same message_id or same (conv, seq) — skip.
      const duplicate = messages.some(
        (msg) =>
          msg.messageId === m.message_id ||
          (msg.seq !== null && msg.seq === m.seq),
      );
      if (duplicate) return;

      // Tombstone contract: a recalled entry never carries body content.
      const body = m.recalled === true ? "" : m.body;

      // Lost-ack recovery: our own unacked optimistic bubble with identical
      // body gets adopted instead of spawning a second entry.
      if (mine && m.recalled !== true) {
        const orphan = messages.find(
          (msg) =>
            msg.seq === null &&
            msg.mine &&
            msg.body === m.body &&
            msg.status !== "failed",
        );
        if (orphan !== undefined) {
          orphan.messageId = m.message_id;
          orphan.seq = m.seq;
          orphan.status = "delivered";
          this.bumpConversationForIncoming(conversation, m, true);
          this.persistConversations();
          this.persistMessages(m.conversation_id);
          return;
        }
      }

      messages.push({
        clientMsgId: "",
        messageId: m.message_id,
        conversationId: m.conversation_id,
        seq: m.seq,
        senderId: m.sender_id,
        body,
        sentAt: m.sent_at,
        mine,
        status: "delivered",
        recalled: m.recalled === true,
        replyToMessageId: m.reply_to_message_id ?? null,
        replyToSenderId: m.reply_to_sender_id ?? null,
        replyToBodyPreview: m.reply_to_body_preview ?? null,
        forwardedFromUsername: m.forwarded_from_username ?? null,
      });
      this.sortMessages(messages);
      this.messagesByConversation[m.conversation_id] = messages;

      // A fresh incoming message from the peer proves they stopped typing.
      if (!mine) this.clearPeerTyping(m.conversation_id);

      this.bumpConversationForIncoming(conversation, m, mine);
      this.persistConversations();
      this.persistMessages(m.conversation_id);
    },

    bumpConversationForIncoming(
      conversation: Conversation,
      m: MsgNew,
      mine: boolean,
    ): void {
      const isNewest = m.seq > conversation.maxSeq;
      conversation.maxSeq = Math.max(conversation.maxSeq, m.seq);
      if (isNewest) {
        conversation.lastMessagePreview = m.body;
        conversation.lastActivityAt = m.sent_at;
      }
      if (this.activeConversationId === conversation.conversationId) {
        // Open conversation: immediately seen, unread stays zero.
        conversation.lastSeenSeq = conversation.maxSeq;
        this.scheduleReadUpdate(conversation.conversationId);
      } else if (!mine && m.seq > conversation.lastSeenSeq) {
        conversation.unread += 1;
      }
    },

    handleSyncRes(res: SyncRes): void {
      // Multiple sync.res batches may arrive until complete:true; each is
      // merged idempotently and kept sorted by seq.
      for (const m of res.messages) {
        this.ingestMessage(m);
      }
    },

    /**
     * Peer read receipt: upgrade MY messages with seq <= last_read_seq from
     * delivered → read (✓✓已读). Idempotent — already-read entries stay put.
     */
    handleReadReceipt(receipt: ReadReceipt): void {
      const messages = this.messagesByConversation[receipt.conversation_id];
      if (messages === undefined) return;
      let changed = false;
      for (const m of messages) {
        if (
          m.mine &&
          m.seq !== null &&
          m.seq <= receipt.last_read_seq &&
          m.status === "delivered"
        ) {
          m.status = "read";
          changed = true;
        }
      }
      if (changed) this.persistMessages(receipt.conversation_id);
    },

    /** Peer typing signal: show for TYPING_VISIBLE_MS, clear on stop. */
    handleTypingNotice(notice: Typing): void {
      // Own multi-device echo would be nonsense feedback.
      if (notice.user_id === this.mySenderId()) return;
      const conversation = this.conversations.find(
        (c) => c.conversationId === notice.conversation_id,
      );
      if (conversation === undefined) return;
      if (notice.state === "start") {
        conversation.peerTypingUntil = Date.now() + TYPING_VISIBLE_MS;
        clearTypingExpiry(conversation.conversationId);
        typingExpiryTimers.set(
          conversation.conversationId,
          setTimeout(() => {
            typingExpiryTimers.delete(conversation.conversationId);
            conversation.peerTypingUntil = null;
          }, TYPING_VISIBLE_MS),
        );
      } else {
        this.clearPeerTyping(conversation.conversationId);
      }
    },

    /** Clears the peer typing indicator immediately (stop / new message). */
    clearPeerTyping(conversationId: number): void {
      clearTypingExpiry(conversationId);
      const conversation = this.conversations.find(
        (c) => c.conversationId === conversationId,
      );
      if (conversation !== undefined) conversation.peerTypingUntil = null;
    },

    /** Recall broadcast: swap the bubble for a localized tombstone. */
    handleMsgRecalled(payload: MsgRecalled): void {
      const messages = this.messagesByConversation[payload.conversation_id];
      const entry = messages?.find((m) => m.messageId === payload.message_id);
      if (entry === undefined) return;
      entry.recalled = true;
      entry.body = "";
      this.persistMessages(payload.conversation_id);
    },

    // ------------------------------------------------------------------
    // M3 secret chats (end-to-end encrypted)
    // ------------------------------------------------------------------

    /**
     * Ensures the device can participate in `conversationId`'s secret chat:
     * publishes this device's key bundle once per tab, then fetches the
     * peer's bundle and installs the initiator session. Throws on failure
     * (callers decide whether to surface it).
     */
    async setupSecretConversation(
      conversationId: number,
      peerUsername: string,
    ): Promise<void> {
      const auth = useAuthStore();
      const token = await auth.ensureAccessToken();
      if (token === null) return;
      if (!identityPublished) {
        await olm.publishBundle(token);
        identityPublished = true;
      }
      await olm.fetchPeerBundleAndEstablish(
        peerUsername,
        conversationId,
        token,
      );
    },

    /** Inbound opaque ciphertext: decrypt → bubble, or failure placeholder. */
    handleE2eeMsg(payload: E2eeMsg): void {
      const conversation = this.conversations.find(
        (c) => c.conversationId === payload.conversation_id,
      );
      if (conversation === undefined || conversation.kind !== "secret") return;
      void this.decryptAndIngest(conversation, payload);
    },

    async decryptAndIngest(
      conversation: Conversation,
      payload: E2eeMsg,
    ): Promise<void> {
      let plaintext: string | null = null;
      let mine = false;
      try {
        const envelope = await olm.decryptEnvelope(
          conversation.conversationId,
          payload.ciphertext,
        );
        plaintext = envelope.plaintext;
        mine = envelope.senderIdentity === (await olm.ensureIdentity());
      } catch (error) {
        console.warn("[ws] e2ee decrypt failed", error);
      }
      if (plaintext !== null && !mine) {
        // Own multi-device echoes decrypt fine but must not bump unread.
        this.clearPeerTyping(conversation.conversationId);
      }
      this.appendE2eeBubble(
        conversation,
        pseudoE2eeId(payload.ciphertext),
        mine,
        plaintext,
      );
    },

    /**
     * Appends a decrypted e2ee bubble (or an undecryptable placeholder when
     * `plaintext` is null). e2ee traffic has no server seq — bubbles keep
     * seq:null and never touch sync cursors, only local display state.
     */
    appendE2eeBubble(
      conversation: Conversation,
      messageId: string,
      mine: boolean,
      plaintext: string | null,
    ): void {
      const messages =
        this.messagesByConversation[conversation.conversationId] ?? [];
      // Sync replays carry identical ciphertext bytes → identical id.
      if (messages.some((m) => m.messageId === messageId)) return;
      const sentAt = new Date().toISOString();
      messages.push({
        clientMsgId: "",
        messageId,
        conversationId: conversation.conversationId,
        seq: null,
        senderId: mine ? this.mySenderId() : conversation.peerUserId,
        body: plaintext ?? "",
        sentAt,
        mine,
        status: "delivered",
        recalled: false,
        replyToMessageId: null,
        replyToSenderId: null,
        replyToBodyPreview: null,
        forwardedFromUsername: null,
        undecryptable: plaintext === null,
      });
      this.sortMessages(messages);
      this.messagesByConversation[conversation.conversationId] = messages;

      if (!mine && plaintext !== null) {
        conversation.lastMessagePreview = plaintext;
        conversation.lastActivityAt = sentAt;
        if (this.activeConversationId !== conversation.conversationId) {
          conversation.unread += 1;
        }
      }
      this.persistConversations();
      this.persistMessages(conversation.conversationId);
    },

    /**
     * Encrypts and transmits one secret message as an e2ee.msg frame.
     * Offline sends are queued already-encrypted (counters stay consistent);
     * transport rejection or engine errors fail the optimistic bubble.
     */
    async transmitSecretMessage(
      message: ChatMessage,
      plaintext: string,
    ): Promise<void> {
      try {
        const { ciphertext, messageType } = await olm.encrypt(
          message.conversationId,
          plaintext,
        );
        const frame: Frame = {
          v: 1,
          t: "e2ee.msg",
          d: {
            conversation_id: message.conversationId,
            ciphertext,
            message_type: messageType,
          },
        };
        const outcome = this.transmitFrame(frame);
        if (outcome === "queued") this.queuedFrames.push(frame);
        else if (outcome === "rejected") message.status = "failed";
      } catch (error) {
        console.warn("[ws] e2ee encrypt failed", error);
        message.status = "failed";
      } finally {
        this.persistMessages(message.conversationId);
      }
    },

    handleErrorFrame(payload: ErrorPayload): void {
      // parseFrame degrades unknown frame types (e.g. server pings) to
      // error{code:"unknown_type"} 鈥?those are heartbeat noise, not failures.
      if (payload.code === "unknown_type") return;

      this.lastError = { ...payload };

      // Error payloads carry no client_msg_id, so conservatively fail every
      // in-flight send; retry() reuses the same client_msg_id (safe dedupe).
      for (const conversation of this.conversations) {
        const messages =
          this.messagesByConversation[conversation.conversationId];
        if (messages === undefined) continue;
        let changed = false;
        for (const m of messages) {
          if (m.status === "sending") {
            m.status = "failed";
            changed = true;
          }
        }
        if (changed) this.persistMessages(conversation.conversationId);
      }
    },

    // ------------------------------------------------------------------
    // Outbound
    // ------------------------------------------------------------------

    send(conversationId: number, body: string): ChatMessage | null {
      const trimmed = body.trim();
      if (trimmed.length === 0) return null;

      let conversation = this.conversations.find(
        (c) => c.conversationId === conversationId,
      );
      if (conversation === undefined) {
        conversation = {
          conversationId,
          peerUserId: "",
          peerUsername: "",
          lastMessagePreview: null,
          lastActivityAt: "",
          unread: 0,
          lastSeenSeq: 0,
          maxSeq: 0,
          peerTypingUntil: null,
          kind: "direct",
        };
        this.conversations.push(conversation);
      }

      const message: ChatMessage = {
        clientMsgId: newClientMsgId(),
        messageId: null,
        conversationId,
        seq: null,
        senderId: this.mySenderId(),
        body: trimmed,
        sentAt: new Date().toISOString(),
        mine: true,
        status: "sending",
        recalled: false,
        replyToMessageId: null,
        replyToSenderId: null,
        replyToBodyPreview: null,
        forwardedFromUsername: null,
      };
      const messages = this.messagesByConversation[conversationId] ?? [];
      messages.push(message);
      this.sortMessages(messages);
      this.messagesByConversation[conversationId] = messages;

      conversation.lastMessagePreview = trimmed;
      conversation.lastActivityAt = message.sentAt;

      // Attach (and consume) the composer reply context. Conditional spread:
      // a plain message must not carry a reply_to key at all.
      const replyTo = this.replyContext?.messageId;
      this.clearReplyContext();

      if (conversation.kind === "secret") {
        // M3 secret chat: the wire carries opaque ciphertext only; the
        // plaintext stays in the local bubble for display. send() stays
        // synchronous — encryption + transmission happen in the background
        // and flip the bubble to failed on error.
        void this.transmitSecretMessage(message, trimmed);
      } else {
        const frame: Frame = {
          v: 1,
          t: "msg.send",
          d: {
            conversation_id: conversationId,
            client_msg_id: message.clientMsgId,
            body: trimmed,
            ...(replyTo !== undefined && replyTo !== null
              ? { reply_to: replyTo }
              : {}),
          },
        };
        const outcome = this.transmitFrame(frame);
        if (outcome === "queued") {
          // Socket not open: hold in memory; flushed automatically on open.
          this.queuedFrames.push(frame);
        } else if (outcome === "rejected") {
          // Send rejection surfaces as an explicit failure on the message.
          message.status = "failed";
        }
      }

      this.persistConversations();
      this.persistMessages(conversationId);
      return message;
    },

    /** Resend a failed message reusing its original client_msg_id. */
    retry(clientMsgId: string): void {
      for (const conversation of this.conversations) {
        const messages =
          this.messagesByConversation[conversation.conversationId];
        if (messages === undefined) continue;
        const entry = messages.find(
          (m) => m.clientMsgId === clientMsgId && m.status === "failed",
        );
        if (entry === undefined) continue;

        entry.status = "sending";
        const owner = this.conversations.find(
          (c) => c.conversationId === entry.conversationId,
        );
        if (owner?.kind === "secret") {
          // Re-encrypt with fresh ratchet keys (the original ciphertext is
          // unrecoverable); the peer may render both copies — MVP tradeoff.
          void this.transmitSecretMessage(entry, entry.body);
        } else {
          const frame: Frame = {
            v: 1,
            t: "msg.send",
            d: {
              conversation_id: entry.conversationId,
              client_msg_id: entry.clientMsgId,
              body: entry.body,
            },
          };
          if (this.transmitFrame(frame) === "queued")
            this.queuedFrames.push(frame);
        }
        this.persistMessages(entry.conversationId);
        return;
      }
    },

    // ------------------------------------------------------------------
    // M2 outbound: typing / read receipts / recall / forward / reply
    // ------------------------------------------------------------------

    /**
     * Composer activity ping. Throttled to one frame per
     * TYPING_SEND_THROTTLE_MS; ephemeral frames are never queued offline.
     */
    notifyTypingStart(conversationId: number): void {
      const now = Date.now();
      if (now - lastTypingSentAt < TYPING_SEND_THROTTLE_MS) return;
      lastTypingSentAt = now;
      typingActive = true;
      this.transmitEphemeral({
        v: 1,
        t: "typing",
        d: { conversation_id: conversationId, state: "start" },
      });
    },

    /** Composer blur / send / clear: stop the peer's indicator promptly. */
    notifyTypingStop(conversationId: number): void {
      if (!typingActive) return;
      typingActive = false;
      lastTypingSentAt = 0;
      this.transmitEphemeral({
        v: 1,
        t: "typing",
        d: { conversation_id: conversationId, state: "stop" },
      });
    },

    /**
     * Debounced auto read-cursor report while the conversation is open and
     * the window is focused. Skipped silently when nothing new was read.
     */
    scheduleReadUpdate(conversationId: number): void {
      if (this.activeConversationId !== conversationId) return;
      if (
        typeof document !== "undefined" &&
        typeof document.hasFocus === "function" &&
        !document.hasFocus()
      ) {
        return;
      }
      clearReadUpdateTimer(conversationId);
      readUpdateTimers.set(
        conversationId,
        setTimeout(() => {
          readUpdateTimers.delete(conversationId);
          const conversation = this.conversations.find(
            (c) => c.conversationId === conversationId,
          );
          if (conversation === undefined) return;
          const alreadySent = lastSentReadSeq.get(conversationId) ?? 0;
          if (conversation.maxSeq <= alreadySent) return;
          lastSentReadSeq.set(conversationId, conversation.maxSeq);
          this.transmitEphemeral({
            v: 1,
            t: "read.update",
            d: {
              conversation_id: conversationId,
              last_read_seq: conversation.maxSeq,
            },
          });
        }, READ_UPDATE_DEBOUNCE_MS),
      );
    },

    /** Sets the composer reply context from a message bubble action. */
    setReplyContext(message: ChatMessage): void {
      if (message.messageId === null || message.recalled) return;
      this.replyContext = {
        messageId: message.messageId,
        senderId: message.senderId,
        bodyPreview: message.body.slice(0, 80),
      };
    },

    clearReplyContext(): void {
      this.replyContext = null;
    },

    /**
     * Forward = pure CLIENT-side compose of a NEW normal message with a
     * "[转发] " body prefix (see FORWARD_BODY_PREFIX). Zero wire/schema
     * change by decision; recalled sources have no content to forward.
     */
    forwardMessage(
      toConversationId: number,
      source: ChatMessage,
    ): ChatMessage | null {
      if (source.recalled || source.body.length === 0) return null;
      return this.send(
        toConversationId,
        `${FORWARD_BODY_PREFIX}${source.body}`,
      );
    },

    /**
     * Recall request for one of MY recent messages. No optimistic swap —
     * the authoritative msg.recalled broadcast (which includes the sender)
     * performs the tombstone transition everywhere.
     */
    recallMessage(conversationId: number, message: ChatMessage): void {
      if (message.messageId === null || !message.mine || message.recalled) {
        return;
      }
      this.transmitEphemeral({
        v: 1,
        t: "msg.recall",
        d: { conversation_id: conversationId, message_id: message.messageId },
      });
    },

    /**
     * Like transmitFrame but for EPHEMERAL signals (typing / read.update /
     * msg.recall): dropped when the socket is not open — they are pointless
     * once stale and must never sit in the reconnect queue.
     */
    transmitEphemeral(frame: Frame): void {
      void this.transmitFrame(frame);
    },

    /**
     * Try to push a frame now. `"queued"` = no open socket (caller queues);
     * `"rejected"` = the socket claimed open but send threw (caller fails
     * the message explicitly).
     */
    transmitFrame(frame: Frame): "sent" | "queued" | "rejected" {
      const ws = socket;
      if (ws === null || ws.readyState !== 1) return "queued";
      try {
        // toRaw: frames sourced from reactive state (queuedFrames) are Vue
        // proxies, which structuredClone rejects 鈥?serialize the raw target.
        ws.send(JSON.stringify(serializeFrame(toRaw(frame))));
        return "sent";
      } catch {
        return "rejected";
      }
    },

    // ------------------------------------------------------------------
    // Conversations / read state
    // ------------------------------------------------------------------

    async createOrOpenConversation(
      peerUsername: string,
      kind: ConversationKind = "direct",
    ): Promise<Conversation> {
      const auth = useAuthStore();
      const token = await auth.ensureAccessToken();
      if (token === null) {
        throw new ApiError(0, "network_error", "not signed in");
      }
      const result = await apiCreateConversation(token, peerUsername, kind);

      let conversation = this.conversations.find(
        (c) => c.conversationId === result.conversation_id,
      );
      if (conversation === undefined) {
        conversation = {
          conversationId: result.conversation_id,
          peerUserId: result.peer.user_id,
          peerUsername: result.peer.username,
          lastMessagePreview: null,
          lastActivityAt: new Date().toISOString(),
          unread: 0,
          lastSeenSeq: 0,
          maxSeq: 0,
          peerTypingUntil: null,
          kind: result.kind === "secret" ? "secret" : "direct",
        };
        this.conversations.push(conversation);
      } else {
        conversation.peerUserId = result.peer.user_id;
        conversation.peerUsername = result.peer.username;
        if (kind === "secret") conversation.kind = "secret";
      }
      this.persistConversations();
      // Establish the crypto session BEFORE the thread opens so the very
      // first message can be encrypted (errors propagate to the caller UI).
      if (
        conversation.kind === "secret" &&
        !olm.hasSession(conversation.conversationId)
      ) {
        await this.setupSecretConversation(
          conversation.conversationId,
          conversation.peerUsername,
        );
      }
      this.openConversation(result.conversation_id);
      return conversation;
    },

    /**
     * Open a conversation: clears unread, marks history as seen, reports the
     * read cursor, and re-arms (or clears) any stale peer typing indicator.
     */
    openConversation(conversationId: number): void {
      this.activeConversationId = conversationId;
      const conversation = this.conversations.find(
        (c) => c.conversationId === conversationId,
      );
      if (conversation === undefined) return;
      conversation.unread = 0;
      conversation.lastSeenSeq = conversation.maxSeq;
      // M3: lazily-discovered secret conversations (sync/bootstrap) set up
      // their crypto session in the background; failures are non-fatal here
      // because inbound e2ee.msg degrades to the undecryptable placeholder.
      if (conversation.kind === "secret" && !olm.hasSession(conversationId)) {
        void this.setupSecretConversation(
          conversationId,
          conversation.peerUsername,
        ).catch((error) => {
          console.warn("[ws] secret session setup failed", error);
        });
      }
      if (conversation.peerTypingUntil !== null) {
        if (conversation.peerTypingUntil <= Date.now()) {
          conversation.peerTypingUntil = null;
        } else {
          clearTypingExpiry(conversationId);
          typingExpiryTimers.set(
            conversationId,
            setTimeout(() => {
              typingExpiryTimers.delete(conversationId);
              conversation.peerTypingUntil = null;
            }, conversation.peerTypingUntil - Date.now()),
          );
        }
      }
      this.persistConversations();
      this.scheduleReadUpdate(conversationId);
    },

    closeConversation(): void {
      this.activeConversationId = null;
    },

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    mySenderId(): string {
      const auth = useAuthStore();
      const userId = auth.user?.userId;
      return typeof userId === "string" && userId.length > 0 ? userId : "";
    },

    sortMessages(messages: ChatMessage[]): void {
      // Unacked optimistic entries (seq=null) sort last; stable elsewhere.
      messages.sort((a, b) => {
        const aSeq = a.seq ?? Number.MAX_SAFE_INTEGER;
        const bSeq = b.seq ?? Number.MAX_SAFE_INTEGER;
        return aSeq - bSeq;
      });
    },

    persistConversations(): void {
      try {
        localStorage.setItem(CONVOS_KEY, JSON.stringify(this.conversations));
      } catch {
        // Best-effort persistence (private mode / quota).
      }
    },

    persistMessages(conversationId: number): void {
      try {
        const messages = this.messagesByConversation[conversationId] ?? [];
        localStorage.setItem(
          MSGS_KEY_PREFIX + String(conversationId),
          JSON.stringify(messages.slice(-MAX_MESSAGES_PER_CONV)),
        );
      } catch {
        // Best-effort persistence (private mode / quota).
      }
    },
  },
});
