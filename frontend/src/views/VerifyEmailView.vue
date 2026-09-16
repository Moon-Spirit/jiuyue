<script setup lang="ts">
import { storeToRefs } from "pinia";
import { computed, onMounted, reactive, ref } from "vue";
import { RouterLink, useRoute } from "vue-router";
import { useAuthStore } from "../stores/auth";

const auth = useAuthStore();
const { loading, errorCode, errorMessage, fieldErrors, user } =
  storeToRefs(auth);
const route = useRoute();

/** The token from the emailed link, when the link was well-formed. */
const token = computed(() =>
  typeof route.query.token === "string" ? route.query.token : "",
);

/**
 * What the page is showing.
 *
 * `inbox` is the "we just sent you mail, go check" state (no token in the URL);
 * `verifying`/`verified`/`failed` are the three outcomes of following a link.
 */
type Phase = "inbox" | "verifying" | "verified" | "failed";
const phase = ref<Phase>("inbox");

const resend = reactive({ email: user.value?.email ?? "" });
const resent = ref(false);

onMounted(async () => {
  if (token.value.length === 0) {
    phase.value = "inbox";
    return;
  }

  phase.value = "verifying";
  const profile = await auth.verifyEmail(token.value);
  phase.value = profile === null ? "failed" : "verified";
});

async function resendLink(): Promise<void> {
  const ok = await auth.resendVerification({ email: resend.email });
  if (ok) resent.value = true;
}
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-zinc-200 bg-white p-8 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h1 class="text-xl font-semibold tracking-tight">验证邮箱</h1>

      <p
        v-if="phase === 'verifying'"
        class="mt-4 text-sm text-zinc-500 dark:text-zinc-400"
        data-test="verifying"
      >
        正在验证…
      </p>

      <template v-else-if="phase === 'verified'">
        <p
          class="mt-4 rounded-lg bg-emerald-50 p-3 text-xs leading-relaxed text-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-300"
          data-test="verified"
        >
          邮箱验证成功，你现在可以开始新的会话了。
        </p>
        <p class="mt-6 text-center text-sm text-zinc-500 dark:text-zinc-400">
          <RouterLink
            class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
            :to="{ name: 'chat' }"
          >
            返回会话
          </RouterLink>
        </p>
      </template>

      <template v-else>
        <p
          v-if="phase === 'failed'"
          class="mt-4 rounded-lg bg-amber-50 p-3 text-xs leading-relaxed text-amber-800 dark:bg-amber-950/50 dark:text-amber-200"
          data-test="verify-error"
        >
          {{ errorMessage ?? "这个验证链接无法使用，请重新获取一封。" }}
        </p>
        <p
          v-else
          class="mt-1 text-sm text-zinc-500 dark:text-zinc-400"
          data-test="inbox"
        >
          我们已经把验证链接发到了你的邮箱。点开邮件里的链接即可完成验证；链接
          24 小时内有效。
        </p>

        <p
          v-if="
            phase === 'failed' &&
            (errorCode === 'TOKEN_EXPIRED' || errorCode === 'TOKEN_INVALID')
          "
          class="mt-2 text-center text-xs text-zinc-500 dark:text-zinc-400"
        >
          <RouterLink
            class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
            :to="{ name: 'forgot-password' }"
          >
            需要重置密码？
          </RouterLink>
        </p>

        <template v-if="!resent">
          <form class="mt-6 space-y-4" novalidate @submit.prevent="resendLink">
            <label class="block">
              <span class="text-sm font-medium">重新发送验证邮件</span>
              <input
                v-model="resend.email"
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

            <button
              type="submit"
              class="w-full rounded-lg bg-zinc-900 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
              :disabled="loading"
            >
              {{ loading ? "发送中…" : "重新发送" }}
            </button>
          </form>
        </template>

        <p
          v-else
          class="mt-6 rounded-lg bg-emerald-50 p-3 text-xs leading-relaxed text-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-300"
          data-test="resent"
        >
          如果该邮箱有待验证的账号，验证邮件已经发出。
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
    </section>
  </main>
</template>
