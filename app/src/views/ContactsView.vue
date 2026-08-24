<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import AppShell from "../components/layout/AppShell.vue";
import LanguageToggle from "../components/LanguageToggle.vue";
import ThemeToggle from "../components/ThemeToggle.vue";
import { friendApiErrorMessage } from "../lib/api/friends";
import type { Friend, IncomingFriendRequest } from "../lib/api/friends";
import { useAuthStore } from "../stores/auth";
import { useFriendsStore } from "../stores/friends";
import { useWsStore } from "../stores/ws";

const { t } = useI18n();
const route = useRoute();
const router = useRouter();
const auth = useAuthStore();
const ws = useWsStore();
const friends = useFriendsStore();

const newFriendUsername = ref("");
const sendingRequest = ref(false);
const requestError = ref("");
/** Incoming request currently being accepted/declined (disables its row). */
const busyRequestId = ref<string | null>(null);
/**
 * Two-step unfriend confirm: the id of the friend whose 删除 button has
 * been clicked once and now shows 确认删除 (null = no pending confirm).
 */
const pendingUnfriendId = ref<string | null>(null);
/** Username whose 发消息 navigation is in flight. */
const messagingUsername = ref<string | null>(null);

onMounted(() => {
  // Same pattern as ChatView: guards make this auth-only; connecting here
  // keeps friend badges live even when the user lands directly on /contacts.
  if (auth.status === "authed") {
    void ws.connect();
    void friends.loadAll();
  }
});

async function logout(): Promise<void> {
  ws.dispose();
  auth.logout();
  await router.push("/login");
}

// ---------------------------------------------------------------------
// Add friend
// ---------------------------------------------------------------------

async function submitFriendRequest(): Promise<void> {
  const username = newFriendUsername.value.trim();
  if (username.length === 0 || sendingRequest.value) return;
  sendingRequest.value = true;
  requestError.value = "";
  try {
    await friends.sendRequest(username);
    newFriendUsername.value = "";
  } catch (error) {
    requestError.value = friendApiErrorMessage(error, (key) => t(key));
  } finally {
    sendingRequest.value = false;
  }
}

// ---------------------------------------------------------------------
// Incoming requests
// ---------------------------------------------------------------------

async function acceptRequest(request: IncomingFriendRequest): Promise<void> {
  if (busyRequestId.value !== null) return;
  busyRequestId.value = request.request_id;
  try {
    await friends.accept(request.request_id);
  } finally {
    busyRequestId.value = null;
  }
}

async function declineRequest(request: IncomingFriendRequest): Promise<void> {
  if (busyRequestId.value !== null) return;
  busyRequestId.value = request.request_id;
  try {
    await friends.decline(request.request_id);
  } finally {
    busyRequestId.value = null;
  }
}

// ---------------------------------------------------------------------
// Friend list actions
// ---------------------------------------------------------------------

function initialOf(username: string): string {
  const first = username.trim().charAt(0);
  return first.length === 0 ? "?" : first.toUpperCase();
}

/**
 * 发消息: create/open a direct conversation with the friend through the ws
 * store (which also selects it as the active conversation), then jump to
 * /chat where the thread is already open.
 */
async function messageFriend(friend: Friend): Promise<void> {
  if (messagingUsername.value !== null) return;
  messagingUsername.value = friend.username;
  try {
    await ws.createOrOpenConversation(friend.username);
    await router.push("/chat");
  } catch (error) {
    requestError.value = friendApiErrorMessage(error, (key) => t(key));
  } finally {
    messagingUsername.value = null;
  }
}

function askUnfriend(friend: Friend): void {
  pendingUnfriendId.value = friend.user_id;
}

async function confirmUnfriend(friend: Friend): Promise<void> {
  pendingUnfriendId.value = null;
  await friends.unfriend(friend.user_id);
}
</script>

<template>
  <AppShell>
    <template #nav>
      <div
        class="flex w-full items-center gap-2 lg:flex-col lg:gap-2 lg:px-1 lg:py-1"
        data-testid="contacts-nav"
      >
        <span class="text-sm font-bold tracking-tight lg:text-[11px]">{{
          t("app.name")
        }}</span>
        <span
          class="ml-auto min-w-0 truncate text-xs text-neutral-500 lg:ml-0 lg:max-w-full lg:text-center dark:text-neutral-400"
          data-testid="nav-username"
          :title="auth.user?.username ?? t('nav.anonymous')"
        >
          {{ auth.user?.username ?? t("nav.anonymous") }}
        </span>
        <router-link
          to="/chat"
          data-testid="nav-chat"
          class="inline-flex h-9 items-center justify-center whitespace-nowrap rounded-lg px-2 text-xs font-medium text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 lg:w-full dark:text-neutral-300 dark:hover:bg-neutral-800 dark:hover:text-white"
          :class="{
            'bg-neutral-100 text-neutral-900 dark:bg-neutral-800 dark:text-white':
              route.name === 'chat',
          }"
        >
          {{ t("nav.chat") }}
        </router-link>
        <router-link
          to="/contacts"
          data-testid="nav-contacts"
          class="relative inline-flex h-9 items-center justify-center gap-1 whitespace-nowrap rounded-lg px-2 text-xs font-medium text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 lg:w-full dark:text-neutral-300 dark:hover:bg-neutral-800 dark:hover:text-white"
          :class="{
            'bg-neutral-100 text-neutral-900 dark:bg-neutral-800 dark:text-white':
              route.name === 'contacts',
          }"
        >
          {{ t("nav.contacts") }}
          <span
            v-if="friends.pendingCount > 0"
            data-testid="nav-contacts-badge"
            class="flex h-4 min-w-4 items-center justify-center rounded-full bg-indigo-600 px-1 text-[10px] font-semibold text-white"
          >
            {{ friends.pendingCount }}
          </span>
        </router-link>
        <ThemeToggle />
        <LanguageToggle />
        <button
          type="button"
          data-testid="logout-button"
          class="inline-flex h-9 items-center justify-center whitespace-nowrap rounded-lg px-2 text-xs font-medium text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 lg:w-full dark:text-neutral-300 dark:hover:bg-neutral-800 dark:hover:text-white"
          @click="logout()"
        >
          {{ t("nav.logout") }}
        </button>
      </div>
    </template>

    <template #sessions>
      <div class="flex h-full flex-col p-3" data-testid="contacts-panel">
        <h2
          class="px-1 pb-2 text-sm font-semibold text-neutral-500 dark:text-neutral-400"
          data-testid="contacts-title"
        >
          {{ t("contacts.title") }}
        </h2>

        <!-- Add friend row -->
        <form
          class="flex items-center gap-2 pb-2"
          @submit.prevent="submitFriendRequest()"
        >
          <input
            v-model="newFriendUsername"
            type="text"
            data-testid="add-friend-input"
            :placeholder="t('contacts.addPlaceholder')"
            class="min-w-0 flex-1 rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
          />
          <button
            type="submit"
            data-testid="add-friend-submit"
            :disabled="sendingRequest || newFriendUsername.trim().length === 0"
            class="shrink-0 rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {{
              sendingRequest
                ? t("contacts.addSubmitting")
                : t("contacts.addSubmit")
            }}
          </button>
        </form>
        <p
          v-if="requestError.length > 0"
          class="px-1 pb-2 text-xs text-red-500"
          data-testid="add-friend-error"
        >
          {{ requestError }}
        </p>

        <!-- Incoming friend requests -->
        <div v-if="friends.incoming.length > 0" class="pb-2">
          <h3
            class="px-1 pb-1 text-xs font-semibold text-neutral-500 dark:text-neutral-400"
            data-testid="incoming-title"
          >
            {{ t("contacts.requestsTitle") }}
          </h3>
          <ul class="space-y-1" data-testid="incoming-list">
            <li
              v-for="request in friends.incoming"
              :key="request.request_id"
              data-testid="incoming-item"
              class="flex items-center gap-2 rounded-lg p-2 hover:bg-neutral-100 dark:hover:bg-neutral-800"
            >
              <span
                class="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-indigo-100 text-xs font-semibold text-indigo-700 dark:bg-indigo-900 dark:text-indigo-200"
                data-testid="incoming-avatar"
              >
                {{ initialOf(request.from.username) }}
              </span>
              <span
                class="min-w-0 flex-1 truncate text-sm font-medium"
                data-testid="incoming-name"
              >
                {{ request.from.username }}
              </span>
              <button
                type="button"
                data-testid="accept-request"
                :disabled="busyRequestId !== null"
                class="shrink-0 rounded-lg bg-emerald-600 px-2 py-1 text-[11px] font-medium text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
                @click="acceptRequest(request)"
              >
                {{ t("contacts.accept") }}
              </button>
              <button
                type="button"
                data-testid="decline-request"
                :disabled="busyRequestId !== null"
                class="shrink-0 rounded-lg border border-neutral-300 px-2 py-1 text-[11px] font-medium text-neutral-600 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                @click="declineRequest(request)"
              >
                {{ t("contacts.decline") }}
              </button>
            </li>
          </ul>
        </div>

        <!-- Friend list -->
        <ul
          v-if="friends.friends.length > 0"
          class="space-y-1"
          data-testid="friend-list"
        >
          <li
            v-for="friend in friends.friends"
            :key="friend.user_id"
            data-testid="friend-item"
            :data-user-id="friend.user_id"
            class="group flex items-center gap-3 rounded-lg p-2 hover:bg-neutral-100 dark:hover:bg-neutral-800"
          >
            <span
              class="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-indigo-100 text-sm font-semibold text-indigo-700 dark:bg-indigo-900 dark:text-indigo-200"
              data-testid="friend-avatar"
            >
              {{ initialOf(friend.username) }}
            </span>
            <span
              class="min-w-0 flex-1 truncate text-sm font-medium"
              data-testid="friend-name"
            >
              {{ friend.username }}
            </span>
            <span class="flex shrink-0 items-center gap-1">
              <button
                type="button"
                data-testid="message-friend"
                :disabled="messagingUsername !== null"
                class="rounded-lg bg-indigo-600 px-2 py-1 text-[11px] font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
                @click="messageFriend(friend)"
              >
                {{ t("contacts.sendMessage") }}
              </button>
              <button
                v-if="pendingUnfriendId !== friend.user_id"
                type="button"
                data-testid="unfriend-friend"
                class="rounded-lg px-2 py-1 text-[11px] font-medium text-neutral-500 opacity-0 transition-opacity hover:text-red-500 focus:opacity-100 group-hover:opacity-100 dark:text-neutral-400"
                @click="askUnfriend(friend)"
              >
                {{ t("contacts.unfriend") }}
              </button>
              <button
                v-else
                type="button"
                data-testid="confirm-unfriend"
                class="rounded-lg bg-red-600 px-2 py-1 text-[11px] font-medium text-white hover:bg-red-500"
                @click="confirmUnfriend(friend)"
              >
                {{ t("contacts.unfriendConfirm") }}
              </button>
            </span>
          </li>
        </ul>
        <p
          v-else
          class="px-1 pt-2 text-xs text-neutral-400 dark:text-neutral-500"
          data-testid="friends-empty"
        >
          {{ t("contacts.friendsEmpty") }}
        </p>
      </div>
    </template>

    <template #main>
      <!-- Default detail pane: pick a friend to act on -->
      <div class="flex h-full items-center justify-center">
        <p
          class="text-sm text-neutral-400 dark:text-neutral-500"
          data-testid="contacts-empty-state"
        >
          {{ t("contacts.emptyState") }}
        </p>
      </div>
    </template>
  </AppShell>
</template>
