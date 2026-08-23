<script setup lang="ts">
import { useI18n } from "vue-i18n";

const { t, locale } = useI18n();

function toggleLocale(): void {
  locale.value = locale.value === "zh-CN" ? "en" : "zh-CN";
}
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-slate-50 text-slate-900 dark:bg-slate-900 dark:text-slate-100"
  >
    <div class="text-center">
      <h1 class="text-5xl font-bold tracking-tight">{{ t("app.name") }}</h1>
      <p class="mt-3 text-slate-500 dark:text-slate-400">
        {{ t("app.tagline") }}
      </p>
      <button
        class="mt-8 rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium text-white hover:bg-indigo-500"
        data-testid="lang-toggle"
        @click="toggleLocale"
      >
        {{ t("app.toggleLocale") }}
      </button>
    </div>
  </main>
</template>
