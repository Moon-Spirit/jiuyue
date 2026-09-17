<script setup lang="ts">
import { storeToRefs } from "pinia";
import { onMounted, reactive, ref } from "vue";
import { RouterLink, useRoute, useRouter } from "vue-router";
import { useAuthStore } from "../stores/auth";
import type { OAuthProviderInfo } from "../generated/OAuthProviderInfo";

const auth = useAuthStore();
const { loading, errorMessage, fieldErrors, retryAfterSeconds } =
  storeToRefs(auth);
const router = useRouter();
const route = useRoute();

const form = reactive({ email: "", password: "" });

/**
 * The providers this instance can drive.
 *
 * Empty when none is configured, in which case the third-party section does not
 * render at all — a provider with no credentials is absent from the server's
 * answer rather than present and failing on click.
 */
const providers = ref<OAuthProviderInfo[]>([]);

onMounted(async () => {
  providers.value = await auth.fetchOAuthProviders();
});

async function submit(): Promise<void> {
  const ok = await auth.login({ email: form.email, password: form.password });
  if (!ok) return;

  const redirect = route.query.redirect;
  await router.replace(typeof redirect === "string" ? redirect : "/");
}

/**
 * Hand the browser to the provider.
 *
 * The URL is the server's answer, because it carries the `state` and the PKCE
 * challenge; the client never assembles an authorization URL. A refusal (an
 * unconfigured provider, a throttled instance) leaves the error on the page
 * rather than navigating anywhere.
 */
async function signInWith(provider: OAuthProviderInfo): Promise<void> {
  const authorizeUrl = await auth.startOAuth(provider.provider);
  if (authorizeUrl === null) return;

  window.location.assign(authorizeUrl);
}
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-zinc-200 bg-white p-8 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h1 class="text-xl font-semibold tracking-tight">登录</h1>
      <p class="mt-1 text-sm text-zinc-500 dark:text-zinc-400">
        使用邮箱和密码进入 jiuyue · 九月
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

        <label class="block">
          <span class="text-sm font-medium">密码</span>
          <input
            v-model="form.password"
            type="password"
            autocomplete="current-password"
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

        <p class="text-right">
          <RouterLink
            class="text-xs font-medium text-zinc-500 underline-offset-4 hover:underline dark:text-zinc-400"
            :to="{ name: 'forgot-password' }"
          >
            忘记密码？
          </RouterLink>
        </p>

        <p
          v-if="retryAfterSeconds !== null"
          class="rounded-lg bg-amber-50 p-3 text-xs leading-relaxed text-amber-800 dark:bg-amber-950/50 dark:text-amber-200"
          data-test="retry-hint"
        >
          请等待 {{ retryAfterSeconds }} 秒后重试
        </p>

        <button
          type="submit"
          class="w-full rounded-lg bg-zinc-900 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
          :disabled="loading"
        >
          {{ loading ? "登录中…" : "登录" }}
        </button>
      </form>

      <div
        v-if="providers.length > 0"
        class="mt-6 border-t border-zinc-200 pt-6 dark:border-zinc-800"
        data-test="oauth-providers"
      >
        <p class="mb-3 text-center text-xs text-zinc-500 dark:text-zinc-400">
          或使用第三方账号登录
        </p>
        <div class="space-y-2">
          <button
            v-for="provider in providers"
            :key="provider.provider"
            type="button"
            class="w-full rounded-lg border border-zinc-300 px-4 py-2 text-sm font-medium transition-colors hover:bg-zinc-50 disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-700 dark:hover:bg-zinc-800"
            :data-test="`oauth-${provider.provider}`"
            :disabled="loading"
            @click="signInWith(provider)"
          >
            使用 {{ provider.display_name }} 登录
          </button>
        </div>
      </div>

      <p class="mt-6 text-center text-sm text-zinc-500 dark:text-zinc-400">
        还没有账号？
        <RouterLink
          class="font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
          :to="{ name: 'register' }"
        >
          注册
        </RouterLink>
      </p>
    </section>
  </main>
</template>
