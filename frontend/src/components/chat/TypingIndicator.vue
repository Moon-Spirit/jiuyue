<script setup lang="ts">
import { computed } from "vue";
import { typingLabel } from "./typing-text";

/**
 * The "who is typing" line, shown above the composer.
 *
 * It takes resolved display names, not User ids: naming a Participant is the
 * view's job (a Direct peer's name is on the Conversation, a Group member's on the
 * member list), and keeping that out of here is what lets the label rules be a
 * pure function. The dots animate, and the animation is dropped under
 * `prefers-reduced-motion`.
 */
const props = defineProps<{ names: readonly string[] }>();

const label = computed(() => typingLabel(props.names));
</script>

<template>
  <p
    v-if="label !== null"
    class="flex items-center gap-2 bg-white px-4 py-1 text-xs text-zinc-500 dark:bg-zinc-900"
    data-test="typing-indicator"
    aria-live="polite"
  >
    <span class="inline-flex items-center gap-0.5" aria-hidden="true">
      <span
        class="h-1 w-1 animate-bounce rounded-full bg-zinc-400 motion-reduce:animate-none"
      ></span>
      <span
        class="h-1 w-1 animate-bounce rounded-full bg-zinc-400 [animation-delay:150ms] motion-reduce:animate-none"
      ></span>
      <span
        class="h-1 w-1 animate-bounce rounded-full bg-zinc-400 [animation-delay:300ms] motion-reduce:animate-none"
      ></span>
    </span>
    <span>{{ label }}</span>
  </p>
</template>
