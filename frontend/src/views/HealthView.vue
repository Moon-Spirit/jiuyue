<script setup lang="ts">
import { storeToRefs } from "pinia";
import { computed, onMounted, onUnmounted } from "vue";
import type { SocketStatus } from "../api/ws";
import { useHealthStore } from "../stores/health";
import { useRealtimeStore } from "../stores/realtime";

const health = useHealthStore();
const { status, version, loading, error } = storeToRefs(health);

const realtime = useRealtimeStore();
const { status: socketStatus, sequence, serverTimeMs } = storeToRefs(realtime);

const statusLabels: Record<SocketStatus, string> = {
  idle: "未连接",
  connecting: "连接中…",
  open: "已连接",
  reconnecting: "重连中…",
  closed: "已断开",
};

const statusTones: Record<SocketStatus, string> = {
  idle: "text-zinc-500 dark:text-zinc-400",
  connecting: "text-amber-600 dark:text-amber-400",
  open: "text-emerald-600 dark:text-emerald-400",
  reconnecting: "text-amber-600 dark:text-amber-400",
  closed: "text-red-600 dark:text-red-400",
};

const socketLabel = computed(() => statusLabels[socketStatus.value]);
const socketTone = computed(() => statusTones[socketStatus.value]);

const lastPingLabel = computed(() =>
  serverTimeMs.value === null
    ? "—"
    : new Date(serverTimeMs.value).toLocaleTimeString("zh-CN", {
        hour12: false,
      }),
);

onMounted(() => {
  void health.fetchHealth();
  realtime.connect();
});

onUnmounted(() => {
  realtime.disconnect();
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

      <div class="mt-8 border-t border-zinc-200 pt-6 dark:border-zinc-800">
        <h2 class="text-sm font-semibold tracking-tight">
          实时通道（WebSocket /ws）
        </h2>
        <p class="mt-1 text-xs text-zinc-500 dark:text-zinc-400">
          连接后服务端推送的第一个事件即为 Ping 心跳。
        </p>

        <dl class="mt-4 space-y-3 text-sm" data-test="realtime-panel">
          <div class="flex items-baseline justify-between gap-4">
            <dt class="text-zinc-500 dark:text-zinc-400">连接状态</dt>
            <dd class="font-medium" :class="socketTone">{{ socketLabel }}</dd>
          </div>
          <div class="flex items-baseline justify-between gap-4">
            <dt class="text-zinc-500 dark:text-zinc-400">
              最近 Ping（服务端时间）
            </dt>
            <dd class="font-mono" data-test="last-ping">
              {{ lastPingLabel }}
            </dd>
          </div>
          <div class="flex items-baseline justify-between gap-4">
            <dt class="text-zinc-500 dark:text-zinc-400">连接序列号</dt>
            <dd class="font-mono" data-test="last-sequence">
              {{ sequence ?? "—" }}
            </dd>
          </div>
        </dl>
      </div>
    </section>
  </main>
</template>
