<script setup lang="ts">
import type { ChatMessage } from "../../stores/chat";

defineProps<{
  message: ChatMessage;
  own: boolean;
  /**
   * Whether the other Participant's public Read Receipt has reached this
   * Message's Sequence Number. Always `false` for a peer's Message: a receipt is
   * only shown on the sender's own bubbles.
   */
  readByPeer: boolean;
}>();

const emit = defineEmits<{ retry: [clientMsgId: string] }>();
</script>

<template>
  <li
    class="flex"
    :class="own ? 'justify-end' : 'justify-start'"
    :data-sender="own ? 'own' : 'peer'"
  >
    <div class="max-w-[min(32rem,80%)]">
      <div
        class="rounded-2xl px-3.5 py-2 text-sm leading-relaxed break-words whitespace-pre-wrap"
        :class="
          own
            ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
            : 'bg-white text-zinc-900 ring-1 ring-zinc-200 dark:bg-zinc-800 dark:text-zinc-100 dark:ring-zinc-700'
        "
      >
        {{ message.body }}
      </div>

      <div v-if="own" class="mt-1 flex items-center justify-end gap-2 text-xs">
        <span
          v-if="message.state === 'sending'"
          class="text-zinc-400"
          data-test="delivery-state"
        >
          发送中…
        </span>
        <template v-else-if="message.state === 'failed'">
          <span class="text-red-500" data-test="delivery-state">
            {{ message.failure ?? "发送失败" }}
          </span>
          <button
            type="button"
            class="font-medium text-red-600 underline-offset-2 hover:underline dark:text-red-400"
            data-test="retry"
            @click="emit('retry', message.clientMsgId)"
          >
            重试
          </button>
        </template>
        <span
          v-else-if="readByPeer"
          class="text-emerald-600 dark:text-emerald-400"
          data-test="delivery-state"
        >
          已读
        </span>
        <span v-else class="text-zinc-400" data-test="delivery-state">
          已发送
        </span>
      </div>
    </div>
  </li>
</template>
