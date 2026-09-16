import { defineStore } from "pinia";
import { computed, ref } from "vue";
import { ApiError, apiGetAuthed, apiPost } from "../api/client";
import type { ConversationList } from "../generated/ConversationList";
import type { ConversationSummary } from "../generated/ConversationSummary";
import type { MessageAck } from "../generated/MessageAck";
import type { MessageList } from "../generated/MessageList";
import type { MessageRejected } from "../generated/MessageRejected";
import type { MessageView } from "../generated/MessageView";
import type { NewMessage } from "../generated/NewMessage";
import type { ServerEvent } from "../generated/ServerEvent";
import { useAuthStore } from "./auth";
import { useRealtimeStore } from "./realtime";

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

/**
 * Compile-time exhaustiveness check for the event switch.
 *
 * Runtime no-op: an unknown variant from a newer server is ignored, which is what
 * makes additive server events backward compatible.
 */
function assertExhaustive(_variant: never): void {}

/** Order Messages by Sequence Number, keeping not-yet-sent ones last. */
function compareMessages(left: ChatMessage, right: ChatMessage): number {
  if (left.seq === null && right.seq === null) return 0;
  if (left.seq === null) return 1;
  if (right.seq === null) return -1;
  return left.seq - right.seq;
}

/** A fresh idempotency key. */
function newClientMsgId(): string {
  if (
    typeof crypto !== "undefined" &&
    typeof crypto.randomUUID === "function"
  ) {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

/** Project a stored Message onto the UI shape. */
function fromView(view: MessageView, state: DeliveryState): ChatMessage {
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

/** Replace the matching entry (by idempotency key, then by server id) or append. */
function upsert(list: ChatMessage[], message: ChatMessage): void {
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

/**
 * Conversations, Messages, and the send state machine.
 *
 * The store owns optimistic sending: `sendMessage` inserts a bubble immediately
 * and hands the payload to the realtime socket; the server's acknowledgement (or
 * rejection) reconciles it. It reads the session token from `auth`, and it
 * subscribes to the chat events `realtime` forwards — so the only dependency
 * direction is `chat → realtime, auth`.
 *
 * Network calls go through `api/client` and the socket through `realtime`, so
 * tests substitute `fetch` and the global `WebSocket` — never a store function.
 */
export const useChatStore = defineStore("chat", () => {
  const conversations = ref<ConversationSummary[]>([]);
  const activeConversationId = ref<string | null>(null);
  const messagesByConversation = ref<Record<string, ChatMessage[]>>({});
  const loadingConversations = ref(false);
  const loadingMessages = ref(false);
  const errorMessage = ref<string | null>(null);
  const notice = ref<string | null>(null);

  const activeConversation = computed<ConversationSummary | null>(
    () =>
      conversations.value.find(
        (conversation) => conversation.id === activeConversationId.value,
      ) ?? null,
  );

  /** The active Conversation's Messages, oldest first, pending ones last. */
  const messages = computed<ChatMessage[]>(() => {
    const id = activeConversationId.value;
    if (id === null) return [];
    return [...(messagesByConversation.value[id] ?? [])].sort(compareMessages);
  });

  /** The bucket for a Conversation, created on first use. */
  function bucket(conversationId: string): ChatMessage[] {
    const existing = messagesByConversation.value[conversationId];
    if (existing !== undefined) return existing;

    const created: ChatMessage[] = [];
    messagesByConversation.value[conversationId] = created;
    return created;
  }

  function accessToken(): string | null {
    return useAuthStore().accessToken;
  }

  function applyError(cause: unknown): void {
    if (cause instanceof ApiError) {
      errorMessage.value =
        cause.body?.error.message ?? `请求失败（HTTP ${cause.status}）`;
      return;
    }
    errorMessage.value = "无法连接服务器，请稍后重试";
  }

  function upsertConversation(conversation: ConversationSummary): void {
    const index = conversations.value.findIndex(
      (entry) => entry.id === conversation.id,
    );
    if (index >= 0) conversations.value.splice(index, 1, conversation);
    else conversations.value.unshift(conversation);
  }

  /** Load the caller's Conversations, newest first. */
  async function loadConversations(): Promise<void> {
    const token = accessToken();
    if (token === null) return;

    loadingConversations.value = true;
    errorMessage.value = null;
    try {
      const payload = await apiGetAuthed<ConversationList>(
        "/conversations",
        token,
      );
      conversations.value = payload.conversations;
    } catch (cause) {
      applyError(cause);
    } finally {
      loadingConversations.value = false;
    }
  }

  /**
   * Open (or reopen) the Direct Conversation with a peer, then focus it.
   *
   * Idempotent server-side, so calling this for an existing chat simply focuses
   * it rather than creating a duplicate.
   */
  async function startDirect(peerUsername: string): Promise<boolean> {
    const token = accessToken();
    if (token === null) return false;

    const peer = peerUsername.trim();
    if (peer === "") {
      errorMessage.value = "请输入对方用户名";
      return false;
    }

    errorMessage.value = null;
    try {
      const conversation = await apiPost<ConversationSummary>(
        "/conversations/direct",
        { peer_username: peer },
        token,
      );
      upsertConversation(conversation);
      activeConversationId.value = conversation.id;
      await loadMessages(conversation.id);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /** Focus a Conversation, loading its history the first time it is opened. */
  async function openConversation(conversationId: string): Promise<void> {
    activeConversationId.value = conversationId;
    if (messagesByConversation.value[conversationId] === undefined) {
      await loadMessages(conversationId);
    }
  }

  /** Fetch the recent history of a Conversation, keeping local pending bubbles. */
  async function loadMessages(conversationId: string): Promise<void> {
    const token = accessToken();
    if (token === null) return;

    loadingMessages.value = true;
    errorMessage.value = null;
    try {
      const payload = await apiGetAuthed<MessageList>(
        `/conversations/${conversationId}/messages`,
        token,
      );
      const list = bucket(conversationId);
      const pending = list.filter((message) => message.seq === null);

      list.length = 0;
      for (const view of payload.messages) list.push(fromView(view, "sent"));
      // Local not-yet-acknowledged bubbles survive a history reload.
      for (const message of pending) list.push(message);
    } catch (cause) {
      applyError(cause);
    } finally {
      loadingMessages.value = false;
    }
  }

  /** Mark a Message failed, wherever it lives. */
  function markFailed(clientMsgId: string, reason: string | null): void {
    for (const list of Object.values(messagesByConversation.value)) {
      const message = list.find((entry) => entry.clientMsgId === clientMsgId);
      if (message !== undefined) {
        message.state = "failed";
        message.failure = reason;
        return;
      }
    }
  }

  /** Hand one optimistic Message to the socket. */
  function dispatch(message: ChatMessage): boolean {
    const sent = useRealtimeStore().send({
      t: "SendMessage",
      d: {
        conversation_id: message.conversationId,
        client_msg_id: message.clientMsgId,
        body: message.body,
      },
    });

    if (!sent) markFailed(message.clientMsgId, "连接已断开");
    return sent;
  }

  /**
   * Insert an optimistic bubble and send it.
   *
   * Returns whether the Message reached the socket. A `false` means the bubble is
   * already marked failed and can be retried with {@link retry}.
   */
  function sendMessage(body: string): boolean {
    const conversationId = activeConversationId.value;
    const text = body.trim();
    if (conversationId === null || text === "") return false;

    const clientMsgId = newClientMsgId();
    const optimistic: ChatMessage = {
      id: `local:${clientMsgId}`,
      clientMsgId,
      conversationId,
      senderId: useAuthStore().user?.id ?? "",
      body: text,
      seq: null,
      createdAtMs: null,
      state: "sending",
      failure: null,
    };

    bucket(conversationId).push(optimistic);
    return dispatch(optimistic);
  }

  /**
   * Resend a failed Message under its original idempotency key.
   *
   * The key is never regenerated, so a Message the server already stored comes
   * back as the same Message instead of a second one.
   */
  function retry(clientMsgId: string): boolean {
    for (const list of Object.values(messagesByConversation.value)) {
      const message = list.find((entry) => entry.clientMsgId === clientMsgId);
      if (message === undefined) continue;

      message.state = "sending";
      message.failure = null;
      return dispatch(message);
    }

    return false;
  }

  /** The server stored a Message this client sent. */
  function applyAck(ack: MessageAck): void {
    upsert(bucket(ack.message.conversation_id), fromView(ack.message, "sent"));
  }

  /**
   * A Message was stored in a Conversation this client participates in.
   *
   * The sender's own connection receives this as well as its acknowledgement, so
   * the reconciliation is by Message id and is idempotent. A Conversation the
   * client does not know about yet is pulled in so the peer's first Message does
   * not arrive without its chat.
   */
  function applyNewMessage(payload: NewMessage): void {
    const view = payload.message;
    upsert(bucket(view.conversation_id), fromView(view, "sent"));

    const known = conversations.value.some(
      (conversation) => conversation.id === view.conversation_id,
    );
    if (!known) void loadConversations();
  }

  /** The server refused a Message; the bubble can be retried. */
  function applyRejection(rejection: MessageRejected): void {
    for (const list of Object.values(messagesByConversation.value)) {
      const message = list.find(
        (entry) => entry.clientMsgId === rejection.client_msg_id,
      );
      if (message !== undefined) {
        message.state = "failed";
        message.failure = rejection.message;
        return;
      }
    }

    notice.value = rejection.message;
  }

  /** A Conversation this client participates in was created. */
  function applyConversationCreated(conversation: ConversationSummary): void {
    upsertConversation(conversation);
  }

  /** Route one forwarded server event into chat state. */
  function handleChatEvent(event: ServerEvent): void {
    switch (event.t) {
      case "Ping":
        break;
      case "MessageAck":
        applyAck(event.d);
        break;
      case "NewMessage":
        applyNewMessage(event.d);
        break;
      case "MessageRejected":
        applyRejection(event.d);
        break;
      case "ConversationCreated":
        applyConversationCreated(event.d.conversation);
        break;
      default:
        assertExhaustive(event);
    }
  }

  // Subscribe once: the realtime store forwards only the chat events, and this
  // store is the thing that knows what to do with them.
  useRealtimeStore().onChatEvent(handleChatEvent);

  function clearNotice(): void {
    notice.value = null;
  }

  return {
    conversations,
    activeConversationId,
    activeConversation,
    messages,
    loadingConversations,
    loadingMessages,
    errorMessage,
    notice,
    loadConversations,
    startDirect,
    openConversation,
    sendMessage,
    retry,
    clearNotice,
  };
});
