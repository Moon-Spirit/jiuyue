<script setup lang="ts">
import { storeToRefs } from "pinia";
import { onMounted } from "vue";
import { useHealthStore } from "../stores/health";

const health = useHealthStore();
const { status, version, loading, error } = storeToRefs(health);

onMounted(() => {
  void health.fetchHealth();
});
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-md rounded-2xl border border-zinc-200 bg-white p-8 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h1 class="text-xl font-semibold tracking-tight">jiuyue · 九月</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        前端骨架 · 后端连通性检查（GET /api/health）
      </p>

      <dl class="mt-8 space-y-3 text-sm">
        <div class="flex items-baseline justify-between gap-4">
          <dt class="text-zinc-500 dark:text-zinc-400">状态</dt>
          <dd
            v-if="loading"
            class="font-medium text-amber-600 dark:text-amber-400"
          >
            检测中…
          </dd>
          <dd
            v-else-if="error"
            class="font-medium text-red-600 dark:text-red-400"
          >
            连接失败
          </dd>
          <dd v-else class="font-medium text-emerald-600 dark:text-emerald-400">
            {{ status ?? "—" }}
          </dd>
        </div>
        <div class="flex items-baseline justify-between gap-4">
          <dt class="text-zinc-500 dark:text-zinc-400">版本</dt>
          <dd class="font-mono">{{ version ?? "—" }}</dd>
        </div>
      </dl>

      <p
        v-if="error"
        class="mt-4 rounded-lg bg-red-50 p-3 text-xs leading-relaxed text-red-700 dark:bg-red-950/50 dark:text-red-300"
      >
        {{
          error
        }}。若后端尚未启动，请先运行后端（http://localhost:8080）再重试。
      </p>

      <button
        type="button"
        class="mt-6 w-full rounded-lg bg-zinc-900 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
        :disabled="loading"
        @click="health.fetchHealth()"
      >
        重新检测
      </button>
    </section>
  </main>
</template>
