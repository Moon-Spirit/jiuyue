<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import AppShell from "../components/layout/AppShell.vue";
import LanguageToggle from "../components/LanguageToggle.vue";
import ThemeToggle from "../components/ThemeToggle.vue";
import { apiErrorMessage } from "../lib/api/messages";
import * as olm from "../lib/crypto/olm-lite";
import { useAuthStore } from "../stores/auth";
import { RECALL_WINDOW_MS, useWsStore } from "../stores/ws";
import type { ChatMessage, Conversation } from "../stores/ws";

const { t, locale } = useI18n();
const router = useRouter();
const auth = useAuthStore();
const ws = useWsStore();

const PREVIEW_MAX_CHARS = 40;

const newPeerUsername = ref("");
/** M3: kind chosen in the segmented 普通/密聊 control. */
const newConversationKind = ref<"direct" | "secret">("direct");
const creatingConversation = ref(false);
const conversationError = ref("");
const draft = ref("");
/** Local half of the safety code for the active secret thread (or null). */
const sasCode = ref<string | null>(null);
const composerEl = ref<HTMLTextAreaElement | null>(null);
const messagesEndRef = ref<HTMLElement | null>(null);

onMounted(() => {
  // Router guards ensure /chat is only reachable while authed; connecting
  // here keeps tests (anon auth) free of network side effects.
  if (auth.status === "authed") void ws.connect();
});

watch(
  () => [ws.activeConversationId, ws.activeMessages.length] as const,
  async () => {
    await nextTick();
    // Optional call: jsdom (tests) does not implement scrollIntoView.
    messagesEndRef.value?.scrollIntoView?.({ block: "end" });
  },
);

async function logout(): Promise<void> {
  ws.dispose();
  auth.logout();
  await router.push("/login");
}

// ---------------------------------------------------------------------
// Sessions pane
// ---------------------------------------------------------------------

function initialOf(username: string): string {
  const first = username.trim().charAt(0);
  return first.length === 0 ? "?" : first.toUpperCase();
}

function displayName(conversation: Conversation): string {
  return conversation.peerUsername.length > 0
    ? conversation.peerUsername
    : t("chat.peerUnknown");
}

function truncatePreview(body: string | null): string {
  if (body === null) return "";
  return body.length > PREVIEW_MAX_CHARS
    ? `${body.slice(0, PREVIEW_MAX_CHARS)}…`
    : body;
}

const relativeFormat = computed(
  () => new Intl.RelativeTimeFormat(locale.value, { numeric: "auto" }),
);

function relativeTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "";
  const diffSecs = Math.round((then - Date.now()) / 1000);
  const absSecs = Math.abs(diffSecs);
  if (absSecs < 60) return relativeFormat.value.format(diffSecs, "second");
  if (absSecs < 3600) {
    return relativeFormat.value.format(Math.round(diffSecs / 60), "minute");
  }
  if (absSecs < 86_400) {
    return relativeFormat.value.format(Math.round(diffSecs / 3600), "hour");
  }
  if (absSecs < 7 * 86_400) {
    return relativeFormat.value.format(Math.round(diffSecs / 86_400), "day");
  }
  return new Intl.DateTimeFormat(locale.value, {
    month: "short",
    day: "numeric",
  }).format(new Date(then));
}

async function submitNewConversation(): Promise<void> {
  const peer = newPeerUsername.value.trim();
  if (peer.length === 0 || creatingConversation.value) return;
  creatingConversation.value = true;
  conversationError.value = "";
  try {
    await ws.createOrOpenConversation(peer, newConversationKind.value);
    newPeerUsername.value = "";
    newConversationKind.value = "direct";
  } catch (error) {
    conversationError.value = apiErrorMessage(error, (key) => t(key));
  } finally {
    creatingConversation.value = false;
  }
}

// ---------------------------------------------------------------------
// M3 secret chats: safety code for the active thread
// ---------------------------------------------------------------------

watch(
  () => [ws.activeConversationId, ws.activeConversation?.kind] as const,
  async () => {
    sasCode.value = null;
    const conversation = ws.activeConversation;
    if (conversation?.kind !== "secret") return;
    const peerIdentity = olm.getPeerIdentity(conversation.conversationId);
    if (peerIdentity === null) return;
    try {
      sasCode.value = await olm.safetyCode(peerIdentity);
    } catch {
      sasCode.value = null;
    }
  },
  { immediate: true },
);

function openConversation(conversationId: number): void {
  // Resets unread and advances lastSeenSeq inside the store.
  ws.openConversation(conversationId);
}

// ---------------------------------------------------------------------
// Message thread
// ---------------------------------------------------------------------

function dayKey(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "unknown";
  return `${date.getFullYear()}-${date.getMonth() + 1}-${date.getDate()}`;
}

function dayLabel(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  const key = dayKey(iso);
  if (key === dayKey(new Date().toISOString())) return t("chat.dateToday");
  if (key === dayKey(new Date(Date.now() - 86_400_000).toISOString())) {
    return t("chat.dateYesterday");
  }
  return new Intl.DateTimeFormat(locale.value, {
    year: "numeric",
    month: "long",
    day: "numeric",
  }).format(date);
}

interface DayGroup {
  key: string;
  label: string;
  messages: ChatMessage[];
}

const dayGroups = computed<DayGroup[]>(() => {
  const groups: DayGroup[] = [];
  for (const message of ws.activeMessages) {
    const key = dayKey(message.sentAt);
    const last = groups[groups.length - 1];
    if (last !== undefined && last.key === key) {
      last.messages.push(message);
    } else {
      groups.push({
        key,
        label: dayLabel(message.sentAt),
        messages: [message],
      });
    }
  }
  return groups;
});

function clockTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return new Intl.DateTimeFormat(locale.value, {
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}

// ---------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------

function autoGrow(): void {
  const el = composerEl.value;
  if (el === null) return;
  el.style.height = "auto";
  el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
}

function sendMessage(): void {
  const conversationId = ws.activeConversationId;
  if (conversationId === null || !ws.isConnected) return;
  const body = draft.value.trim();
  if (body.length === 0) return;
  ws.notifyTypingStop(conversationId);
  ws.send(conversationId, body);
  draft.value = "";
  autoGrow();
}

function retryMessage(clientMsgId: string): void {
  ws.retry(clientMsgId);
}

// ---------------------------------------------------------------------
// M2: reply / forward / recall / typing
// ---------------------------------------------------------------------

/** Forward picker source message; null = picker closed. */
const forwardSource = ref<ChatMessage | null>(null);

/** Picker lists every OTHER conversation (no self-target). */
const forwardTargets = computed(() =>
  ws.sortedConversations.filter(
    (c) => c.conversationId !== forwardSource.value?.conversationId,
  ),
);

/** Sender-only, 120s window (mirrors the server-side RecallPolicy). */
function canRecall(message: ChatMessage): boolean {
  if (!message.mine || message.recalled || message.messageId === null) {
    return false;
  }
  const sentAt = Date.parse(message.sentAt);
  return Number.isFinite(sentAt) && Date.now() - sentAt < RECALL_WINDOW_MS;
}

function replyToMessage(message: ChatMessage): void {
  ws.setReplyContext(message);
}

function cancelReply(): void {
  ws.clearReplyContext();
}

function openForwardPicker(message: ChatMessage): void {
  if (!message.recalled) forwardSource.value = message;
}

function confirmForward(conversationId: number): void {
  const source = forwardSource.value;
  forwardSource.value = null;
  if (source === null) return;
  ws.forwardMessage(conversationId, source);
}

function recallMessage(message: ChatMessage): void {
  if (canRecall(message)) ws.recallMessage(message.conversationId, message);
}

function onComposerInput(): void {
  autoGrow();
  const conversationId = ws.activeConversationId;
  if (conversationId !== null && ws.isConnected) {
    ws.notifyTypingStart(conversationId);
  }
}

function onComposerBlur(): void {
  const conversationId = ws.activeConversationId;
  if (conversationId !== null) ws.notifyTypingStop(conversationId);
}
</script>

<template>
  <AppShell>
    <template #nav>
      <div
        class="flex w-full items-center gap-2 lg:flex-col lg:gap-2 lg:px-1 lg:py-1"
        data-testid="chat-nav"
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
      <div class="flex h-full flex-col p-3">
        <h2
          class="px-1 pb-2 text-sm font-semibold text-neutral-500 dark:text-neutral-400"
          data-testid="sessions-title"
        >
          {{ t("chat.sessionsTitle") }}
        </h2>

        <!-- New conversation row -->
        <form
          class="flex items-center gap-2 pb-2"
          @submit.prevent="submitNewConversation()"
        >
          <input
            v-model="newPeerUsername"
            type="text"
            data-testid="new-conversation-input"
            :placeholder="t('chat.newConversationPlaceholder')"
            class="min-w-0 flex-1 rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
          />
          <!-- M3: conversation kind segmented control (普通 / 密聊) -->
          <div
            class="flex shrink-0 overflow-hidden rounded-lg border border-neutral-300 text-xs dark:border-neutral-700"
            data-testid="kind-toggle"
          >
            <button
              type="button"
              data-testid="kind-direct"
              class="px-2 py-1.5 font-medium transition-colors"
              :class="
                newConversationKind === 'direct'
                  ? 'bg-indigo-600 text-white'
                  : 'text-neutral-500 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-800'
              "
              @click="newConversationKind = 'direct'"
            >
              {{ t("chat.kindDirect") }}
            </button>
            <button
              type="button"
              data-testid="kind-secret"
              class="border-l border-neutral-300 px-2 py-1.5 font-medium transition-colors dark:border-neutral-700"
              :class="
                newConversationKind === 'secret'
                  ? 'bg-emerald-600 text-white'
                  : 'text-neutral-500 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-800'
              "
              @click="newConversationKind = 'secret'"
            >
              {{ t("chat.kindSecret") }}
            </button>
          </div>
          <button
            type="submit"
            data-testid="new-conversation-submit"
            :disabled="
              creatingConversation || newPeerUsername.trim().length === 0
            "
            class="shrink-0 rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {{
              creatingConversation
                ? t("chat.newConversationCreating")
                : t("chat.newConversationSubmit")
            }}
          </button>
        </form>
        <p
          v-if="conversationError.length > 0"
          class="px-1 pb-2 text-xs text-red-500"
          data-testid="new-conversation-error"
        >
          {{ conversationError }}
        </p>

        <!-- Conversation list -->
        <ul
          v-if="ws.sortedConversations.length > 0"
          class="space-y-1"
          data-testid="session-list"
        >
          <li
            v-for="conversation in ws.sortedConversations"
            :key="conversation.conversationId"
            data-testid="session-item"
            :data-conversation-id="conversation.conversationId"
            class="flex cursor-pointer items-center gap-3 rounded-lg p-2 hover:bg-neutral-100 dark:hover:bg-neutral-800"
            :class="{
              'bg-neutral-100 dark:bg-neutral-800':
                ws.activeConversationId === conversation.conversationId,
            }"
            @click="openConversation(conversation.conversationId)"
          >
            <span
              class="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-indigo-100 text-sm font-semibold text-indigo-700 dark:bg-indigo-900 dark:text-indigo-200"
              data-testid="session-avatar"
            >
              {{ initialOf(conversation.peerUsername) }}
            </span>
            <span class="min-w-0 flex-1">
              <span class="flex items-baseline justify-between gap-2">
                <span
                  class="flex min-w-0 items-center gap-1 text-sm font-medium"
                >
                  <svg
                    v-if="conversation.kind === 'secret'"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    class="h-3 w-3 shrink-0 text-emerald-600 dark:text-emerald-400"
                    data-testid="session-lock"
                    aria-hidden="true"
                  >
                    <rect x="5" y="11" width="14" height="9" rx="2" />
                    <path d="M8 11V7a4 4 0 0 1 8 0v4" />
                  </svg>
                  <span class="min-w-0 truncate" data-testid="session-name">{{
                    displayName(conversation)
                  }}</span>
                </span>
                <span
                  class="shrink-0 text-[11px] text-neutral-400 dark:text-neutral-500"
                  data-testid="session-time"
                  >{{ relativeTime(conversation.lastActivityAt) }}</span
                >
              </span>
              <span class="flex items-center justify-between gap-2">
                <span
                  class="min-w-0 truncate text-xs text-neutral-500 dark:text-neutral-400"
                  data-testid="session-preview"
                  >{{ truncatePreview(conversation.lastMessagePreview) }}</span
                >
                <span
                  v-if="conversation.unread > 0"
                  class="flex h-5 min-w-5 shrink-0 items-center justify-center rounded-full bg-indigo-600 px-1.5 text-[11px] font-semibold text-white"
                  data-testid="unread-badge"
                  >{{ conversation.unread }}</span
                >
              </span>
            </span>
          </li>
        </ul>
        <p
          v-else
          class="px-1 pt-2 text-xs text-neutral-400 dark:text-neutral-500"
          data-testid="sessions-empty"
        >
          {{ t("chat.sessionsEmpty") }}
        </p>
      </div>
    </template>

    <template #main>
      <!-- Empty state: no conversation selected -->
      <div
        v-if="ws.activeConversation === null"
        class="flex h-full items-center justify-center"
      >
        <p
          class="text-sm text-neutral-400 dark:text-neutral-500"
          data-testid="chat-empty-state"
        >
          {{ t("chat.emptyState") }}
        </p>
      </div>

      <div v-else class="flex h-full min-h-0 flex-col">
        <!-- Disconnected banner -->
        <div
          v-if="!ws.isConnected"
          class="flex shrink-0 items-center justify-center bg-amber-100 py-1.5 text-xs font-medium text-amber-800 dark:bg-amber-900/40 dark:text-amber-200"
          data-testid="ws-banner"
        >
          {{ t("chat.reconnectingBanner") }}
        </div>
        <button
          v-if="ws.lastError !== null"
          type="button"
          class="shrink-0 bg-red-100 py-1.5 text-xs font-medium text-red-700 dark:bg-red-900/40 dark:text-red-200"
          data-testid="ws-error"
          @click="ws.lastError = null"
        >
          {{ t("chat.wsError") }}
        </button>

        <!-- M3 secret chat banner: device-bound, never synced elsewhere -->
        <div
          v-if="ws.activeConversation?.kind === 'secret'"
          class="flex shrink-0 items-center justify-center gap-1.5 bg-emerald-100 py-1.5 text-xs font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-200"
          data-testid="secret-banner"
        >
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="h-3 w-3 shrink-0"
            aria-hidden="true"
          >
            <rect x="5" y="11" width="14" height="9" rx="2" />
            <path d="M8 11V7a4 4 0 0 1 8 0v4" />
          </svg>
          {{ t("chat.secretBanner") }}
        </div>

        <!-- Thread header -->
        <header
          class="flex shrink-0 items-center gap-2 border-b border-neutral-200 px-4 py-2 dark:border-neutral-800"
        >
          <span
            class="flex h-8 w-8 items-center justify-center rounded-full bg-indigo-100 text-xs font-semibold text-indigo-700 dark:bg-indigo-900 dark:text-indigo-200"
          >
            {{ initialOf(ws.activeConversation.peerUsername) }}
          </span>
          <span class="text-sm font-semibold" data-testid="thread-title">
            {{ displayName(ws.activeConversation) }}
          </span>
          <svg
            v-if="ws.activeConversation.kind === 'secret'"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="h-3.5 w-3.5 shrink-0 text-emerald-600 dark:text-emerald-400"
            data-testid="thread-lock"
            aria-hidden="true"
          >
            <rect x="5" y="11" width="14" height="9" rx="2" />
            <path d="M8 11V7a4 4 0 0 1 8 0v4" />
          </svg>
        </header>

        <!-- M3 safety code row: compare with the peer out-of-band -->
        <div
          v-if="ws.activeConversation?.kind === 'secret'"
          class="flex shrink-0 flex-wrap items-center gap-x-2 gap-y-0.5 border-b border-neutral-200 px-4 py-1.5 text-[11px] dark:border-neutral-800"
          data-testid="sas-row"
        >
          <span class="font-medium text-neutral-500 dark:text-neutral-400">
            {{ t("chat.sasLabel") }}
          </span>
          <code
            class="font-mono text-xs font-semibold tracking-widest text-emerald-700 dark:text-emerald-300"
            data-testid="sas-code"
          >
            {{ sasCode ?? t("chat.sasPending") }}
          </code>
          <span class="text-neutral-400 dark:text-neutral-500">
            {{ t("chat.sasHint") }}
          </span>
        </div>

        <!-- Message list -->
        <div
          class="min-h-0 flex-1 overflow-y-auto px-4 py-3"
          data-testid="message-list"
        >
          <p
            v-if="dayGroups.length === 0"
            class="pt-6 text-center text-xs text-neutral-400 dark:text-neutral-500"
            data-testid="thread-empty"
          >
            {{ t("chat.emptyState") }}
          </p>
          <template v-for="group in dayGroups" :key="group.key">
            <div class="my-3 flex justify-center">
              <span
                class="rounded-full bg-neutral-100 px-3 py-0.5 text-[11px] text-neutral-500 dark:bg-neutral-800 dark:text-neutral-400"
                data-testid="date-separator"
              >
                {{ group.label }}
              </span>
            </div>
            <div
              v-for="message in group.messages"
              :key="message.messageId ?? message.clientMsgId"
              class="group mb-2 flex"
              :class="message.mine ? 'justify-end' : 'justify-start'"
              data-testid="message-item"
            >
              <div class="max-w-[75%]">
                <div
                  v-if="!message.mine"
                  class="mb-0.5 text-[11px] text-neutral-400 dark:text-neutral-500"
                  data-testid="message-sender"
                >
                  {{ message.senderId }}
                </div>
                <!-- M3 undecryptable: localized placeholder replaces content -->
                <div
                  v-if="message.undecryptable"
                  class="inline-block rounded-2xl bg-slate-100 px-3 py-1.5 text-sm italic leading-relaxed text-neutral-400 break-words dark:bg-slate-800 dark:text-neutral-500"
                  data-testid="undecryptable-placeholder"
                >
                  {{ t("chat.undecryptablePlaceholder") }}
                </div>
                <!-- Recall tombstone: localized placeholder replaces content -->
                <div
                  v-else-if="message.recalled"
                  class="inline-block rounded-2xl bg-slate-100 px-3 py-1.5 text-sm italic leading-relaxed text-neutral-400 break-words dark:bg-slate-800 dark:text-neutral-500"
                  data-testid="recalled-placeholder"
                >
                  {{ t("chat.recalledPlaceholder") }}
                </div>
                <div
                  v-else
                  class="inline-block rounded-2xl px-3 py-1.5 text-sm leading-relaxed break-words"
                  :class="
                    message.mine
                      ? 'bg-indigo-600 text-white'
                      : 'bg-slate-100 text-neutral-900 dark:bg-slate-800 dark:text-neutral-100'
                  "
                  data-testid="message-bubble"
                >
                  <!-- Reply quote rendered inline from server-resolved metadata -->
                  <span
                    v-if="message.replyToBodyPreview"
                    class="mb-1 block rounded border-l-2 px-2 py-0.5 text-xs opacity-80"
                    :class="
                      message.mine
                        ? 'border-white/70 bg-white/10'
                        : 'border-indigo-400 bg-black/5 dark:bg-white/5'
                    "
                    data-testid="reply-quote"
                  >
                    {{ message.replyToBodyPreview }}
                  </span>
                  {{ message.body }}
                </div>
                <!-- Hover/tap action row: reply / forward / recall.
                     Tombstones and undecryptable placeholders offer no
                     actions — there is nothing left to act on. -->
                <div
                  v-if="!message.recalled && !message.undecryptable"
                  class="mt-0.5 flex items-center gap-2 text-[11px] opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100"
                  :class="message.mine ? 'justify-end' : 'justify-start'"
                  data-testid="message-actions"
                >
                  <button
                    type="button"
                    class="text-neutral-500 hover:text-indigo-600 hover:underline dark:text-neutral-400"
                    data-testid="action-reply"
                    @click="replyToMessage(message)"
                  >
                    {{ t("chat.actionReply") }}
                  </button>
                  <button
                    type="button"
                    class="text-neutral-500 hover:text-indigo-600 hover:underline dark:text-neutral-400"
                    data-testid="action-forward"
                    @click="openForwardPicker(message)"
                  >
                    {{ t("chat.actionForward") }}
                  </button>
                  <button
                    v-if="canRecall(message)"
                    type="button"
                    class="text-neutral-500 hover:text-red-500 hover:underline dark:text-neutral-400"
                    data-testid="action-recall"
                    @click="recallMessage(message)"
                  >
                    {{ t("chat.actionRecall") }}
                  </button>
                </div>
                <div
                  class="mt-0.5 flex items-center gap-1.5 text-[11px]"
                  :class="message.mine ? 'justify-end' : 'justify-start'"
                >
                  <span
                    class="text-neutral-400 dark:text-neutral-500"
                    data-testid="message-time"
                  >
                    {{ clockTime(message.sentAt) }}
                  </span>
                  <span
                    v-if="message.mine && message.status === 'sending'"
                    class="animate-pulse text-indigo-500"
                    data-testid="message-status"
                  >
                    ✓ {{ t("chat.statusSending") }}
                  </span>
                  <span
                    v-else-if="message.mine && message.status === 'delivered'"
                    class="text-emerald-600 dark:text-emerald-400"
                    data-testid="message-status"
                  >
                    ✓✓ {{ t("chat.statusDelivered") }}
                  </span>
                  <span
                    v-else-if="message.mine && message.status === 'read'"
                    class="font-medium text-blue-500 dark:text-blue-400"
                    data-testid="message-status"
                  >
                    ✓✓ {{ t("chat.statusRead") }}
                  </span>
                  <button
                    v-else-if="message.mine && message.status === 'failed'"
                    type="button"
                    class="font-medium text-red-500 hover:underline"
                    data-testid="retry-button"
                    @click="retryMessage(message.clientMsgId)"
                  >
                    ⚠ {{ t("chat.statusFailed") }}
                  </button>
                </div>
              </div>
            </div>
          </template>
          <div ref="messagesEndRef"></div>
        </div>

        <!-- Peer typing indicator -->
        <div
          v-if="ws.activeConversation?.peerTypingUntil !== null"
          class="shrink-0 px-4 pb-1 text-xs text-neutral-500 dark:text-neutral-400"
          data-testid="typing-indicator"
        >
          {{ t("chat.typingIndicator") }}
        </div>

        <!-- Composer -->
        <footer
          class="flex shrink-0 flex-col border-t border-neutral-200 dark:border-neutral-800"
        >
          <!-- Reply context strip above the input -->
          <div
            v-if="ws.replyContext !== null"
            class="flex items-center gap-2 border-b border-neutral-200 bg-neutral-50 px-3 py-1.5 text-xs dark:border-neutral-800 dark:bg-neutral-900"
            data-testid="reply-context"
          >
            <span class="text-indigo-500">↩</span>
            <span
              class="min-w-0 flex-1 truncate text-neutral-600 dark:text-neutral-300"
              data-testid="reply-context-preview"
            >
              {{ ws.replyContext.bodyPreview }}
            </span>
            <button
              type="button"
              class="shrink-0 rounded px-1.5 py-0.5 font-medium text-neutral-500 hover:bg-neutral-200 hover:text-neutral-800 dark:hover:bg-neutral-700 dark:hover:text-neutral-100"
              data-testid="reply-cancel"
              :aria-label="t('chat.replyCancel')"
              @click="cancelReply()"
            >
              ×
            </button>
          </div>
          <div class="flex items-end gap-2 p-3">
            <textarea
              ref="composerEl"
              v-model="draft"
              rows="1"
              data-testid="composer-input"
              :placeholder="t('chat.inputPlaceholder')"
              :disabled="!ws.isConnected"
              class="max-h-40 min-h-9 flex-1 resize-none rounded-xl border border-neutral-300 bg-transparent px-3 py-2 text-sm outline-none focus:border-indigo-500 disabled:cursor-not-allowed disabled:opacity-60 dark:border-neutral-700"
              @keydown.enter.exact.prevent="sendMessage()"
              @input="onComposerInput()"
              @blur="onComposerBlur()"
            ></textarea>
            <button
              type="button"
              data-testid="composer-send"
              :disabled="!ws.isConnected || draft.trim().length === 0"
              class="shrink-0 rounded-xl bg-indigo-600 px-4 py-2 text-sm font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
              @click="sendMessage()"
            >
              {{ t("chat.composerSend") }}
            </button>
          </div>
        </footer>
      </div>

      <!-- Forward picker modal -->
      <div
        v-if="forwardSource !== null"
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
        data-testid="forward-picker"
      >
        <div
          class="w-full max-w-xs rounded-xl bg-white p-4 shadow-lg dark:bg-neutral-900"
        >
          <h3
            class="pb-2 text-sm font-semibold"
            data-testid="forward-picker-title"
          >
            {{ t("chat.forwardPickerTitle") }}
          </h3>
          <ul class="max-h-64 space-y-1 overflow-y-auto">
            <li
              v-for="conversation in forwardTargets"
              :key="conversation.conversationId"
            >
              <button
                type="button"
                class="flex w-full items-center gap-2 rounded-lg p-2 text-left text-sm hover:bg-neutral-100 dark:hover:bg-neutral-800"
                data-testid="forward-target"
                :data-conversation-id="conversation.conversationId"
                @click="confirmForward(conversation.conversationId)"
              >
                <span
                  class="flex h-7 w-7 shrink-0 items-center justify-center rounded-full bg-indigo-100 text-xs font-semibold text-indigo-700 dark:bg-indigo-900 dark:text-indigo-200"
                >
                  {{ initialOf(conversation.peerUsername) }}
                </span>
                {{ displayName(conversation) }}
              </button>
            </li>
          </ul>
          <button
            type="button"
            class="mt-2 w-full rounded-lg border border-neutral-300 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            data-testid="forward-cancel"
            @click="forwardSource = null"
          >
            {{ t("chat.forwardPickerCancel") }}
          </button>
        </div>
      </div>
    </template>
  </AppShell>
</template>
