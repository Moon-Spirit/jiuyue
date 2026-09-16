<script setup lang="ts">
import type { ChatMessage } from "../../stores/chat";
import MessageBubble from "./MessageBubble.vue";

defineProps<{
  messages: readonly ChatMessage[];
  currentUserId: string;
  loading: boolean;
}>();

const emit = defineEmits<{ retry: [clientMsgId: string] }>();
</script>

<template>
  <div
    class="flex h-full flex-col overflow-y-auto px-4 py-4"
    data-test="message-list"
  >
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
