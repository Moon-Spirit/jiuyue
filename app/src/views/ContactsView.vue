<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import AppShell from "../components/layout/AppShell.vue";
import Avatar from "../components/Avatar.vue";
import LanguageToggle from "../components/LanguageToggle.vue";
import ThemeToggle from "../components/ThemeToggle.vue";
import { friendApiErrorMessage } from "../lib/api/friends";
import type { Friend, IncomingFriendRequest } from "../lib/api/friends";
import type { GroupInvite } from "../lib/api/groups";
import { groupApiErrorMessage } from "../lib/api/groups";
import type { UserSearchResult } from "../lib/api/users";
import { displayNameOf } from "../lib/identity";
import { useAuthStore } from "../stores/auth";
import { useFriendsStore } from "../stores/friends";
import { useGroupsStore } from "../stores/groups";
import { useProfileStore } from "../stores/profile";
import { useWsStore } from "../stores/ws";

const SEARCH_DEBOUNCE_MS = 400;

const { t } = useI18n();
const route = useRoute();
const router = useRouter();
const auth = useAuthStore();
const ws = useWsStore();
const friends = useFriendsStore();
const groups = useGroupsStore();
const profile = useProfileStore();

const searchQuery = ref("");
const searching = ref(false);
const searchError = ref("");
/** Whether the last search ran (distinguishes "untouched" from "no hits"). */
const searchDone = ref(false);
const searchResults = ref<UserSearchResult[]>([]);
/** uid of the result whose 添加 request is in flight. */
const sendingUid = ref<number | null>(null);
/** Incoming request currently being accepted/declined (disables its row). */
const busyRequestId = ref<string | null>(null);
/**
 * Two-step unfriend confirm: the id of the friend whose 删除 button has
 * been clicked once and now shows 确认删除 (null = no pending confirm).
 */
const pendingUnfriendId = ref<string | null>(null);
/** Username whose 发消息 navigation is in flight. */
const messagingUsername = ref<string | null>(null);
/** Group invite currently being accepted/declined (disables its row). */
const busyInviteId = ref<string | null>(null);

let searchTimer: number | null = null;

onMounted(() => {
  // Same pattern as ChatView: guards make this auth-only; connecting here
  // keeps friend badges live even when the user lands directly on /contacts.
  if (auth.status === "authed") {
    profile.ensureLoaded();
    void ws.connect();
    void friends.loadAll();
    void groups.loadInvites();
  }
});

onUnmounted(() => {
  if (searchTimer !== null) window.clearTimeout(searchTimer);
});

async function logout(): Promise<void> {
  ws.dispose();
  auth.logout();
  await router.push("/login");
}

// ---------------------------------------------------------------------
// Search by UID / username → result card → send request
// ---------------------------------------------------------------------

/** Open the public profile of a user (seed transient peer state first). */
function openUserProfile(user: {
  user_id: string;
  username: string;
  uid?: number;
  display_name?: string;
  avatar?: string | null;
}): void {
  profile.seedPeer(user.user_id, {
    username: user.username,
    uid: user.uid ?? 0,
    displayName: user.display_name ?? "",
    avatar: user.avatar ?? null,
  });
  void router.push(`/profile/${encodeURIComponent(user.user_id)}`);
}

function cancelPendingSearch(): void {
  if (searchTimer !== null) {
    window.clearTimeout(searchTimer);
    searchTimer = null;
  }
}

function onSearchInput(): void {
  // Debounced search: typing settles before a request goes out.
  cancelPendingSearch();
  searchTimer = window.setTimeout(() => {
    searchTimer = null;
    void runSearch();
  }, SEARCH_DEBOUNCE_MS);
}

async function runSearch(): Promise<void> {
  cancelPendingSearch();
  const query = searchQuery.value.trim();
  if (query.length === 0) {
    searchResults.value = [];
    searchDone.value = false;
    searchError.value = "";
    return;
  }
  if (searching.value) return;
  searching.value = true;
  searchError.value = "";
  try {
    searchResults.value = await friends.search(query);
    searchDone.value = true;
  } catch (error) {
    searchResults.value = [];
    searchDone.value = true;
    searchError.value = friendApiErrorMessage(error, (key) => t(key));
  } finally {
    searching.value = false;
  }
}

/** Search hit already an established friend → row renders 已添加. */
function isFriendResult(user: UserSearchResult): boolean {
  return friends.friends.some(
    (f) => f.user_id === user.user_id || f.uid === user.uid,
  );
}

/** Search hit with an outgoing request already pending → row renders 已发送. */
function isPendingResult(user: UserSearchResult): boolean {
  return friends.outgoing.some(
    (r) => r.to.user_id === user.user_id || r.to.username === user.username,
  );
}

async function sendRequestTo(user: UserSearchResult): Promise<void> {
  if (sendingUid.value !== null) return;
  sendingUid.value = user.uid;
  searchError.value = "";
  try {
    // username-add remains the wire contract; UID search only locates the user.
    await friends.sendRequest(user.username);
  } catch (error) {
    searchError.value = friendApiErrorMessage(error, (key) => t(key));
  } finally {
    sendingUid.value = null;
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
// Group invitations
// ---------------------------------------------------------------------

async function acceptGroupInvite(invite: GroupInvite): Promise<void> {
  if (busyInviteId.value !== null) return;
  busyInviteId.value = invite.invite_id;
  try {
    const conversationId = await groups.acceptInvite(invite.invite_id);
    if (conversationId !== null) await router.push("/chat");
  } catch (error) {
    searchError.value = groupApiErrorMessage(error, (key) => t(key));
  } finally {
    busyInviteId.value = null;
  }
}

async function declineGroupInvite(invite: GroupInvite): Promise<void> {
  if (busyInviteId.value !== null) return;
  busyInviteId.value = invite.invite_id;
  try {
    await groups.declineInvite(invite.invite_id);
  } finally {
    busyInviteId.value = null;
  }
}

// ---------------------------------------------------------------------
// Friend list actions
// ---------------------------------------------------------------------

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
    searchError.value = friendApiErrorMessage(error, (key) => t(key));
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
        <button
          type="button"
          class="ml-auto flex items-center gap-1.5 rounded-full p-0.5 hover:bg-neutral-100 dark:hover:bg-neutral-800 lg:ml-0 lg:flex-col lg:gap-0.5 lg:p-1"
          data-testid="nav-self"
          :title="t('nav.profile')"
          @click="router.push('/profile')"
        >
          <Avatar
            :username="auth.user?.username ?? ''"
            :display-name="auth.user?.displayName"
            :avatar="auth.user?.avatar"
            :size="28"
          />
          <span
            class="max-w-[6rem] truncate text-[10px] text-neutral-500 dark:text-neutral-400"
            data-testid="nav-username"
          >
            {{ auth.user?.username ?? t("nav.anonymous") }}
          </span>
        </button>
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
            v-if="friends.pendingCount + groups.pendingInviteCount > 0"
            data-testid="nav-contacts-badge"
            class="flex h-4 min-w-4 items-center justify-center rounded-full bg-indigo-600 px-1 text-[10px] font-semibold text-white"
          >
            {{ friends.pendingCount + groups.pendingInviteCount }}
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

        <!-- Search by UID or username -->
        <div class="pb-2">
          <div class="relative">
            <input
              v-model="searchQuery"
              type="search"
              data-testid="user-search-input"
              :placeholder="t('contacts.searchPlaceholder')"
              class="w-full rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 pr-8 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
              @input="onSearchInput()"
              @keydown.enter.prevent="runSearch()"
            />
            <span
              v-if="searching"
              class="absolute right-2 top-1/2 -translate-y-1/2 animate-pulse text-xs text-neutral-400"
              data-testid="user-search-spinner"
              >…</span
            >
          </div>
          <p
            v-if="searchError.length > 0"
            class="px-1 pt-2 text-xs text-red-500"
            data-testid="search-error"
          >
            {{ searchError }}
          </p>

          <!-- Search result cards -->
          <div
            v-if="searchDone && searchQuery.trim().length > 0"
            class="pt-1"
            data-testid="search-results"
          >
            <p
              v-if="searchResults.length === 0"
              class="px-1 py-2 text-xs text-neutral-400 dark:text-neutral-500"
              data-testid="search-empty"
            >
              {{ t("contacts.searchEmpty") }}
            </p>
            <ul v-else class="space-y-1">
              <li
                v-for="user in searchResults"
                :key="user.user_id"
                data-testid="search-result-item"
                :data-user-id="user.user_id"
                class="flex items-center gap-2 rounded-lg p-2 hover:bg-neutral-100 dark:hover:bg-neutral-800"
              >
                <Avatar
                  :username="user.username"
                  :display-name="user.display_name"
                  :avatar="user.avatar"
                  :size="32"
                  data-testid="search-result-avatar"
                  @click="openUserProfile(user)"
                />
                <span class="min-w-0 flex-1">
                  <span
                    class="block truncate text-sm font-medium"
                    data-testid="search-result-name"
                    >{{ displayNameOf(user) }}</span
                  >
                  <span
                    class="block font-mono text-[11px] text-neutral-400 dark:text-neutral-500"
                    data-testid="search-result-uid"
                    >{{ t("contacts.uidLabel") }}: {{ user.uid }}</span
                  >
                </span>
                <button
                  v-if="isFriendResult(user)"
                  type="button"
                  disabled
                  data-testid="search-result-already-friend"
                  class="shrink-0 cursor-not-allowed rounded-lg bg-neutral-200 px-2 py-1 text-[11px] font-medium text-neutral-500 dark:bg-neutral-800 dark:text-neutral-400"
                >
                  {{ t("contacts.alreadyFriend") }}
                </button>
                <button
                  v-else-if="isPendingResult(user)"
                  type="button"
                  disabled
                  data-testid="search-result-sent"
                  class="shrink-0 cursor-not-allowed rounded-lg bg-neutral-200 px-2 py-1 text-[11px] font-medium text-neutral-500 dark:bg-neutral-800 dark:text-neutral-400"
                >
                  {{ t("contacts.requestSent") }}
                </button>
                <button
                  v-else
                  type="button"
                  data-testid="search-result-add"
                  :disabled="sendingUid !== null"
                  class="shrink-0 rounded-lg bg-indigo-600 px-2 py-1 text-[11px] font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
                  @click="sendRequestTo(user)"
                >
                  {{ t("contacts.addFriend") }}
                </button>
              </li>
            </ul>
          </div>
        </div>

        <!-- Pending group invitations -->
        <div
          v-if="groups.invites.length > 0"
          class="pb-2"
          data-testid="group-invites"
        >
          <h3
            class="px-1 pb-1 text-xs font-semibold text-neutral-500 dark:text-neutral-400"
            data-testid="group-invites-title"
          >
            {{ t("contacts.groupInvitesTitle") }}
          </h3>
          <ul class="space-y-1" data-testid="group-invite-list">
            <li
              v-for="invite in groups.invites"
              :key="invite.invite_id"
              data-testid="group-invite-item"
              :data-invite-id="invite.invite_id"
              class="flex items-center gap-2 rounded-lg p-2 hover:bg-neutral-100 dark:hover:bg-neutral-800"
            >
              <Avatar
                class="shrink-0"
                group
                :username="invite.group_name"
                :size="32"
                data-testid="group-invite-avatar"
              />
              <span class="min-w-0 flex-1">
                <span
                  class="block truncate text-sm font-medium"
                  data-testid="group-invite-name"
                  >{{ invite.group_name }}</span
                >
                <span
                  class="block truncate text-[11px] text-neutral-400 dark:text-neutral-500"
                  data-testid="group-invite-from"
                  >{{
                    t("contacts.groupInviteFrom", {
                      name: displayNameOf(invite.from),
                    })
                  }}</span
                >
              </span>
              <button
                type="button"
                data-testid="group-invite-accept"
                :disabled="busyInviteId !== null"
                class="shrink-0 rounded-lg bg-emerald-600 px-2 py-1 text-[11px] font-medium text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
                @click="acceptGroupInvite(invite)"
              >
                {{ t("contacts.groupInviteAccept") }}
              </button>
              <button
                type="button"
                data-testid="group-invite-decline"
                :disabled="busyInviteId !== null"
                class="shrink-0 rounded-lg border border-neutral-300 px-2 py-1 text-[11px] font-medium text-neutral-600 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                @click="declineGroupInvite(invite)"
              >
                {{ t("contacts.groupInviteDecline") }}
              </button>
            </li>
          </ul>
        </div>

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
              <Avatar
                :username="request.from.username"
                :display-name="request.from.display_name"
                :avatar="request.from.avatar"
                :size="32"
                data-testid="incoming-avatar"
                @click="openUserProfile(request.from)"
              />
              <span
                class="min-w-0 flex-1 truncate text-sm font-medium"
                data-testid="incoming-name"
              >
                {{ displayNameOf(request.from) }}
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
            <Avatar
              :username="friend.username"
              :display-name="friend.display_name"
              :avatar="friend.avatar"
              :size="36"
              data-testid="friend-avatar"
              @click="openUserProfile(friend)"
            />
            <span class="min-w-0 flex-1">
              <span
                class="block truncate text-sm font-medium"
                data-testid="friend-name"
                >{{ displayNameOf(friend) }}</span
              >
              <span
                v-if="typeof friend.uid === 'number' && friend.uid > 0"
                class="block font-mono text-[11px] text-neutral-400 dark:text-neutral-500"
                data-testid="friend-uid"
                >{{ t("contacts.uidLabel") }}: {{ friend.uid }}</span
              >
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
          v-else-if="friends.loaded"
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
