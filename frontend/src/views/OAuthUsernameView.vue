<script setup lang="ts">
import { storeToRefs } from "pinia";
import { computed, reactive } from "vue";
import { RouterLink, useRouter } from "vue-router";
import { useAuthStore } from "../stores/auth";

/**
 * The username step a first-time third-party user must finish.
 *
 * Until this succeeds the account exists but is unusable: the limited session the
 * provider sign-in issued is not an access token, so nothing else in the app
 * accepts it. The step is therefore not a nicety — it is the only way forward,
 * and a reload that lost the limited token sends the user back to the provider
 * button rather than leaving them stuck.
 */

const auth = useAuthStore();
const { loading, errorMessage, fieldErrors, limitedToken } = storeToRefs(auth);
const router = useRouter();

const form = reactive({ username: "", displayName: "" });

/**
 * Whether this page can be used at all.
 *
 * No limited token means the browser lost it (a new tab, or storage denied). The
 * provider button is the way back — the server mints a fresh limited session for
 * the same account on the next callback — so the form is hidden rather than
 * allowed to fail on submit.
 */
const canSubmit = computed(() => limitedToken.value !== null);

async function submit(): Promise<void> {
  const ok = await auth.completeOAuthSignIn(form.username, form.displayName);
  if (!ok) return;

  await router.replace("/");
}
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-zinc-200 bg-white p-8 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h1 class="text-xl font-semibold tracking-tight">选择用户名</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        第三方账号已连接，设置一个用户名即可开始使用
      </p>

      <p
        v-if="!canSubmit"
        class="mt-4 rounded-lg bg-amber-50 p-3 text-xs leading-relaxed text-amber-800 dark:bg-amber-950/50 dark:text-amber-200"
        data-test="missing-token"
      >
        设置状态已丢失，请返回登录页重新使用第三方登录
      </p>

      <form
        v-if="canSubmit"
        class="mt-6 space-y-4"
        novalidate
        @submit.prevent="submit"
      >
        <label class="block">
          <span class="text-sm font-medium">用户名</span>
          <input
            v-model="form.username"
            type="text"
            autocomplete="username"
            placeholder="小写字母、数字和下划线"
            data-test="username-input"
            class="mt-1 w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
          />
          <span
            v-if="fieldErrors.username"
            class="mt-1 block text-xs text-red-600 dark:text-red-400"
            data-test="username-error"
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
          {{ loading ? "设置中…" : "完成设置" }}
        </button>
      </form>

      <p class="mt-6 text-center text-sm text-zinc-500 dark:text-zinc-400">
        <RouterLink
          class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
          :to="{ name: 'login' }"
        >
          返回登录页
        </RouterLink>
      </p>
    </section>
  </main>
</template>
