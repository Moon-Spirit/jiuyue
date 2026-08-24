<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { apiErrorMessage } from "../lib/api/messages";
import { useAuthStore } from "../stores/auth";

const { t } = useI18n();
const router = useRouter();
const auth = useAuthStore();

const identifier = ref("");
const password = ref("");
const submitting = ref(false);
const errorMessage = ref("");

const canSubmit = computed(
  () => identifier.value.trim().length > 0 && password.value.length > 0,
);

async function submit(): Promise<void> {
  if (!canSubmit.value || submitting.value) return;
  submitting.value = true;
  errorMessage.value = "";
  try {
    await auth.login(identifier.value.trim(), password.value);
    await router.push("/chat");
  } catch (error) {
    errorMessage.value = apiErrorMessage(error, (key) => t(key));
  } finally {
    submitting.value = false;
  }
}
</script>

<template>
  <main
    class="flex min-h-dvh items-center justify-center bg-neutral-50 px-4 dark:bg-neutral-950"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-neutral-200 bg-white p-6 shadow-sm dark:border-neutral-800 dark:bg-neutral-900"
    >
      <h1 class="text-xl font-bold">{{ t("login.title") }}</h1>
      <p class="mt-1 text-sm text-neutral-500 dark:text-neutral-400">
        {{ t("login.subtitle") }}
      </p>

      <p
        v-if="errorMessage"
        role="alert"
        data-testid="login-error"
        class="mt-4 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-600 dark:bg-red-950/50 dark:text-red-400"
      >
        {{ errorMessage }}
      </p>

      <form class="mt-4 space-y-4" @submit.prevent="submit()">
        <div>
          <label for="login-identifier" class="block text-sm font-medium">
            {{ t("login.identifierLabel") }}
          </label>
          <input
            id="login-identifier"
            v-model="identifier"
            type="text"
            autocomplete="username"
            :placeholder="t('login.identifierPlaceholder')"
            data-testid="login-identifier"
            class="mt-1 w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700 dark:bg-neutral-800"
          />
        </div>
        <div>
          <label for="login-password" class="block text-sm font-medium">
            {{ t("login.passwordLabel") }}
          </label>
          <input
            id="login-password"
            v-model="password"
            type="password"
            autocomplete="current-password"
            :placeholder="t('login.passwordPlaceholder')"
            data-testid="login-password"
            class="mt-1 w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700 dark:bg-neutral-800"
          />
        </div>
        <button
          type="submit"
          :disabled="!canSubmit || submitting"
          data-testid="login-submit"
          class="w-full rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {{ submitting ? t("login.submitting") : t("login.submit") }}
        </button>
      </form>

      <p
        class="mt-4 text-center text-sm text-neutral-500 dark:text-neutral-400"
      >
        {{ t("login.noAccount") }}
        <RouterLink
          to="/register"
          class="font-medium text-indigo-600 hover:underline dark:text-indigo-400"
          data-testid="goto-register"
        >
          {{ t("login.gotoRegister") }}
        </RouterLink>
      </p>
    </section>
  </main>
</template>
