<script setup lang="ts">
import { storeToRefs } from "pinia";
import { computed, reactive, ref } from "vue";
import { RouterLink, useRoute } from "vue-router";
import { useAuthStore } from "../stores/auth";

const auth = useAuthStore();
const { loading, errorCode, errorMessage, fieldErrors } = storeToRefs(auth);
const route = useRoute();

/** The token from the emailed link, when the link was well-formed. */
const token = computed(() =>
  typeof route.query.token === "string" ? route.query.token : "",
);

const form = reactive({ password: "", confirm: "" });
const done = ref(false);

async function submit(): Promise<void> {
  const ok = await auth.resetPassword({
    token: token.value,
    password: form.password,
    confirm: form.confirm,
  });
  if (ok) done.value = true;
}
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-zinc-200 bg-white p-8 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h1 class="text-xl font-semibold tracking-tight">设置新密码</h1>

      <template v-if="done">
        <p
          class="mt-4 rounded-lg bg-emerald-50 p-3 text-xs leading-relaxed text-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-300"
          data-test="done"
        >
          密码已更新。为安全起见，此前登录的设备都已退出，请用新密码重新登录。
        </p>
        <p class="mt-6 text-center text-sm text-zinc-500 dark:text-zinc-400">
          <RouterLink
            class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
            :to="{ name: 'login' }"
          >
            去登录
          </RouterLink>
        </p>
      </template>

      <template v-else-if="token.length === 0">
        <p
          class="mt-4 rounded-lg bg-amber-50 p-3 text-xs leading-relaxed text-amber-800 dark:bg-amber-950/50 dark:text-amber-200"
          data-test="missing-token"
        >
          这个链接不完整，请重新打开邮件里的重置链接，或重新申请一封。
        </p>
        <p class="mt-6 text-center text-sm text-zinc-500 dark:text-zinc-400">
          <RouterLink
            class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
            :to="{ name: 'forgot-password' }"
          >
            重新申请
          </RouterLink>
        </p>
      </template>

      <template v-else>
        <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
          设置一个新密码。链接只能使用一次。
        </p>

        <form class="mt-6 space-y-4" novalidate @submit.prevent="submit">
          <label class="block">
            <span class="text-sm font-medium">新密码</span>
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

          <label class="block">
            <span class="text-sm font-medium">确认新密码</span>
            <input
              v-model="form.confirm"
              type="password"
              autocomplete="new-password"
              class="mt-1 w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
            />
            <span
              v-if="fieldErrors.confirm"
              class="mt-1 block text-xs text-red-600 dark:text-red-400"
            >
              {{ fieldErrors.confirm }}
            </span>
          </label>

          <p
            v-if="errorMessage"
            class="rounded-lg bg-red-50 p-3 text-xs leading-relaxed text-red-700 dark:bg-red-950/50 dark:text-red-300"
            data-test="form-error"
          >
            {{ errorMessage }}
          </p>

          <p
            v-if="
              errorCode === 'TOKEN_EXPIRED' || errorCode === 'TOKEN_INVALID'
            "
            class="text-center text-xs text-zinc-500 dark:text-zinc-400"
          >
            <RouterLink
              class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
              :to="{ name: 'forgot-password' }"
            >
              申请一封新的重置邮件
            </RouterLink>
          </p>

          <button
            type="submit"
            class="w-full rounded-lg bg-zinc-900 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
            :disabled="loading"
          >
            {{ loading ? "提交中…" : "设置新密码" }}
          </button>
        </form>
      </template>
    </section>
  </main>
</template>
