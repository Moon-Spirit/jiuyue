<script setup lang="ts">
import { storeToRefs } from "pinia";
import { computed, onMounted } from "vue";
import { RouterLink, useRouter } from "vue-router";
import { useAuthStore } from "../stores/auth";

const auth = useAuthStore();
const { user, whoami, loading, errorMessage } = storeToRefs(auth);
const router = useRouter();

const memberSince = computed(() =>
  user.value === null
    ? "—"
    : new Date(user.value.created_at_ms).toLocaleString("zh-CN", {
        hour12: false,
      }),
);

onMounted(() => {
  // The protected path is called explicitly so reaching this page is *proof*
  // that the session works, not just that a token is sitting in storage.
  void auth.fetchWhoAmI();
});

async function signOut(): Promise<void> {
  await auth.logout();
  await router.replace({ name: "login" });
}
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-md rounded-2xl border border-zinc-200 bg-white p-8 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h1 class="text-xl font-semibold tracking-tight">已登录</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        受保护页面 · 会话有效时才能看到
      </p>

      <dl class="mt-8 space-y-3 text-sm" data-test="profile">
        <div class="flex items-baseline justify-between gap-4">
          <dt class="text-zinc-500 dark:text-zinc-400">用户名</dt>
          <dd class="font-medium">{{ user?.username ?? "—" }}</dd>
        </div>
        <div class="flex items-baseline justify-between gap-4">
          <dt class="text-zinc-500 dark:text-zinc-400">昵称</dt>
          <dd>{{ user?.display_name ?? "—" }}</dd>
        </div>
        <div class="flex items-baseline justify-between gap-4">
          <dt class="text-zinc-500 dark:text-zinc-400">邮箱</dt>
          <dd class="font-mono text-xs">{{ user?.email ?? "—" }}</dd>
        </div>
        <div class="flex items-baseline justify-between gap-4">
          <dt class="text-zinc-500 dark:text-zinc-400">注册时间</dt>
          <dd class="font-mono text-xs">{{ memberSince }}</dd>
        </div>
      </dl>

      <div class="mt-8 border-t border-zinc-200 pt-6 dark:border-zinc-800">
        <h2 class="text-sm font-semibold tracking-tight">
          受保护接口（GET /api/auth/whoami）
        </h2>
        <dl class="mt-4 space-y-3 text-sm" data-test="whoami">
          <div class="flex items-baseline justify-between gap-4">
            <dt class="text-zinc-500 dark:text-zinc-400">用户 ID</dt>
            <dd class="font-mono text-xs">{{ whoami?.user_id ?? "—" }}</dd>
          </div>
          <div class="flex items-baseline justify-between gap-4">
            <dt class="text-zinc-500 dark:text-zinc-400">会话 ID</dt>
            <dd class="font-mono text-xs" data-test="session-id">
              {{ whoami?.session_id ?? "—" }}
            </dd>
          </div>
        </dl>
      </div>

      <p
        v-if="errorMessage"
        class="mt-4 rounded-lg bg-red-50 p-3 text-xs leading-relaxed text-red-700 dark:bg-red-950/50 dark:text-red-300"
      >
        {{ errorMessage }}
      </p>

      <div class="mt-6 flex gap-3">
        <button
          type="button"
          class="flex-1 rounded-lg bg-zinc-900 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
          :disabled="loading"
          @click="signOut"
        >
          退出登录
        </button>
        <RouterLink
          class="flex-1 rounded-lg border border-zinc-300 px-4 py-2 text-center text-sm font-medium transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
          :to="{ name: 'health' }"
        >
          系统状态
        </RouterLink>
      </div>
    </section>
  </main>
</template>
