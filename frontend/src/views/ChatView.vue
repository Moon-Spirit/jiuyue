<script setup lang="ts">
import { storeToRefs } from "pinia";
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { RouterLink, useRouter } from "vue-router";
import type { SocketStatus } from "../api/ws";
import ConversationList from "../components/chat/ConversationList.vue";
import CreateGroupForm from "../components/chat/CreateGroupForm.vue";
import GroupInfoPanel from "../components/chat/GroupInfoPanel.vue";
import MessageComposer from "../components/chat/MessageComposer.vue";
import MessageList from "../components/chat/MessageList.vue";
import PresenceBadge from "../components/chat/PresenceBadge.vue";
import type { PresenceStatus } from "../generated/PresenceStatus";
import { useAuthStore } from "../stores/auth";
import { useChatStore } from "../stores/chat";
import { usePresenceStore } from "../stores/presence";
import { useRealtimeStore } from "../stores/realtime";

const auth = useAuthStore();
const chat = useChatStore();
const presence = usePresenceStore();
const realtime = useRealtimeStore();
const router = useRouter();

const { user } = storeToRefs(auth);
const {
  conversations,
  activeConversation,
  activeConversationId,
  messages,
  groupMembers,
  loadingGroupInfo,
  hasMoreHistory,
  loadingConversations,
  loadingMessages,
  loadingOlder,
  unreadCounts,
  peerReceipts,
  errorMessage,
  notice,
} = storeToRefs(chat);
const { status } = storeToRefs(realtime);

const newPeer = ref("");
const starting = ref(false);
const creatingGroup = ref(false);
/** Whether the Group info panel is showing instead of the message list. */
const showGroupInfo = ref(false);

const currentUserId = computed(() => user.value?.id ?? "");
const connected = computed(() => status.value === "open");
const socketLabel = computed(() => SOCKET_LABELS[status.value]);

/** Whether the focused Conversation is a Group (and therefore has an info panel). */
const isGroup = computed(() => activeConversation.value?.group !== undefined);

/** The focused Conversation's title: a group's name, or the peer's. */
const conversationTitle = computed(() => {
  const conversation = activeConversation.value;
  if (conversation === null) return "";
  if (conversation.group !== undefined) return conversation.group.title;
  return (
    conversation.peer?.display_name || conversation.peer?.username || "群聊"
  );
});

/** The focused Conversation's second line: member count, or the peer's handle. */
const conversationSubtitle = computed(() => {
  const conversation = activeConversation.value;
  if (conversation === null) return "";
  if (conversation.group !== undefined) {
    return `${conversation.group.member_count} 位成员`;
  }
  return `@${conversation.peer?.username ?? "group"}`;
});

/** The focused Group's member list, or an empty list for a Direct Conversation. */
const activeGroupMembers = computed(() => {
  const id = activeConversationId.value;
  if (id === null) return [];
  return groupMembers.value[id] ?? [];
});

/**
 * The focused Conversation's peer receipt, or `0`.
 *
 * A public Read Receipt drives the "read" indicator on the user's own Messages;
 * it is not the caller's private Read Marker, which never reaches this view.
 */
const activePeerReceiptSeq = computed(() => {
  const id = activeConversationId.value;
  if (id === null) return 0;
  return peerReceipts.value[id] ?? 0;
});

/** The focused Conversation's peer id, or `null` for a Group (which has none). */
const activePeerId = computed(() => activeConversation.value?.peer?.id ?? null);

/** The focused peer's reachability, or `null` while nothing is known yet. */
const activePeerStatus = computed<PresenceStatus | null>(() => {
  const id = activePeerId.value;
  return id === null ? null : presence.statusFor(id);
});

/** When the focused peer was last reachable; `null` while online or unknown. */
const activePeerLastSeen = computed<number | null>(() => {
  const id = activePeerId.value;
  return id === null ? null : presence.lastSeenFor(id);
});

/**
 * Every Direct peer's known presence, for the Conversation list's dots.
 *
 * Derived from the Conversation list rather than tracked per row, so a peer's
 * status change re-renders the list from one authoritative map.
 */
const peerStatuses = computed<Record<string, PresenceStatus>>(() => {
  const statuses: Record<string, PresenceStatus> = {};
  for (const conversation of conversations.value) {
    const peer = conversation.peer;
    if (peer === null) continue;
    const status = presence.statusFor(peer.id);
    if (status !== null) statuses[peer.id] = status;
  }
  return statuses;
});

/**
 * The Direct peers currently on screen, as a stable string key.
 *
 * Watching a joined key rather than the array means an in-place `unshift` of a new
 * Conversation still triggers a presence read, which a shallow watch on the array
 * reference would miss.
 */
const peerIdKey = computed(() =>
  conversations.value
    .filter((conversation) => conversation.peer !== null)
    .map((conversation) => conversation.peer?.id ?? "")
    .sort()
    .join(","),
);

/** Ask the server for the current presence of every Direct peer. */
function loadPresence(): void {
  void presence.load(peerIdKey.value === "" ? [] : peerIdKey.value.split(","));
}

// The Conversation list is the seed: once the peers are known, ask for their
// presence once, and let the socket keep it current from then on.
watch(peerIdKey, loadPresence, { immediate: true });

const SOCKET_LABELS: Record<SocketStatus, string> = {
  idle: "未连接",
  connecting: "连接中…",
  open: "已连接",
  reconnecting: "重连中…",
  closed: "已断开",
};

onMounted(async () => {
  realtime.connect();
  await chat.loadConversations();
});

// A socket that opens (or reopens) after the Conversation was entered must still
// report the read: its socket was not there to carry the `MarkRead` at entry
// time. Idempotent, because the store only reports a position once.
watch(connected, (isConnected) => {
  if (isConnected) {
    chat.markActiveRead();
    // Presence events only arrive while the socket is up, so a reconnect is the
    // moment to re-read the current state rather than trust a map that went stale
    // while the user was disconnected.
    loadPresence();
  }
});

onUnmounted(() => {
  realtime.disconnect();
});

// Switching Conversations leaves the info panel behind: it belongs to the chat
// that was open, not to the one now focused.
watch(activeConversationId, () => {
  showGroupInfo.value = false;
});

async function startChat(): Promise<void> {
  if (starting.value) return;

  starting.value = true;
  try {
    const opened = await chat.startDirect(newPeer.value);
    if (opened) newPeer.value = "";
  } finally {
    starting.value = false;
  }
}

/** Create a Group and open its info panel, so the new group is inspectable. */
async function createGroup(title: string, members: string[]): Promise<void> {
  if (creatingGroup.value) return;

  creatingGroup.value = true;
  try {
    const created = await chat.createGroup(title, members);
    if (created) showGroupInfo.value = true;
  } finally {
    creatingGroup.value = false;
  }
}

function inviteMembers(usernames: string[]): void {
  const id = activeConversationId.value;
  if (id === null) return;
  void chat.inviteMembers(id, usernames);
}

function removeMember(userId: string): void {
  const id = activeConversationId.value;
  if (id === null) return;
  void chat.removeMember(id, userId);
}

function setMemberRole(userId: string, role: "admin" | "member"): void {
  const id = activeConversationId.value;
  if (id === null) return;
  void chat.setMemberRole(id, userId, role);
}

function transferOwnership(userId: string): void {
  const id = activeConversationId.value;
  if (id === null) return;
  void chat.transferOwnership(id, userId);
}

async function leaveGroup(): Promise<void> {
  const id = activeConversationId.value;
  if (id === null) return;
  await chat.leaveGroup(id);
  showGroupInfo.value = false;
}

async function dissolveGroup(): Promise<void> {
  const id = activeConversationId.value;
  if (id === null) return;
  await chat.dissolveGroup(id);
  showGroupInfo.value = false;
}

async function signOut(): Promise<void> {
  realtime.disconnect();
  // Presence belongs to the signed-in User: leaving it behind would show the next
  // account the previous one's peers as online.
  presence.clear();
  await auth.logout();
  await router.replace({ name: "login" });
}
</script>

<template>
  <main
    class="flex h-screen bg-zinc-50 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <aside
      class="flex w-72 shrink-0 flex-col border-r border-zinc-200 bg-white dark:border-zinc-800 dark:bg-zinc-900"
    >
      <header class="border-b border-zinc-200 p-4 dark:border-zinc-800">
        <div class="flex items-baseline justify-between gap-2">
          <h1 class="text-base font-semibold tracking-tight">jiuyue · 九月</h1>
          <span
            class="text-xs"
            :class="
              connected
                ? 'text-emerald-600 dark:text-emerald-400'
                : 'text-amber-600 dark:text-amber-400'
            "
            data-test="socket-status"
          >
            {{ socketLabel }}
          </span>
        </div>
        <p class="mt-1 truncate text-xs text-zinc-500">
          @{{ user?.username ?? "—" }}
        </p>
      </header>

      <form
        class="flex gap-2 border-b border-zinc-200 p-3 dark:border-zinc-800"
        @submit.prevent="startChat"
      >
        <input
          v-model="newPeer"
          type="text"
          placeholder="对方用户名，如 bob"
          class="min-w-0 flex-1 rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
          data-test="peer-input"
        />
        <button
          type="submit"
          class="rounded-lg bg-zinc-900 px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
          :disabled="starting || newPeer.trim() === ''"
        >
          开始
        </button>
      </form>

      <CreateGroupForm :busy="creatingGroup" @create="createGroup" />

      <ConversationList
        :conversations="conversations"
        :active-id="activeConversationId"
        :loading="loadingConversations"
        :unread-counts="unreadCounts"
        :presence="peerStatuses"
        @select="chat.openConversation($event)"
      />

      <footer class="border-t border-zinc-200 p-3 dark:border-zinc-800">
        <div class="flex gap-2">
          <RouterLink
            class="flex-1 rounded-lg border border-zinc-300 px-2 py-2 text-center text-xs font-medium transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
            :to="{ name: 'account' }"
          >
            账号
          </RouterLink>
          <RouterLink
            class="flex-1 rounded-lg border border-zinc-300 px-2 py-2 text-center text-xs font-medium transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
            :to="{ name: 'health' }"
          >
            状态
          </RouterLink>
          <button
            type="button"
            class="flex-1 rounded-lg border border-zinc-300 px-2 py-2 text-xs font-medium transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
            @click="signOut"
          >
            退出
          </button>
        </div>
      </footer>
    </aside>

    <section class="flex min-w-0 flex-1 flex-col">
      <p
        v-if="user !== null && !user.email_verified"
        class="flex items-center justify-between gap-3 border-b border-amber-200 bg-amber-50 px-4 py-2 text-xs text-amber-800 dark:border-amber-900 dark:bg-amber-950/40 dark:text-amber-300"
        data-test="verify-banner"
      >
        <span>邮箱尚未验证：验证后才能开始新的会话。</span>
        <RouterLink
          class="shrink-0 font-medium underline underline-offset-4"
          :to="{ name: 'verify-email' }"
        >
          去验证
        </RouterLink>
      </p>

      <template v-if="activeConversation !== null">
        <header
          class="flex items-center justify-between gap-3 border-b border-zinc-200 bg-white px-4 py-3 dark:border-zinc-800 dark:bg-zinc-900"
        >
          <div class="min-w-0">
            <h2
              class="truncate text-sm font-semibold"
              data-test="conversation-title"
            >
              {{ conversationTitle }}
            </h2>
            <div class="flex items-center gap-2">
              <p class="truncate text-xs text-zinc-500">
                {{ conversationSubtitle }}
              </p>
              <PresenceBadge
                v-if="!isGroup"
                :status="activePeerStatus"
                :last-seen-ms="activePeerLastSeen"
              />
            </div>
          </div>
          <button
            v-if="isGroup"
            type="button"
            class="shrink-0 rounded-lg border border-zinc-300 px-3 py-1.5 text-xs font-medium transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
            data-test="toggle-group-info"
            @click="showGroupInfo = !showGroupInfo"
          >
            {{ showGroupInfo ? "返回消息" : "群资料" }}
          </button>
        </header>

        <p
          v-if="notice"
          class="cursor-pointer border-b border-amber-200 bg-amber-50 px-4 py-2 text-xs text-amber-800 dark:border-amber-900 dark:bg-amber-950/40 dark:text-amber-300"
          data-test="notice"
          @click="chat.clearNotice()"
        >
          {{ notice }}
        </p>

        <div class="min-h-0 flex-1">
          <GroupInfoPanel
            v-if="isGroup && showGroupInfo"
            :conversation="activeConversation"
            :members="activeGroupMembers"
            :current-user-id="currentUserId"
            :loading="loadingGroupInfo"
            @invite="inviteMembers"
            @remove="removeMember"
            @set-role="setMemberRole"
            @transfer="transferOwnership"
            @leave="leaveGroup"
            @dissolve="dissolveGroup"
          />
          <MessageList
            v-else
            :messages="messages"
            :current-user-id="currentUserId"
            :loading="loadingMessages"
            :has-more="hasMoreHistory"
            :loading-older="loadingOlder"
            :peer-receipt-seq="activePeerReceiptSeq"
            @retry="chat.retry($event)"
            @load-older="chat.loadOlder()"
            @read="chat.markActiveRead()"
          />
        </div>

        <p
          v-if="errorMessage"
          class="border-t border-red-200 bg-red-50 px-4 py-2 text-xs text-red-700 dark:border-red-900 dark:bg-red-950/50 dark:text-red-300"
          data-test="chat-error"
        >
          {{ errorMessage }}
        </p>

        <MessageComposer
          v-if="!(isGroup && showGroupInfo)"
          :disabled="!connected"
          @send="chat.sendMessage($event)"
        />
      </template>

      <div
        v-else
        class="m-auto max-w-sm text-center text-sm text-zinc-500"
        data-test="no-conversation"
      >
        <p class="text-base font-medium text-zinc-700 dark:text-zinc-300">
          选择一个会话
        </p>
        <p class="mt-1 leading-relaxed">
          在左侧输入对方用户名开始单聊，或使用「建群」创建群聊。
        </p>
      </div>
    </section>
  </main>
</template>
