import { defineStore } from "pinia";
import { toRaw } from "vue";
import { requestWsTicket } from "../lib/api/auth";
import { ApiError } from "../lib/api/client";
import {
  createConversation as apiCreateConversation,
  listConversations,
} from "../lib/api/messages";
import type {
  ErrorCode,
  ErrorPayload,
  Frame,
  MsgAck,
  MsgNew,
  SyncRes,
} from "../lib/protocol/frames";
import {
  parseFrame,
  ProtocolParseError,
  serializeFrame,
} from "../lib/protocol/frames";
import { useAuthStore } from "./auth";

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

export type MessageStatus = "sending" | "delivered" | "failed";

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
  /** Highest seq seen locally 鈥?sent as the sync cursor on reconnect. */
  maxSeq: number;
}

const CONVOS_KEY = "jiuyue.convos";
const MSGS_KEY_PREFIX = "jiuyue.msgs.";

export const BACKOFF_BASE_MS = 500;
export const BACKOFF_CAP_MS = 15_000;
export const MAX_MESSAGES_PER_CONV = 500;
export const STALE_AFTER_MS = 90_000;
export const STALE_CHECK_INTERVAL_MS = 15_000;

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
            : "delivered",
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
        `/ws?ticket=${encodeURIComponent(ticket)}&platform=web`,
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
        case "error":
          this.handleErrorFrame(frame.d);
          break;
        default:
          // auth.ticket.* / sync.req / msg.send never come inbound; unknown
          // types already degrade to error frames inside parseFrame.
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
        };
        this.conversations.push(conversation);
      }

      const mine = m.sender_id === this.mySenderId();
      const messages = this.messagesByConversation[m.conversation_id] ?? [];

      // Idempotent re-delivery: same message_id or same (conv, seq) 鈫?skip.
      const duplicate = messages.some(
        (msg) =>
          msg.messageId === m.message_id ||
          (msg.seq !== null && msg.seq === m.seq),
      );
      if (duplicate) return;

      // Lost-ack recovery: our own unacked optimistic bubble with identical
      // body gets adopted instead of spawning a second entry.
      if (mine) {
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
        body: m.body,
        sentAt: m.sent_at,
        mine,
        status: "delivered",
      });
      this.sortMessages(messages);
      this.messagesByConversation[m.conversation_id] = messages;

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
      };
      const messages = this.messagesByConversation[conversationId] ?? [];
      messages.push(message);
      this.sortMessages(messages);
      this.messagesByConversation[conversationId] = messages;

      conversation.lastMessagePreview = trimmed;
      conversation.lastActivityAt = message.sentAt;

      const frame: Frame = {
        v: 1,
        t: "msg.send",
        d: {
          conversation_id: conversationId,
          client_msg_id: message.clientMsgId,
          body: trimmed,
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
        this.persistMessages(entry.conversationId);
        return;
      }
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
    ): Promise<Conversation> {
      const auth = useAuthStore();
      const token = await auth.ensureAccessToken();
      if (token === null) {
        throw new ApiError(0, "network_error", "not signed in");
      }
      const result = await apiCreateConversation(token, peerUsername);

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
        };
        this.conversations.push(conversation);
      } else {
        conversation.peerUserId = result.peer.user_id;
        conversation.peerUsername = result.peer.username;
      }
      this.persistConversations();
      this.openConversation(result.conversation_id);
      return conversation;
    },

    /** Open a conversation: clears unread and marks history as seen. */
    openConversation(conversationId: number): void {
      this.activeConversationId = conversationId;
      const conversation = this.conversations.find(
        (c) => c.conversationId === conversationId,
      );
      if (conversation === undefined) return;
      conversation.unread = 0;
      conversation.lastSeenSeq = conversation.maxSeq;
      this.persistConversations();
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
