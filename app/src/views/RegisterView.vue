<script setup lang="ts">
import { computed, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import type { AuthChannel } from "../lib/api/auth";
import { apiErrorMessage } from "../lib/api/messages";
import { useAuthStore } from "../stores/auth";

const USERNAME_PATTERN = /^[a-z0-9_]{3,32}$/;
const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
const PHONE_PATTERN = /^\+?[0-9]{5,20}$/;
const MAX_COUNTDOWN_SECS = 60;

const { t } = useI18n();
const router = useRouter();
const auth = useAuthStore();

const channel = ref<AuthChannel>("email");
const target = ref("");
const code = ref("");
const username = ref("");
const password = ref("");
const submitting = ref(false);
const sendingCode = ref(false);
const errorMessage = ref("");
const countdown = ref(0);
/** Post-registration success screen shows the freshly-assigned UID. */
const registeredUid = ref<number | null>(null);

let countdownTimer: number | null = null;

const targetValid = computed(() =>
  channel.value === "email"
    ? EMAIL_PATTERN.test(target.value.trim())
    : PHONE_PATTERN.test(target.value.trim()),
);
const usernameValid = computed(() => USERNAME_PATTERN.test(username.value));
const passwordValid = computed(() => password.value.length >= 8);

const targetLabel = computed(() =>
  channel.value === "email"
    ? t("register.targetLabelEmail")
    : t("register.targetLabelPhone"),
);
const targetPlaceholder = computed(() =>
  channel.value === "email"
    ? t("register.targetPlaceholderEmail")
    : t("register.targetPlaceholderPhone"),
);

const usernameHintText = computed(() =>
  username.value.length > 0 && !usernameValid.value
    ? t("register.usernameInvalid")
    : t("register.usernameHint"),
);
const usernameHintClass = computed(() =>
  username.value.length > 0 && !usernameValid.value
    ? "text-red-500 dark:text-red-400"
    : "text-neutral-400 dark:text-neutral-500",
);
const passwordHintText = computed(() =>
  password.value.length > 0 && !passwordValid.value
    ? t("register.passwordInvalid")
    : t("register.passwordHint"),
);
const passwordHintClass = computed(() =>
  password.value.length > 0 && !passwordValid.value
    ? "text-red-500 dark:text-red-400"
    : "text-neutral-400 dark:text-neutral-500",
);

const canSendCode = computed(
  () => targetValid.value && countdown.value === 0 && !sendingCode.value,
);
const sendCodeLabel = computed(() =>
  countdown.value > 0
    ? t("register.resendCountdown", { secs: countdown.value })
    : t("register.sendCode"),
);

const formValid = computed(
  () =>
    targetValid.value &&
    code.value.trim().length > 0 &&
    usernameValid.value &&
    passwordValid.value,
);

function startCountdown(seconds: number): void {
  countdown.value = Math.max(0, Math.min(seconds, MAX_COUNTDOWN_SECS));
  if (countdownTimer !== null) window.clearInterval(countdownTimer);
  countdownTimer = window.setInterval(() => {
    countdown.value -= 1;
    if (countdown.value <= 0 && countdownTimer !== null) {
      window.clearInterval(countdownTimer);
      countdownTimer = null;
    }
  }, 1000);
}

onUnmounted(() => {
  if (countdownTimer !== null) window.clearInterval(countdownTimer);
});

async function sendCode(): Promise<void> {
  if (!canSendCode.value) return;
  sendingCode.value = true;
  errorMessage.value = "";
  try {
    const expiresInSecs = await auth.requestCode(
      channel.value,
      target.value.trim(),
    );
    startCountdown(expiresInSecs);
  } catch (error) {
    errorMessage.value = apiErrorMessage(error, (key) => t(key));
  } finally {
    sendingCode.value = false;
  }
}

async function submit(): Promise<void> {
  if (!formValid.value || submitting.value) return;
  submitting.value = true;
  errorMessage.value = "";
  try {
    await auth.register({
      channel: channel.value,
      target: target.value.trim(),
      code: code.value.trim(),
      username: username.value,
      password: password.value,
    });
    // Surface the new numeric UID once, prominently, before entering the app.
    registeredUid.value = auth.user?.uid ?? null;
  } catch (error) {
    errorMessage.value = apiErrorMessage(error, (key) => t(key));
  } finally {
    submitting.value = false;
  }
}

async function continueToChat(): Promise<void> {
  await router.push("/chat");
}
</script>

<template>
  <main
    class="flex min-h-dvh items-center justify-center bg-neutral-50 px-4 dark:bg-neutral-950"
  >
    <section
      class="w-full max-w-sm rounded-2xl border border-neutral-200 bg-white p-6 shadow-sm dark:border-neutral-800 dark:bg-neutral-900"
    >
      <h1 class="text-xl font-bold">{{ t("register.title") }}</h1>
      <p class="mt-1 text-sm text-neutral-500 dark:text-neutral-400">
        {{ t("register.subtitle") }}
      </p>

      <!-- Post-registration success: show the fresh UID before entering -->
      <div
        v-if="registeredUid !== null"
        class="mt-4 rounded-xl border border-indigo-200 bg-indigo-50 p-4 dark:border-indigo-800 dark:bg-indigo-950/40"
        data-testid="register-success"
      >
        <p
          class="text-sm font-medium leading-relaxed text-indigo-700 dark:text-indigo-300"
          data-testid="register-uid"
        >
          {{ t("register.uidIntro", { uid: registeredUid }) }}
        </p>
        <button
          type="button"
          data-testid="register-continue"
          class="mt-3 w-full rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium text-white hover:bg-indigo-500"
          @click="continueToChat()"
        >
          {{ t("register.continue") }}
        </button>
      </div>

      <p
        v-else-if="errorMessage"
        role="alert"
        data-testid="register-error"
        class="mt-4 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-600 dark:bg-red-950/50 dark:text-red-400"
      >
        {{ errorMessage }}
      </p>

      <form
        v-if="registeredUid === null"
        class="mt-4 space-y-4"
        @submit.prevent="submit()"
      >
        <!-- Channel tabs -->
        <div
          class="grid grid-cols-2 gap-1 rounded-lg bg-neutral-100 p-1 dark:bg-neutral-800"
          role="tablist"
          data-testid="channel-tabs"
        >
          <button
            type="button"
            role="tab"
            :aria-selected="channel === 'email'"
            data-testid="channel-email"
            class="rounded-md px-3 py-1.5 text-sm font-medium"
            :class="
              channel === 'email'
                ? 'bg-white text-neutral-900 shadow dark:bg-neutral-900 dark:text-white'
                : 'text-neutral-500 hover:text-neutral-700 dark:hover:text-neutral-300'
            "
            @click="channel = 'email'"
          >
            {{ t("register.channelEmail") }}
          </button>
          <button
            type="button"
            role="tab"
            :aria-selected="channel === 'phone'"
            data-testid="channel-phone"
            class="rounded-md px-3 py-1.5 text-sm font-medium"
            :class="
              channel === 'phone'
                ? 'bg-white text-neutral-900 shadow dark:bg-neutral-900 dark:text-white'
                : 'text-neutral-500 hover:text-neutral-700 dark:hover:text-neutral-300'
            "
            @click="channel = 'phone'"
          >
            {{ t("register.channelPhone") }}
          </button>
        </div>

        <!-- Target (email or phone) -->
        <div>
          <label for="register-target" class="block text-sm font-medium">{{
            targetLabel
          }}</label>
          <input
            id="register-target"
            v-model="target"
            type="text"
            :placeholder="targetPlaceholder"
            data-testid="register-target"
            class="mt-1 w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700 dark:bg-neutral-800"
          />
        </div>

        <!-- Verification code + send button -->
        <div>
          <label for="register-code" class="block text-sm font-medium">{{
            t("register.codeLabel")
          }}</label>
          <div class="mt-1 flex gap-2">
            <input
              id="register-code"
              v-model="code"
              type="text"
              inputmode="numeric"
              maxlength="6"
              :placeholder="t('register.codePlaceholder')"
              data-testid="register-code"
              class="min-w-0 flex-1 rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700 dark:bg-neutral-800"
            />
            <button
              type="button"
              :disabled="!canSendCode"
              data-testid="send-code"
              class="shrink-0 rounded-lg border border-indigo-500 px-3 py-2 text-sm font-medium text-indigo-600 hover:bg-indigo-50 disabled:cursor-not-allowed disabled:border-neutral-300 disabled:text-neutral-400 disabled:hover:bg-transparent dark:border-indigo-400 dark:text-indigo-400 dark:hover:bg-indigo-950/40 dark:disabled:border-neutral-700 dark:disabled:text-neutral-500"
              @click="sendCode()"
            >
              {{ sendCodeLabel }}
            </button>
          </div>
        </div>

        <!-- Username -->
        <div>
          <label for="register-username" class="block text-sm font-medium">{{
            t("register.usernameLabel")
          }}</label>
          <input
            id="register-username"
            v-model="username"
            type="text"
            autocomplete="username"
            :placeholder="t('register.usernamePlaceholder')"
            data-testid="register-username"
            class="mt-1 w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700 dark:bg-neutral-800"
          />
          <p
            data-testid="register-username-hint"
            class="mt-1 text-xs"
            :class="usernameHintClass"
          >
            {{ usernameHintText }}
          </p>
        </div>

        <!-- Password -->
        <div>
          <label for="register-password" class="block text-sm font-medium">{{
            t("register.passwordLabel")
          }}</label>
          <input
            id="register-password"
            v-model="password"
            type="password"
            autocomplete="new-password"
            :placeholder="t('register.passwordPlaceholder')"
            data-testid="register-password"
            class="mt-1 w-full rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700 dark:bg-neutral-800"
          />
          <p
            data-testid="register-password-hint"
            class="mt-1 text-xs"
            :class="passwordHintClass"
          >
            {{ passwordHintText }}
          </p>
        </div>

        <button
          type="submit"
          :disabled="!formValid || submitting"
          data-testid="register-submit"
          class="w-full rounded-lg bg-indigo-600 px-4 py-2 text-sm font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {{ submitting ? t("register.submitting") : t("register.submit") }}
        </button>
      </form>

      <p
        v-if="registeredUid === null"
        class="mt-4 text-center text-sm text-neutral-500 dark:text-neutral-400"
      >
        {{ t("register.hasAccount") }}
        <RouterLink
          to="/login"
          class="font-medium text-indigo-600 hover:underline dark:text-indigo-400"
          data-testid="goto-login"
        >
          {{ t("register.gotoLogin") }}
        </RouterLink>
      </p>
    </section>
  </main>
</template>
