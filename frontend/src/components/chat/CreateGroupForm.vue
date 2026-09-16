<script setup lang="ts">
import { computed, ref } from "vue";

/**
 * Create a Group Conversation: a title and at least two member `@handle`s.
 *
 * The member picker is a free-text field rather than a contact list because the
 * contacts module is a later ticket; the server resolves handles case
 * insensitively and enforces the three-Participant minimum, so this form only
 * has to be honest about what it collected.
 */
const props = defineProps<{ busy: boolean }>();

const emit = defineEmits<{
  create: [title: string, members: string[]];
}>();

const title = ref("");
const memberInput = ref("");

/**
 * The handles typed so far.
 *
 * Split on whitespace, commas and the Chinese enumeration comma, so all three are
 * equally acceptable separators. Blanks are dropped; the server deduplicates.
 */
const members = computed<string[]>(() =>
  memberInput.value
    .split(/[\s,，、]+/)
    .map((username) => username.trim())
    .filter((username) => username !== ""),
);

const canSubmit = computed(
  () => !props.busy && title.value.trim() !== "" && members.value.length >= 2,
);

function submit(): void {
  if (!canSubmit.value) return;
  emit("create", title.value.trim(), members.value);
  title.value = "";
  memberInput.value = "";
}
</script>

<template>
  <form
    class="space-y-2 border-b border-zinc-200 p-3 dark:border-zinc-800"
    data-test="create-group-form"
    @submit.prevent="submit"
  >
    <p class="text-xs font-medium text-zinc-500">建群</p>
    <input
      v-model="title"
      type="text"
      placeholder="群名称"
      class="w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
      data-test="group-title-input"
    />
    <input
      v-model="memberInput"
      type="text"
      placeholder="成员用户名，逗号或空格分隔"
      class="w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
      data-test="group-members-input"
    />
    <button
      type="submit"
      class="w-full rounded-lg bg-zinc-900 px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
      :disabled="!canSubmit"
      data-test="create-group-submit"
    >
      创建群聊（至少 2 位成员）
    </button>
  </form>
</template>
