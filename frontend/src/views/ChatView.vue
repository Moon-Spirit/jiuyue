<script setup lang="ts">
import { storeToRefs } from "pinia";
import { computed, onMounted, onUnmounted, ref } from "vue";
import { RouterLink, useRouter } from "vue-router";
import type { SocketStatus } from "../api/ws";
import ConversationList from "../components/chat/ConversationList.vue";
import MessageComposer from "../components/chat/MessageComposer.vue";
import MessageList from "../components/chat/MessageList.vue";
import { useAuthStore } from "../stores/auth";
import { useChatStore } from "../stores/chat";
import { useRealtimeStore } from "../stores/realtime";

const auth = useAuthStore();
const chat = useChatStore();
const realtime = useRealtimeStore();
const router = useRouter();

const { user } = storeToRefs(auth);
const {
  conversations,
  activeConversation,
  activeConversationId,
  messages,
  loadingConversations,
  loadingMessages,
  errorMessage,
  notice,
} = storeToRefs(chat);
const { status } = storeToRefs(realtime);

const newPeer = ref("");
const starting = ref(false);

const currentUserId = computed(() => user.value?.id ?? "");
const connected = computed(() => status.value === "open");
const socketLabel = computed(() => SOCKET_LABELS[status.value]);

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

onUnmounted(() => {
  realtime.disconnect();
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

async function signOut(): Promise<void> {
  realtime.disconnect();
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

      <ConversationList
        :conversations="conversations"
        :active-id="activeConversationId"
        :loading="loadingConversations"
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
      <template v-if="activeConversation !== null">
        <header
          class="border-b border-zinc-200 bg-white px-4 py-3 dark:border-zinc-800 dark:bg-zinc-900"
        >
          <h2 class="text-sm font-semibold" data-test="conversation-title">
            {{
              activeConversation.peer?.display_name ||
              activeConversation.peer?.username ||
              "群聊"
            }}
          </h2>
          <p class="text-xs text-zinc-500">
            @{{ activeConversation.peer?.username ?? "group" }}
          </p>
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
          <MessageList
            :messages="messages"
            :current-user-id="currentUserId"
            :loading="loadingMessages"
            @retry="chat.retry($event)"
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
          或者在左侧输入对方用户名，开始一个新的单聊。
        </p>
      </div>
    </section>
  </main>
</template>
