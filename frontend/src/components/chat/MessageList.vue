<script setup lang="ts">
import { ref, watch } from "vue";
import type { ChatMessage } from "../../stores/chat";
import MessageBubble from "./MessageBubble.vue";

const props = defineProps<{
  messages: readonly ChatMessage[];
  currentUserId: string;
  loading: boolean;
  hasMore: boolean;
  loadingOlder: boolean;
}>();

const emit = defineEmits<{
  retry: [clientMsgId: string];
  loadOlder: [];
}>();

/** How close to the top (px) the user must scroll to pull in older history. */
const LOAD_THRESHOLD_PX = 64;

const scroller = ref<HTMLElement | null>(null);

/**
 * Scroll metrics captured just before an older page is requested, or `null` when
 * no anchor is pending. The height is the "before" value the prepend is measured
 * against.
 */
let anchorHeight: number | null = null;
let anchorTop = 0;

/**
 * Pull in the previous page when the user reaches the top.
 *
 * The anchor is captured *before* the store fetches, so the scroll restore below
 * can compensate for the height the prepend will add above the viewport.
 */
function onScroll(): void {
  const element = scroller.value;
  if (element === null) return;
  if (element.scrollTop > LOAD_THRESHOLD_PX) return;
  if (!props.hasMore || props.loadingOlder || props.loading) return;

  anchorHeight = element.scrollHeight;
  anchorTop = element.scrollTop;
  emit("loadOlder");
}

/**
 * Keep the viewport anchored when older Messages are prepended.
 *
 * Prepending grows the scrollable content above the viewport, so the browser
 * would otherwise leave the user looking at different Messages — the classic
 * "the list jumped" bug. Restoring `scrollTop` by the height delta keeps the
 * Message that was at the top edge exactly where it was.
 */
watch(
  () => props.messages.length,
  (current, previous) => {
    if (anchorHeight === null || current <= previous) return;

    const heightBefore = anchorHeight;
    const topBefore = anchorTop;
    anchorHeight = null;

    const element = scroller.value;
    if (element === null) return;
    element.scrollTop = element.scrollHeight - heightBefore + topBefore;
  },
  { flush: "post" },
);

/**
 * Drop an anchor that never became a prepend.
 *
 * A load can end with no new Messages — the beginning was reached, or the fetch
 * failed. The length watcher above does not fire for it, so a stale anchor would
 * otherwise be applied to the *next* Message that arrives. This runs after it, in
 * the same post-flush queue, and only sees an anchor the prepend path left behind.
 */
watch(
  () => props.loadingOlder,
  (isLoading) => {
    if (!isLoading) anchorHeight = null;
  },
  { flush: "post" },
);
</script>

<template>
  <div
    ref="scroller"
    class="flex h-full flex-col overflow-y-auto px-4 py-4"
    data-test="message-list"
    @scroll.passive="onScroll"
  >
    <p
      v-if="loadingOlder"
      class="mb-2 text-center text-xs text-zinc-500"
      data-test="loading-older"
    >
      正在加载更早的消息…
    </p>
    <p
      v-else-if="hasMore && messages.length > 0"
      class="mb-2 text-center text-xs text-zinc-400"
      data-test="more-history"
    >
      向上滚动加载更早的消息
    </p>

    <p v-if="loading" class="text-sm text-zinc-500">加载中…</p>
    <p
      v-else-if="messages.length === 0"
      class="m-auto text-sm text-zinc-500"
      data-test="message-empty"
    >
      还没有消息，说点什么吧。
    </p>
    <ul v-else role="list" class="mt-auto flex flex-col gap-3">
      <MessageBubble
        v-for="message in messages"
        :key="message.clientMsgId"
        :message="message"
        :own="message.senderId === currentUserId"
        @retry="emit('retry', $event)"
      />
    </ul>
  </div>
</template>
