<script setup lang="ts">
import { storeToRefs } from "pinia";
import { reactive, ref } from "vue";
import { RouterLink } from "vue-router";
import { useAuthStore } from "../stores/auth";

const auth = useAuthStore();
const { loading, errorMessage, fieldErrors } = storeToRefs(auth);

const form = reactive({ email: "" });
/** Whether the request was accepted. Always shown for a well-formed address. */
const sent = ref(false);

async function submit(): Promise<void> {
  const ok = await auth.forgotPassword({ email: form.email });
  if (ok) sent.value = true;
}
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-zinc-200 bg-white p-8 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h1 class="text-xl font-semibold tracking-tight">找回密码</h1>

      <template v-if="sent">
        <p
          class="mt-4 rounded-lg bg-emerald-50 p-3 text-xs leading-relaxed text-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-300"
          data-test="sent"
        >
          如果该邮箱已注册，我们已发送一封包含重置链接的邮件。请查收（也看看垃圾邮件），链接
          1 小时内有效。
        </p>
        <p class="mt-6 text-center text-sm text-zinc-500 dark:text-zinc-400">
          <RouterLink
            class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
            :to="{ name: 'login' }"
          >
            返回登录
          </RouterLink>
        </p>
      </template>

      <template v-else>
        <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
          输入注册时使用的邮箱，我们会把重置链接发到那里。
        </p>

        <form class="mt-6 space-y-4" novalidate @submit.prevent="submit">
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
            {{ loading ? "发送中…" : "发送重置链接" }}
          </button>
        </form>

        <p class="mt-6 text-center text-sm text-zinc-500 dark:text-zinc-400">
          想起密码了？
          <RouterLink
            class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
            :to="{ name: 'login' }"
          >
            返回登录
          </RouterLink>
        </p>
      </template>
    </section>
  </main>
</template>
