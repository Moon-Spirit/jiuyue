<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import type { PresenceStatus } from "../../generated/PresenceStatus";
import { presenceLabel } from "./presence-text";

const props = defineProps<{
  /** The User's reachability, or `null` when nothing is known yet. */
  status: PresenceStatus | null;
  /** When the User was last reachable, from the server; `null` while online. */
  lastSeenMs: number | null;
}>();

/**
 * The clock the relative text is measured against.
 *
 * Refreshed on a slow timer so "刚刚" becomes "1 分钟前" while a screen stays open
 * — a presence line that never ages is worse than no line at all.
 */
const nowMs = ref(Date.now());
let refreshTimer: ReturnType<typeof setInterval> | null = null;

onMounted(() => {
  refreshTimer = setInterval(() => {
    nowMs.value = Date.now();
  }, 30_000);
});

onUnmounted(() => {
  if (refreshTimer !== null) clearInterval(refreshTimer);
  refreshTimer = null;
});

const label = computed(() =>
  presenceLabel(props.status, props.lastSeenMs, nowMs.value),
);
const online = computed(() => props.status === "online");
</script>

<template>
  <span
    v-if="label !== null"
    class="inline-flex items-center gap-1.5 text-xs"
    data-test="presence"
  >
    <span
      aria-hidden="true"
      class="h-2 w-2 shrink-0 rounded-full"
      :class="online ? 'bg-emerald-500' : 'bg-zinc-400 dark:bg-zinc-600'"
    />
    <span
      data-test="presence-label"
      :class="
        online ? 'text-emerald-600 dark:text-emerald-400' : 'text-zinc-500'
      "
    >
      {{ label }}
    </span>
  </span>
</template>
