<script setup lang="ts">
import { storeToRefs } from "pinia";
import { reactive } from "vue";
import { RouterLink, useRouter } from "vue-router";
import { useAuthStore } from "../stores/auth";

const auth = useAuthStore();
const { loading, errorMessage, fieldErrors } = storeToRefs(auth);
const router = useRouter();

const form = reactive({
  username: "",
  email: "",
  displayName: "",
  password: "",
});

async function submit(): Promise<void> {
  const ok = await auth.register({
    username: form.username,
    email: form.email,
    password: form.password,
    displayName: form.displayName,
  });
  if (ok) await router.replace("/");
}
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-zinc-200 bg-white p-8 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h1 class="text-xl font-semibold tracking-tight">注册</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        创建一个 jiuyue · 九月 账号
      </p>

      <form class="mt-6 space-y-4" novalidate @submit.prevent="submit">
        <label class="block">
          <span class="text-sm font-medium">用户名</span>
          <input
            v-model="form.username"
            type="text"
            autocomplete="username"
            placeholder="小写字母、数字和下划线"
            class="mt-1 w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
          />
          <span
            v-if="fieldErrors.username"
            class="mt-1 block text-xs text-red-600 dark:text-red-400"
          >
            {{ fieldErrors.username }}
          </span>
        </label>

        <label class="block">
          <span class="text-sm font-medium">昵称（可选）</span>
          <input
            v-model="form.displayName"
            type="text"
            autocomplete="nickname"
            class="mt-1 w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
          />
          <span
            v-if="fieldErrors.display_name"
            class="mt-1 block text-xs text-red-600 dark:text-red-400"
          >
            {{ fieldErrors.display_name }}
          </span>
        </label>

        <label class="block">
          <span class="text-sm font-medium">邮箱</span>
          <input
            v-model="form.email"
            type="email"
            autocomplete="email"
            class="mt-1 w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
          />
          <span
            v-if="fieldErrors.email"
            class="mt-1 block text-xs text-red-600 dark:text-red-400"
          >
            {{ fieldErrors.email }}
          </span>
        </label>

        <label class="block">
          <span class="text-sm font-medium">密码</span>
          <input
            v-model="form.password"
            type="password"
            autocomplete="new-password"
            placeholder="至少 8 位，包含字母和数字"
            class="mt-1 w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
          />
          <span
            v-if="fieldErrors.password"
            class="mt-1 block text-xs text-red-600 dark:text-red-400"
          >
            {{ fieldErrors.password }}
          </span>
        </label>

        <p
          v-if="errorMessage"
          class="rounded-lg bg-red-50 p-3 text-xs leading-relaxed text-red-700 dark:bg-red-950/50 dark:text-red-300"
          data-test="form-error"
        >
          {{ errorMessage }}
        </p>

        <button
          type="submit"
          class="w-full rounded-lg bg-zinc-900 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
          :disabled="loading"
        >
          {{ loading ? "注册中…" : "注册" }}
        </button>
      </form>

      <p class="mt-6 text-center text-sm text-zinc-500 dark:text-zinc-400">
        已有账号？
        <RouterLink
          class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
          :to="{ name: 'login' }"
        >
          登录
        </RouterLink>
      </p>
    </section>
  </main>
</template>
