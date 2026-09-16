import { defineStore } from "pinia";
import { computed, ref } from "vue";
import { ApiError, apiGetAuthed, apiPost } from "../api/client";
import type { ConversationList } from "../generated/ConversationList";
import type { ConversationSummary } from "../generated/ConversationSummary";
import type { MessageAck } from "../generated/MessageAck";
import type { MessageList } from "../generated/MessageList";
import type { MessageRejected } from "../generated/MessageRejected";
import type { NewMessage } from "../generated/NewMessage";
import type { ServerEvent } from "../generated/ServerEvent";
import { useAuthStore } from "./auth";
import {
  compareMessages,
  fromView,
  maxSeq,
  newClientMsgId,
  repairStartSeq,
  upsert,
  type ChatMessage,
} from "./chat-messages";
import { useRealtimeStore } from "./realtime";

// The Message shape and its pure helpers live in `chat-messages`; re-exported here
// so existing importers (`MessageList.vue`, `MessageComposer.vue`) keep one path.
export { MESSAGE_MAX_CHARS } from "./chat-messages";
export type { ChatMessage, DeliveryState } from "./chat-messages";

/**
 * Compile-time exhaustiveness check for the event switch.
 *
 * Runtime no-op: an unknown variant from a newer server is ignored, which is what
 * makes additive server events backward compatible.
 */
function assertExhaustive(_variant: never): void {}

/**
 * Conversations, Messages, and the send state machine.
 *
 * The store owns optimistic sending: `sendMessage` inserts a bubble immediately
 * and hands the payload to the realtime socket; the server's acknowledgement (or
 * rejection) reconciles it. It reads the session token from `auth`, and it
 * subscribes to the chat events `realtime` forwards — so the only dependency
 * direction is `chat → realtime, auth`.
 *
 * History is paged backwards on the Conversation's Sequence Number:
 * `loadMessages` fetches the newest page, and `loadOlder` walks to the page
 * before the oldest Message held. Every merge goes through {@link upsert}, so a
 * Message at a page boundary is reconciled rather than duplicated.
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
  const loadingOlder = ref(false);
  const errorMessage = ref<string | null>(null);
  const notice = ref<string | null>(null);

  /**
   * The backwards cursor per Conversation: the `seq` to pass as `before` for the
   * next older page, `null` once the beginning has been reached.
   */
  const nextBeforeByConversation = ref<Record<string, number | null>>({});
  /** Whether a Conversation has history older than what is loaded. */
  const hasMoreByConversation = ref<Record<string, boolean>>({});

  /** The in-flight repair walk, so concurrent repair triggers can coalesce. */
  let repairInFlight: Promise<void> | null = null;

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

  /** Whether the active Conversation has older history to page back into. */
  const hasMoreHistory = computed<boolean>(() => {
    const id = activeConversationId.value;
    if (id === null) return false;
    return hasMoreByConversation.value[id] ?? false;
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

  /**
   * Fetch the newest page of a Conversation's history, keeping local pending
   * bubbles and the pagination cursor in sync.
   *
   * A page is the most recent `DEFAULT_MESSAGE_PAGE_SIZE` Messages; the server
   * hands back `next_before` / `has_more`, which is what {@link loadOlder} walks.
   */
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

      nextBeforeByConversation.value[conversationId] =
        payload.next_before ?? null;
      hasMoreByConversation.value[conversationId] = payload.has_more === true;
    } catch (cause) {
      applyError(cause);
    } finally {
      loadingMessages.value = false;
    }
  }

  /**
   * Load the page of history older than the oldest Message already held.
   *
   * The cursor is the Conversation's Sequence Number, so this is immune to the
   * bug that makes `offset`-based paging lose or repeat rows while new Messages
   * arrive: the boundary is anchored to a `seq`, not to a count that shifts.
   * Messages are merged through {@link upsert}, so a Message that appears in both
   * the loaded list and the older page is kept exactly once.
   *
   * Returns whether a page was actually requested.
   */
  async function loadOlder(): Promise<boolean> {
    const conversationId = activeConversationId.value;
    if (conversationId === null) return false;
    if (loadingOlder.value) return false;
    if (hasMoreByConversation.value[conversationId] !== true) return false;

    const before = nextBeforeByConversation.value[conversationId];
    if (before === null || before === undefined) return false;

    const token = accessToken();
    if (token === null) return false;

    loadingOlder.value = true;
    errorMessage.value = null;
    try {
      const payload = await apiGetAuthed<MessageList>(
        `/conversations/${conversationId}/messages?before=${before}`,
        token,
      );
      const list = bucket(conversationId);
      for (const view of payload.messages) upsert(list, fromView(view, "sent"));

      nextBeforeByConversation.value[conversationId] =
        payload.next_before ?? null;
      hasMoreByConversation.value[conversationId] = payload.has_more === true;
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    } finally {
      loadingOlder.value = false;
    }
  }

  /**
   * Pull one Conversation forward from the last Sequence Number it holds.
   *
   * This is the Conversation-layer half of ADR-0003's repair: everything with a
   * `seq` greater than the client's cursor, oldest first, page by page. Every
   * merge goes through {@link upsert}, so re-delivered Messages are absorbed by
   * Message ID and applying a duplicate is a no-op.
   */
  async function repairConversation(
    conversationId: string,
    token: string,
  ): Promise<void> {
    const list = messagesByConversation.value[conversationId];
    if (list === undefined) return;

    // Nothing held means there is no cursor to walk from; the newest page is the
    // only sensible starting point.
    if (list.length === 0) {
      await loadMessages(conversationId);
      return;
    }

    const start = repairStartSeq(list);
    if (start === null) return;
    let cursor = start;

    for (;;) {
      const payload: MessageList = await apiGetAuthed<MessageList>(
        `/conversations/${conversationId}/messages?after=${cursor}&limit=100`,
        token,
      );
      for (const view of payload.messages) upsert(list, fromView(view, "sent"));

      if (payload.has_more !== true) break;
      const next: number | null = payload.next_after ?? null;
      // A cursor that does not advance would loop forever; the server guarantees
      // one, but a defensive break costs nothing and cannot hide a real page.
      if (next === null || next <= cursor) break;
      cursor = next;
    }
  }

  /**
   * Repair every Conversation the client has loaded, refreshing the list first.
   *
   * Coalesced: several triggers (a reconnect, a connection-level gap, a
   * Conversation-level gap) can request a repair at the same instant, and one
   * walk is enough for all of them.
   */
  function repairConversations(): Promise<void> {
    if (repairInFlight !== null) return repairInFlight;

    repairInFlight = runRepairs().finally(() => {
      repairInFlight = null;
    });
    return repairInFlight;
  }

  async function runRepairs(): Promise<void> {
    const token = accessToken();
    if (token === null) return;

    await loadConversations();

    for (const conversationId of Object.keys(messagesByConversation.value)) {
      try {
        await repairConversation(conversationId, token);
      } catch (cause) {
        applyError(cause);
        return;
      }
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
    const list = bucket(view.conversation_id);
    const before = maxSeq(list);
    upsert(list, fromView(view, "sent"));

    // A Message more than one Sequence Number beyond the high-water mark means the
    // Conversation stream has a hole; pull the missing range forward. The
    // connection layer may have filled its own gap, but this is the exact repair.
    if (before !== null && view.seq > before + 1) {
      void repairConversations();
    }

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
      case "Resync":
        // Handled by the realtime store, which owns the connection sequence.
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

  // A reconnect the server cannot resume, a connection-level gap, or an
  // unavailable replay position all mean the same thing here: repair forward from
  // each Conversation's own cursor. Coalesced, so several triggers are one walk.
  useRealtimeStore().onResync(() => {
    void repairConversations();
  });

  function clearNotice(): void {
    notice.value = null;
  }

  return {
    conversations,
    activeConversationId,
    activeConversation,
    messages,
    hasMoreHistory,
    loadingConversations,
    loadingMessages,
    loadingOlder,
    errorMessage,
    notice,
    loadConversations,
    startDirect,
    openConversation,
    loadOlder,
    sendMessage,
    retry,
    repairConversations,
    clearNotice,
  };
});
