<script setup lang="ts">
import { computed, ref } from "vue";
import { MESSAGE_MAX_CHARS } from "../../stores/chat";
import { charCount } from "../../validation";

const props = defineProps<{ disabled: boolean }>();
const emit = defineEmits<{ send: [body: string] }>();

const draft = ref("");
const remaining = computed(() => MESSAGE_MAX_CHARS - charCount(draft.value));

function submit(): void {
  const body = draft.value.trim();
  if (body === "" || props.disabled) return;

  emit("send", body);
  draft.value = "";
}

/** Enter sends; Shift+Enter inserts a newline. */
function onKeydown(event: KeyboardEvent): void {
  if (event.key !== "Enter" || event.shiftKey) return;
  event.preventDefault();
  submit();
}
</script>

<template>
  <form
    class="border-t border-zinc-200 bg-white p-3 dark:border-zinc-800 dark:bg-zinc-900"
    @submit.prevent="submit"
  >
    <div class="flex items-end gap-2">
      <textarea
        v-model="draft"
        :maxlength="MESSAGE_MAX_CHARS"
        rows="1"
        :placeholder="
          disabled
            ? '连接断开，暂时无法发送'
            : '输入消息，Enter 发送，Shift+Enter 换行'
        "
        :disabled="disabled"
        class="max-h-40 min-h-10 flex-1 resize-y rounded-xl border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-950"
        data-test="composer"
        @keydown="onKeydown"
      ></textarea>
      <button
        type="submit"
        class="rounded-xl bg-zinc-900 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
        :disabled="disabled || draft.trim() === ''"
      >
        发送
      </button>
    </div>
    <p v-if="remaining < 200" class="mt-1 text-right text-xs text-zinc-500">
      还可以输入 {{ remaining }} 个字
    </p>
  </form>
</template>
