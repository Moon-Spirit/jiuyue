<script setup lang="ts">
import { computed } from "vue";
import { useRouter } from "vue-router";
import { avatarEmojiOf } from "../lib/identity";

/**
 * Round user avatar: the curated emoji when the user has picked one, else a
 * colored initial-circle built from the display label.
 *
 * Every conversation-context surface (nav self entry, session rows, contacts,
 * search hits, thread headers, profile header) renders through this one
 * component so the emoji-vs-initial rule and the click behavior can't drift.
 *
 * Click semantics:
 * - The inner click ALWAYS calls event.stopPropagation(), so an avatar placed
 *   inside a clickable row (session item / contact row) never opens the row.
 * - With `to` set, the avatar navigates there itself.
 * - Without `to`, the component only emits "click" — consumers that need
 *   different behavior (e.g. opening the avatar picker) listen for it.
 */

const props = withDefaults(
  defineProps<{
    /** Stable username (fallback label + initial source). */
    username: string;
    /** Curated label; falls back to username inside the circle. */
    displayName?: string;
    /** Curated avatar emoji, or null/unset to render the initial circle. */
    avatar?: string | null;
    /** Square canvas size in px (rendered as a circle). */
    size?: number;
    /** When set, clicking navigates to this route (stopPropagation first). */
    to?: string;
    /** Group avatar: force the indigo initial circle (never a curated palette). */
    group?: boolean;
  }>(),
  { displayName: "", avatar: null, size: 36, to: "", group: false },
);

const emit = defineEmits<{
  click: [];
}>();

const router = useRouter();

const label = computed<string>(() => {
  const display = props.displayName?.trim() ?? "";
  return display.length > 0 ? display : props.username;
});

const emoji = computed<string | null>(() => {
  const raw = props.avatar ?? null;
  if (raw !== null && raw.startsWith("data:image/")) return null;
  return avatarEmojiOf(raw);
});

/** Custom uploaded image (data: URL) renders as an <img>, not a glyph. */
const customImage = computed<string | null>(() => {
  const raw = props.avatar ?? null;
  return raw !== null && raw.startsWith("data:image/") ? raw : null;
});

const initial = computed<string>(() => {
  const first = label.value.trim().charAt(0);
  return first.length === 0 ? "?" : first.toUpperCase();
});

/** Deterministic palette per user so initials stay stable across reloads. */
const circleClass = computed<string>(() => {
  // Groups always render the brand indigo circle, regardless of name hash.
  if (props.group) {
    return "bg-indigo-500 text-white dark:bg-indigo-600 dark:text-white";
  }
  const palette = [
    "bg-indigo-100 text-indigo-700 dark:bg-indigo-900 dark:text-indigo-200",
    "bg-rose-100 text-rose-700 dark:bg-rose-900 dark:text-rose-200",
    "bg-emerald-100 text-emerald-700 dark:bg-emerald-900 dark:text-emerald-200",
    "bg-amber-100 text-amber-700 dark:bg-amber-900 dark:text-amber-200",
    "bg-sky-100 text-sky-700 dark:bg-sky-900 dark:text-sky-200",
    "bg-fuchsia-100 text-fuchsia-700 dark:bg-fuchsia-900 dark:text-fuchsia-200",
  ];
  let hash = 0;
  const basis = props.username.length > 0 ? props.username : label.value;
  for (let i = 0; i < basis.length; i += 1) {
    hash = (hash * 31 + basis.charCodeAt(i)) >>> 0;
  }
  return palette[hash % palette.length] ?? palette[0]!;
});

const style = computed(() => ({
  width: `${props.size}px`,
  height: `${props.size}px`,
  fontSize: `${Math.max(10, Math.round(props.size * 0.36))}px`,
}));

const clickable = computed<boolean>(() => props.to.length > 0);

function handleClick(event: MouseEvent): void {
  // Rows / cells behind the avatar are never activated by an avatar click.
  event.stopPropagation();
  emit("click");
  if (props.to.length > 0) void router.push(props.to);
}
</script>

<template>
  <span
    class="inline-flex shrink-0 select-none items-center justify-center rounded-full"
    :class="
      clickable
        ? 'cursor-pointer hover:ring-2 hover:ring-indigo-400/60 dark:hover:ring-indigo-500/60'
        : ''
    "
    :style="style"
    :title="label"
    data-testid="avatar"
    :aria-label="label"
    @click="handleClick($event)"
  >
    <img
      v-if="customImage !== null"
      :src="customImage"
      :alt="label"
      class="h-full w-full rounded-full object-cover"
      data-testid="avatar-image"
    />
    <span
      v-else-if="emoji !== null"
      class="leading-none"
      data-testid="avatar-emoji"
      >{{ emoji }}</span
    >
    <span
      v-else
      class="flex h-full w-full items-center justify-center rounded-full font-semibold"
      :class="circleClass"
      data-testid="avatar-initial"
      >{{ initial }}</span
    >
  </span>
</template>
