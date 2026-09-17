<script setup lang="ts">
import { onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useAuthStore } from "../stores/auth";
import type { OAuthProvider } from "../generated/OAuthProvider";

/**
 * The page the provider's `redirect_uri` lands on.
 *
 * It renders no form: the query string carries a one-shot `code` and a `state`,
 * and the redeem request happens once on mount. Doing it on mount rather than in
 * a click handler is what makes a normal provider redirect work — the browser
 * arrives here with a GET and nothing else.
 *
 * The `code` is spent by the server either way, so a reload of this page fails
 * with `OAUTH_STATE_INVALID` rather than signing in twice.
 */

const auth = useAuthStore();
const router = useRouter();
const route = useRoute();

const failure = ref<string | null>(null);

/** Read a query parameter as a non-empty string. */
function queryString(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

/**
 * Which provider answered.
 *
 * The provider is part of the redirect URI, not of the provider's own response,
 * so it travels in the `provider` query parameter this application appends when
 * it registers the callback. A missing one is a malformed redirect, not a
 * default.
 */
function providerFromQuery(): OAuthProvider | null {
  const raw = queryString(route.query["provider"]) ?? "github";
  return raw === "github" || raw === "google" ? raw : null;
}

onMounted(async () => {
  const code = queryString(route.query["code"]);
  const state = queryString(route.query["state"]);
  const provider = providerFromQuery();

  if (code === null || state === null || provider === null) {
    failure.value = "第三方登录的回调不完整，请重新登录";
    return;
  }

  const response = await auth.completeOAuthCallback(provider, code, state);
  if (response === null) {
    failure.value = auth.errorMessage ?? "第三方登录失败，请重试";
    return;
  }

  // A first-time user is sent to the username step rather than into the app: the
  // account exists but cannot be used until a handle is chosen.
  if (response.session === undefined) {
    await router.replace({ name: "oauth-username" });
    return;
  }

  const redirect = response.session.redirect_path;
  await router.replace(redirect ?? "/");
});
</script>

<template>
  <main
    class="flex min-h-screen items-center justify-center bg-zinc-50 p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-zinc-200 bg-white p-8 text-center shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
    >
      <p
        v-if="failure === null"
        class="text-sm text-zinc-500 dark:text-zinc-400"
        data-test="oauth-pending"
      >
        正在完成第三方登录…
      </p>

      <template v-else>
        <h1 class="text-lg font-semibold tracking-tight">登录未完成</h1>
        <p
          class="mt-2 rounded-lg bg-red-50 p-3 text-xs leading-relaxed text-red-700 dark:bg-red-950/50 dark:text-red-300"
          data-test="oauth-error"
        >
          {{ failure }}
        </p>
        <RouterLink
          class="mt-4 inline-block text-sm font-medium text-zinc-900 underline-offset-4 hover:underline dark:text-zinc-100"
          :to="{ name: 'login' }"
        >
          返回登录页
        </RouterLink>
      </template>
    </section>
  </main>
</template>
