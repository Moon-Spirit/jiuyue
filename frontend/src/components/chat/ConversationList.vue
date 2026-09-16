<script setup lang="ts">
import type { ConversationSummary } from "../../generated/ConversationSummary";

const props = defineProps<{
  conversations: readonly ConversationSummary[];
  activeId: string | null;
  loading: boolean;
  /** The User's Unread Count per Conversation id (CONTEXT.md: 未读数). */
  unreadCounts: Record<string, number>;
}>();

const emit = defineEmits<{ select: [id: string] }>();

/** The name shown for a Conversation: the peer's, or a group placeholder. */
function label(conversation: ConversationSummary): string {
  const peer = conversation.peer;
  if (peer === null) return "群聊";
  return peer.display_name === "" ? peer.username : peer.display_name;
}

/** This Conversation's badge, defaulting to zero for one never counted. */
function unreadFor(conversation: ConversationSummary): number {
  return props.unreadCounts[conversation.id] ?? 0;
}
</script>

<template>
  <nav class="min-h-0 flex-1 overflow-y-auto" aria-label="会话列表">
    <p v-if="loading" class="p-4 text-sm text-zinc-500">加载中…</p>
    <p
      v-else-if="conversations.length === 0"
      class="p-4 text-sm leading-relaxed text-zinc-500"
    >
      还没有会话。在上方输入对方用户名，开始一个新的单聊。
    </p>
    <ul
      v-else
      role="list"
      class="divide-y divide-zinc-100 dark:divide-zinc-800"
    >
      <li v-for="conversation in conversations" :key="conversation.id">
        <button
          type="button"
          class="flex w-full items-center gap-3 px-3 py-3 text-left transition-colors hover:bg-zinc-50 dark:hover:bg-zinc-800/60"
          :class="
            conversation.id === activeId ? 'bg-zinc-100 dark:bg-zinc-800' : ''
          "
          :aria-current="conversation.id === activeId ? 'true' : undefined"
          @click="emit('select', conversation.id)"
        >
          <span
            class="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-zinc-900 text-xs font-medium text-white dark:bg-zinc-100 dark:text-zinc-900"
          >
            {{ label(conversation).slice(0, 1).toUpperCase() }}
          </span>
          <span class="min-w-0 flex-1">
            <span class="block truncate text-sm font-medium">
              {{ label(conversation) }}
            </span>
            <span class="block truncate text-xs text-zinc-500">
              @{{ conversation.peer?.username ?? "group" }}
            </span>
          </span>
          <span
            v-if="unreadFor(conversation) > 0"
            class="shrink-0 rounded-full bg-zinc-900 px-2 py-0.5 text-xs font-medium text-white dark:bg-zinc-100 dark:text-zinc-900"
            data-test="unread-badge"
            :aria-label="`${unreadFor(conversation)} 条未读`"
          >
            {{ unreadFor(conversation) > 99 ? "99+" : unreadFor(conversation) }}
          </span>
        </button>
      </li>
    </ul>
  </nav>
</template>
