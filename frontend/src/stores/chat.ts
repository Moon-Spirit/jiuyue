import { defineStore } from "pinia";
import { computed, ref } from "vue";
import {
  ApiError,
  apiDeleteAuthed,
  apiDeleteAuthedNoContent,
  apiGetAuthed,
  apiPatchAuthed,
  apiPost,
  apiPostAuthed,
  apiPostAuthedNoContent,
} from "../api/client";
import type { ConversationList } from "../generated/ConversationList";
import type { ConversationSummary } from "../generated/ConversationSummary";
import type { GroupInfo } from "../generated/GroupInfo";
import type { GroupSummary } from "../generated/GroupSummary";
import type { MemberView } from "../generated/MemberView";
import type { MembershipChanged } from "../generated/MembershipChanged";
import type { MessageAck } from "../generated/MessageAck";
import type { MessageList } from "../generated/MessageList";
import type { MessageRejected } from "../generated/MessageRejected";
import type { NewMessage } from "../generated/NewMessage";
import type { ReadMarker } from "../generated/ReadMarker";
import type { ReadReceipt } from "../generated/ReadReceipt";
import type { Role } from "../generated/Role";
import type { ServerEvent } from "../generated/ServerEvent";
import type { SyncState } from "../generated/SyncState";
import { useAuthStore } from "./auth";
import {
  compareMessages,
  fromView,
  maxSeq,
  minSeq,
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

  /**
   * The **Device's** persisted Sync Cursor per Conversation, learned from the
   * server's `SyncState` frame.
   *
   * This is deliberately not derived from the in-memory buckets: the point of a
   * per-Device cursor is that it outlives the page. The server remembers what this
   * Device consumed, and re-sends those positions on connect so a returning client
   * repairs forward from exactly where it stopped instead of re-reading whole
   * Conversations.
   */
  const syncCursors = ref<Record<string, number>>({});

  /**
   * The **User's** Unread Count per Conversation (CONTEXT.md: 未读数).
   *
   * Scoped to the User, not the Device: reading on one Device clears it on all of
   * them. Seeded from the Conversation list's `unread_count`, bumped locally when
   * a peer's Message arrives, and settled authoritatively by `ReadMarker` events —
   * which is what makes it eventually consistent across the account's Devices.
   */
  const unreadCounts = ref<Record<string, number>>({});

  /**
   * The other Participant's public Read Receipt per Conversation.
   *
   * Deliberately a *different* map from {@link unreadCounts}: this is the peer's
   * public position, rendered as "read" on the caller's own Messages. It is fed
   * by `ReadReceipt` events and by the `read_receipts` of a history page — never
   * by the caller's private Read Marker, which the server does not send to peers
   * and which this store does not read from any peer-facing shape.
   */
  const peerReceipts = ref<Record<string, number>>({});

  /**
   * The member list of each Group Conversation, keyed by Conversation id.
   *
   * Seeded from `GET /conversations/{id}` when a group is opened and kept current
   * by `MembershipChanged` events, so the info panel reflects a join, a leave, a
   * removal or a Role change without a refetch. A Direct Conversation has no entry
   * here — it has no member list to show.
   */
  const groupMembers = ref<Record<string, MemberView[]>>({});

  /** Whether a Group's member list is being loaded. */
  const loadingGroupInfo = ref(false);

  /** The highest read position this client has already reported per Conversation. */
  const reportedReadSeq = new Map<string, number>();

  /** Cursor reports waiting to be sent, one per Conversation. */
  const pendingCursorReports = new Map<string, number>();
  /** Whether a flush is already queued for the current batch of reports. */
  let cursorReportScheduled = false;

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

      // The list is the authoritative seed for the badges: each row carries the
      // caller's own Unread Count, computed server-side from the private marker.
      const counts: Record<string, number> = {};
      for (const conversation of payload.conversations) {
        counts[conversation.id] = conversation.unread_count;
      }
      unreadCounts.value = counts;
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
      // Entering the Conversation is reading it: clear the badge on every Device
      // of the account through the server's private Read Marker.
      markConversationRead(conversation.id);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /**
   * Create a Group Conversation with the caller as owner, then focus it.
   *
   * Members are named by `@handle`; the server resolves them, enforces the
   * three-Participant minimum and returns the owner's view of the new group.
   */
  async function createGroup(
    title: string,
    memberUsernames: readonly string[],
  ): Promise<boolean> {
    const token = accessToken();
    if (token === null) return false;

    const name = title.trim();
    if (name === "") {
      errorMessage.value = "请输入群名称";
      return false;
    }
    const members = memberUsernames
      .map((username) => username.trim())
      .filter((username) => username !== "");
    if (members.length < 2) {
      errorMessage.value = "群聊至少需要邀请两位成员";
      return false;
    }

    errorMessage.value = null;
    try {
      const conversation = await apiPostAuthed<ConversationSummary>(
        "/conversations/group",
        { title: name, member_usernames: members },
        token,
      );
      upsertConversation(conversation);
      activeConversationId.value = conversation.id;
      await loadGroupInfo(conversation.id);
      markConversationRead(conversation.id);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /**
   * Load (or refresh) a Group's member list, and adopt the summary it carries.
   *
   * The summary adopted is the server's, so the caller's own Role is never
   * guessed locally: a transfer or a demotion shows up here without a refetch.
   */
  async function loadGroupInfo(conversationId: string): Promise<void> {
    const token = accessToken();
    if (token === null) return;

    loadingGroupInfo.value = true;
    errorMessage.value = null;
    try {
      const info = await apiGetAuthed<GroupInfo>(
        `/conversations/${conversationId}`,
        token,
      );
      applyGroupInfo(info);
    } catch (cause) {
      applyError(cause);
    } finally {
      loadingGroupInfo.value = false;
    }
  }

  /** Invite Users into a Group, then adopt the refreshed member list. */
  async function inviteMembers(
    conversationId: string,
    memberUsernames: readonly string[],
  ): Promise<boolean> {
    const token = accessToken();
    if (token === null) return false;

    const members = memberUsernames
      .map((username) => username.trim())
      .filter((username) => username !== "");
    if (members.length === 0) {
      errorMessage.value = "请输入要邀请的用户名";
      return false;
    }

    errorMessage.value = null;
    try {
      const info = await apiPostAuthed<GroupInfo>(
        `/conversations/${conversationId}/members`,
        { member_usernames: members },
        token,
      );
      applyGroupInfo(info);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /** Remove a Participant from a Group. */
  async function removeMember(
    conversationId: string,
    userId: string,
  ): Promise<boolean> {
    const token = accessToken();
    if (token === null) return false;

    errorMessage.value = null;
    try {
      const info = await apiDeleteAuthed<GroupInfo>(
        `/conversations/${conversationId}/members/${userId}`,
        token,
      );
      applyGroupInfo(info);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /** Promote a member to admin, or demote an admin back to member. */
  async function setMemberRole(
    conversationId: string,
    userId: string,
    role: Exclude<Role, "owner">,
  ): Promise<boolean> {
    const token = accessToken();
    if (token === null) return false;

    errorMessage.value = null;
    try {
      const info = await apiPatchAuthed<GroupInfo>(
        `/conversations/${conversationId}/members/${userId}`,
        { role },
        token,
      );
      applyGroupInfo(info);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /** Hand ownership of a Group to another Participant. */
  async function transferOwnership(
    conversationId: string,
    userId: string,
  ): Promise<boolean> {
    const token = accessToken();
    if (token === null) return false;

    errorMessage.value = null;
    try {
      const info = await apiPostAuthed<GroupInfo>(
        `/conversations/${conversationId}/transfer`,
        { user_id: userId },
        token,
      );
      applyGroupInfo(info);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /** Leave a Group. The caller's own list drops it immediately. */
  async function leaveGroup(conversationId: string): Promise<boolean> {
    const token = accessToken();
    if (token === null) return false;

    errorMessage.value = null;
    try {
      await apiPostAuthedNoContent(
        `/conversations/${conversationId}/leave`,
        token,
      );
      dropConversation(conversationId);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /** Dissolve a Group the caller owns; it disappears for everyone. */
  async function dissolveGroup(conversationId: string): Promise<boolean> {
    const token = accessToken();
    if (token === null) return false;

    errorMessage.value = null;
    try {
      await apiDeleteAuthedNoContent(`/conversations/${conversationId}`, token);
      dropConversation(conversationId);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    }
  }

  /** Adopt a Group's authoritative member list and summary. */
  function applyGroupInfo(info: GroupInfo): void {
    groupMembers.value[info.conversation.id] = info.members;
    upsertConversation(info.conversation);
  }

  /**
   * Forget a Conversation entirely: it is no longer visible to the caller.
   *
   * Used when the caller leaves, is removed, or the group is dissolved — and on
   * the `MembershipChanged` events that tell a client about those. The Message
   * bucket goes too: keeping it would leave a chat the user cannot open, and the
   * server would refuse to serve history for it anyway.
   */
  function dropConversation(conversationId: string): void {
    conversations.value = conversations.value.filter(
      (conversation) => conversation.id !== conversationId,
    );
    delete messagesByConversation.value[conversationId];
    delete groupMembers.value[conversationId];
    delete unreadCounts.value[conversationId];
    delete peerReceipts.value[conversationId];

    if (activeConversationId.value === conversationId) {
      activeConversationId.value = null;
    }
  }

  /** Insert or replace one member in a Conversation's member list. */
  function upsertMember(conversationId: string, member: MemberView): void {
    const members = groupMembers.value[conversationId];
    if (members === undefined) {
      groupMembers.value[conversationId] = [member];
      return;
    }

    const index = members.findIndex(
      (entry) => entry.user_id === member.user_id,
    );
    if (index >= 0) members.splice(index, 1, member);
    else members.push(member);
  }

  /** Mutate the Group part of a Conversation's summary, when it has one. */
  function withGroupSummary(
    conversationId: string,
    mutate: (group: GroupSummary) => void,
  ): void {
    const conversation = conversations.value.find(
      (entry) => entry.id === conversationId,
    );
    if (conversation === undefined || conversation.group === undefined) return;
    mutate(conversation.group);
  }

  /** Keep the summary's Participant count in step with a membership change. */
  function adjustMemberCount(conversationId: string, delta: number): void {
    withGroupSummary(conversationId, (group) => {
      group.member_count = Math.max(0, group.member_count + delta);
    });
  }

  /** Update the caller's own Role in a Conversation's summary. */
  function setMyRole(conversationId: string, role: Role): void {
    withGroupSummary(conversationId, (group) => {
      group.my_role = role;
    });
  }

  /**
   * Apply one membership change.
   *
   * This is the single reducer for all six shapes, and it keeps three things
   * coherent in one place: the member list, the Conversation summary (count and
   * the caller's own Role), and the caller's Conversation list (a group the caller
   * left, was removed from, or that was dissolved leaves it).
   *
   * The server decides **who** receives the event; this only decides how it lands.
   */
  function applyMembershipChanged(payload: MembershipChanged): void {
    const conversationId = payload.conversation_id;
    const change = payload.change;
    const selfId = useAuthStore().user?.id ?? "";

    switch (change.type) {
      case "joined":
        upsertMember(conversationId, change.member);
        adjustMemberCount(conversationId, 1);
        break;
      case "left":
      case "removed": {
        const members = groupMembers.value[conversationId];
        if (members !== undefined) {
          groupMembers.value[conversationId] = members.filter(
            (member) => member.user_id !== change.user_id,
          );
        }
        adjustMemberCount(conversationId, -1);
        if (change.user_id === selfId) dropConversation(conversationId);
        break;
      }
      case "role_changed": {
        const members = groupMembers.value[conversationId];
        const member = members?.find(
          (entry) => entry.user_id === change.user_id,
        );
        if (member !== undefined) member.role = change.role;
        if (change.user_id === selfId) setMyRole(conversationId, change.role);
        break;
      }
      case "ownership_transferred": {
        const members = groupMembers.value[conversationId];
        const from = members?.find(
          (member) => member.user_id === change.from_user_id,
        );
        const to = members?.find(
          (member) => member.user_id === change.to_user_id,
        );
        // The outgoing owner is demoted to admin; the incoming one owns the group.
        if (from !== undefined) from.role = "admin";
        if (to !== undefined) to.role = "owner";
        if (change.from_user_id === selfId) setMyRole(conversationId, "admin");
        if (change.to_user_id === selfId) setMyRole(conversationId, "owner");
        break;
      }
      case "dissolved":
        dropConversation(conversationId);
        break;
      default:
        assertExhaustive(change);
    }
  }

  /** Focus a Conversation, loading its history the first time it is opened. */
  async function openConversation(conversationId: string): Promise<void> {
    activeConversationId.value = conversationId;
    if (messagesByConversation.value[conversationId] === undefined) {
      await loadMessages(conversationId);
    }
    // A Group's member list is only fetched when its info panel could be shown,
    // and only once: `MembershipChanged` keeps it current afterwards.
    const conversation = conversations.value.find(
      (entry) => entry.id === conversationId,
    );
    if (
      conversation?.kind === "group" &&
      groupMembers.value[conversationId] === undefined
    ) {
      await loadGroupInfo(conversationId);
    }
    // Entering a Conversation is the first read of it; the scroll-to-bottom
    // trigger in `MessageList` keeps it current as new Messages arrive.
    markConversationRead(conversationId);
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
      // The page carries the peers' public receipts, so "read" indicators render
      // on first paint instead of waiting for a live event. Never the caller's
      // own private marker — the contract has no such field.
      for (const receipt of payload.read_receipts ?? [])
        applyPeerReceipt(receipt);
      // The newest page is the position this Device now holds.
      scheduleCursorReport(conversationId);
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
      for (const receipt of payload.read_receipts ?? [])
        applyPeerReceipt(receipt);
      // Older history does not move the high-water mark, but the report keeps the
      // persisted Device cursor honest with what is loaded.
      scheduleCursorReport(conversationId);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    } finally {
      loadingOlder.value = false;
    }
  }

  /**
   * Note how far this Device has consumed a Conversation.
   *
   * Called wherever a Message is applied — live delivery, history, repair — so the
   * persisted Sync Cursor follows the client's real position, not a guess.
   *
   * The value reported is {@link repairStartSeq}, not the highest Sequence Number
   * held: a report is a promise that nothing at or below it still needs sending, so
   * a position with a hole under it would make a later resume skip that hole
   * silently. `repairStartSeq` is exactly the highest position with no hole below
   * it (or the highest held when there is none), i.e. the largest value it is safe
   * to promise.
   *
   * Reports are batched on a microtask: several Messages applied in one turn become
   * one frame, which the server coalesces again before it writes anything, so a
   * chatty client cannot become a write per Message.
   */
  function scheduleCursorReport(conversationId: string): void {
    const safe = repairStartSeq(
      messagesByConversation.value[conversationId] ?? [],
    );
    if (safe === null) return;

    const queued = pendingCursorReports.get(conversationId);
    pendingCursorReports.set(
      conversationId,
      queued === undefined ? safe : Math.max(queued, safe),
    );

    if (cursorReportScheduled) return;
    cursorReportScheduled = true;
    queueMicrotask(flushCursorReports);
  }

  /**
   * Send every queued report, keeping the ones the socket refused.
   *
   * A closed socket is not a lost position: the entry stays queued, and the next
   * applied Message re-arms the flush. If the client never reconnects, the server
   * simply re-sends the tail on the next connect — at-least-once tolerates that.
   */
  function flushCursorReports(): void {
    cursorReportScheduled = false;
    if (pendingCursorReports.size === 0) return;

    const realtime = useRealtimeStore();
    for (const [conversationId, lastSeq] of [...pendingCursorReports]) {
      const sent = realtime.send({
        t: "SyncCursor",
        d: { conversation_id: conversationId, last_seq: lastSeq },
      });
      if (sent) pendingCursorReports.delete(conversationId);
    }
  }

  /**
   * Report that the User has read a Conversation up to its newest held Message.
   *
   * The Read Marker is per **User**, so the server echoes it to every Device of
   * the account — which is exactly what clears the badge on the account's other
   * Devices without any of them opening the Conversation. The value sent is the
   * highest Sequence Number actually held; the server clamps it to real Messages
   * and only ever moves forward, and {@link reportedReadSeq} stops this client
   * reporting the same position twice.
   */
  function markConversationRead(conversationId: string): boolean {
    const newest = maxSeq(messagesByConversation.value[conversationId] ?? []);
    if (newest === null) return false;

    const already = reportedReadSeq.get(conversationId) ?? 0;
    if (newest <= already) return false;

    const sent = useRealtimeStore().send({
      t: "MarkRead",
      d: { conversation_id: conversationId, last_read_seq: newest },
    });
    if (sent) {
      reportedReadSeq.set(conversationId, newest);
      // Optimistic clear: the server's ReadMarker confirms the authoritative
      // value, but the badge should not wait a round trip to disappear.
      unreadCounts.value[conversationId] = 0;
    }
    return sent;
  }

  /** Mark the currently focused Conversation read, if there is one. */
  function markActiveRead(): boolean {
    const conversationId = activeConversationId.value;
    if (conversationId === null) return false;
    return markConversationRead(conversationId);
  }

  /**
   * Record the peer's public Read Receipt.
   *
   * Receipts only move forward, so a re-delivered or out-of-order one cannot
   * rewind the indicator. Kept in {@link peerReceipts}, separate from the
   * private badge state: one is a peer's public position, the other a User's own
   * private reading.
   */
  function applyPeerReceipt(receipt: ReadReceipt): void {
    const current = peerReceipts.value[receipt.conversation_id] ?? 0;
    peerReceipts.value[receipt.conversation_id] = Math.max(
      current,
      receipt.last_read_seq,
    );
  }

  /**
   * Adopt the account's private Read Marker, delivered to this Device because it
   * belongs to the same User.
   *
   * The Unread Count in the event is authoritative and overwrites any local
   * guess, which is how two Devices converge without either one polling. The
   * reported position is folded in too, so this Device does not echo back a read
   * the account has already made.
   */
  function applyReadMarker(marker: ReadMarker): void {
    unreadCounts.value[marker.conversation_id] = marker.unread_count;
    const already = reportedReadSeq.get(marker.conversation_id) ?? 0;
    reportedReadSeq.set(
      marker.conversation_id,
      Math.max(already, marker.last_read_seq),
    );
  }

  /** A peer's public Read Receipt advanced. */
  function applyReadReceipt(receipt: ReadReceipt): void {
    applyPeerReceipt(receipt);
  }

  /**
   * Adopt the Device's persisted cursors from `SyncState`, then catch up.
   *
   * The server pushes this once per connection when this Device has stored
   * positions. Recording them is what makes a reload resumable; the repair then
   * walks each Conversation forward from its cursor — the same bounded primitive a
   * reconnect uses, not a parallel path.
   */
  function adoptSyncState(state: SyncState): void {
    for (const cursor of state.cursors) {
      syncCursors.value[cursor.conversation_id] = cursor.last_seq;
    }

    void repairConversations();
  }

  /**
   * Pull one Conversation forward from the Device's cursor.
   *
   * This is the Conversation-layer half of ADR-0003's repair: everything with a
   * `seq` greater than the client's position, oldest first, page by page. Every
   * merge goes through {@link upsert}, so re-delivered Messages are absorbed by
   * Message ID and applying a duplicate is a no-op.
   *
   * The start is the client's own contiguous position when it holds Messages, and
   * the **persisted Device cursor** when it holds nothing (a reload, a restarted
   * process): the server's record of what this Device consumed is the floor, so the
   * client is told exactly what it missed rather than re-reading the Conversation.
   */
  async function repairConversation(
    conversationId: string,
    token: string,
  ): Promise<void> {
    const list = messagesByConversation.value[conversationId];
    if (list === undefined) return;

    const floor = syncCursors.value[conversationId] ?? null;
    const start = repairStartSeq(list) ?? floor;

    // Nothing held and no stored cursor: the newest page is the only sensible start.
    if (start === null) {
      await loadMessages(conversationId);
      return;
    }

    // Whether the walk's start came from the persisted cursor rather than from
    // Messages already held. Only that case can leave the bucket holding just the
    // missed tail, which needs a backwards cursor wired in below.
    const seededFromCursor = repairStartSeq(list) === null;
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

    // A cursor-seeded walk can leave only the missed tail in the bucket. Wire the
    // backwards cursor so the older history stays reachable and the view is not
    // stuck on a partial list.
    if (seededFromCursor) {
      const oldest = minSeq(list);
      nextBeforeByConversation.value[conversationId] = oldest;
      hasMoreByConversation.value[conversationId] =
        oldest !== null && oldest > 1;
    }

    scheduleCursorReport(conversationId);
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
    scheduleCursorReport(ack.message.conversation_id);
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
    // `upsert` reports whether this was a new Message or a re-delivery. Delivery
    // is at-least-once, so only a genuinely new Message may raise the badge.
    const inserted = upsert(list, fromView(view, "sent"));

    // A Message more than one Sequence Number beyond the high-water mark means the
    // Conversation stream has a hole; pull the missing range forward. The
    // connection layer may have filled its own gap, but this is the exact repair.
    if (before !== null && view.seq > before + 1) {
      void repairConversations();
    }

    // The badge is the User's, not the Device's. A peer's Message raises it
    // immediately; the server's `ReadMarker` settles it authoritatively, and a
    // Conversation open at the bottom clears it through `markConversationRead`.
    const fromSelf = view.sender_id === (useAuthStore().user?.id ?? "");
    if (inserted && !fromSelf) {
      const current = unreadCounts.value[view.conversation_id] ?? 0;
      unreadCounts.value[view.conversation_id] = current + 1;
    }

    scheduleCursorReport(view.conversation_id);

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
      case "MembershipChanged":
        applyMembershipChanged(event.d);
        break;
      case "SyncState":
        adoptSyncState(event.d);
        break;
      case "ReadMarker":
        applyReadMarker(event.d);
        break;
      case "ReadReceipt":
        applyReadReceipt(event.d);
        break;
      case "Presence":
        // Presence is not chat state: the realtime store routes it to the presence
        // store's own subscription, so it never reaches this reducer. The case
        // exists because the tagged union is exhaustive — the compiler refuses to
        // let a new event type go unconsidered.
        break;
      case "Typing":
        // A Typing Indicator is ephemeral state about a Participant, not Message
        // history, so the realtime store routes it to the typing store's own
        // subscription. It is deliberately not reduced here: nothing about it may
        // touch the Conversation or Message state that is persisted and repaired.
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
    syncCursors,
    unreadCounts,
    peerReceipts,
    groupMembers,
    loadingGroupInfo,
    hasMoreHistory,
    loadingConversations,
    loadingMessages,
    loadingOlder,
    errorMessage,
    notice,
    loadConversations,
    startDirect,
    createGroup,
    loadGroupInfo,
    inviteMembers,
    removeMember,
    setMemberRole,
    transferOwnership,
    leaveGroup,
    dissolveGroup,
    openConversation,
    loadOlder,
    sendMessage,
    retry,
    markConversationRead,
    markActiveRead,
    repairConversations,
    clearNotice,
  };
});
