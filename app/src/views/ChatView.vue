<script setup lang="ts">
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import AppShell from "../components/layout/AppShell.vue";
import LanguageToggle from "../components/LanguageToggle.vue";
import ThemeToggle from "../components/ThemeToggle.vue";
import { useAuthStore } from "../stores/auth";

const { t } = useI18n();
const router = useRouter();
const auth = useAuthStore();

async function logout(): Promise<void> {
  auth.logout();
  await router.push("/login");
}
</script>

<template>
  <AppShell>
    <template #nav>
      <div
        class="flex w-full items-center gap-2 lg:flex-col lg:gap-2 lg:px-1 lg:py-1"
        data-testid="chat-nav"
      >
        <span class="text-sm font-bold tracking-tight lg:text-[11px]">{{
          t("app.name")
        }}</span>
        <span
          class="ml-auto min-w-0 truncate text-xs text-neutral-500 lg:ml-0 lg:max-w-full lg:text-center dark:text-neutral-400"
          data-testid="nav-username"
          :title="auth.user?.username ?? t('nav.anonymous')"
        >
          {{ auth.user?.username ?? t("nav.anonymous") }}
        </span>
        <ThemeToggle />
        <LanguageToggle />
        <button
          type="button"
          data-testid="logout-button"
          class="inline-flex h-9 items-center justify-center rounded-lg px-2 text-xs font-medium text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 lg:w-full dark:text-neutral-300 dark:hover:bg-neutral-800 dark:hover:text-white"
          @click="logout()"
        >
          {{ t("nav.logout") }}
        </button>
      </div>
    </template>

    <template #sessions>
      <div class="p-3">
        <h2
          class="px-1 pb-2 text-sm font-semibold text-neutral-500 dark:text-neutral-400"
          data-testid="sessions-title"
        >
          {{ t("chat.sessionsTitle") }}
        </h2>
        <ul class="space-y-1" data-testid="session-list">
          <li
            v-for="i in 3"
            :key="i"
            class="flex items-center gap-3 rounded-lg p-2 hover:bg-neutral-100 dark:hover:bg-neutral-800"
          >
            <span
              class="h-9 w-9 shrink-0 rounded-full bg-neutral-200 dark:bg-neutral-700"
            ></span>
            <span
              class="h-3 w-24 rounded bg-neutral-200 dark:bg-neutral-700"
            ></span>
          </li>
        </ul>
        <p
          class="px-1 pt-2 text-xs text-neutral-400 dark:text-neutral-500"
          data-testid="sessions-empty"
        >
          {{ t("chat.sessionsEmpty") }}
        </p>
      </div>
    </template>

    <template #main>
      <div class="flex h-full items-center justify-center">
        <p
          class="text-sm text-neutral-400 dark:text-neutral-500"
          data-testid="chat-empty-state"
        >
          {{ t("chat.emptyState") }}
        </p>
      </div>
    </template>
  </AppShell>
</template>
