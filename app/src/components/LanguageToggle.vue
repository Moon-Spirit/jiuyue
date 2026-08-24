<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";
import { persistLocale } from "../i18n";

const { locale, t } = useI18n();

// Shows the language you would switch TO, like most apps do.
const label = computed(() => (locale.value === "zh-CN" ? "EN" : "中文"));

function toggle(): void {
  const next = locale.value === "zh-CN" ? "en" : "zh-CN";
  locale.value = next;
  persistLocale(next);
}
</script>

<template>
  <button
    type="button"
    class="inline-flex h-9 min-w-9 items-center justify-center rounded-lg px-2 text-sm font-medium text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 dark:text-neutral-300 dark:hover:bg-neutral-800 dark:hover:text-white"
    data-testid="lang-toggle"
    :aria-label="t('lang.toggle')"
    :title="t('lang.toggle')"
    @click="toggle()"
  >
    {{ label }}
  </button>
</template>
