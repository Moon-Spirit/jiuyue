import { defineStore } from "pinia";
import { computed, ref } from "vue";
import {
  ApiError,
  apiGetAuthed,
  apiPost,
  apiPostAuthedNoContent,
} from "../api/client";
import type { AuthSession } from "../generated/AuthSession";
import type { ErrorCode } from "../generated/ErrorCode";
import type { FieldError } from "../generated/FieldError";
import type { TokenPair } from "../generated/TokenPair";
import type { UserProfile } from "../generated/UserProfile";
import type { WhoAmI } from "../generated/WhoAmI";
import {
  fieldMessages,
  normalizeEmail,
  normalizeUsername,
  validateLogin,
  validateRegistration,
  type LoginForm,
  type RegisterForm,
} from "../validation";

const ACCESS_TOKEN_KEY = "jiuyue.auth.access_token";
const REFRESH_TOKEN_KEY = "jiuyue.auth.refresh_token";

/**
 * The outcome of one silent re-authentication attempt.
 *
 * The distinction matters: only `rejected` means the session is genuinely gone.
 * A `failed` attempt is a network or server problem, and discarding the user's
 * session because the backend was briefly unreachable would be wrong.
 */
type Attempt = "ok" | "rejected" | "failed";

/**
 * Whether local storage accepted a write.
 *
 * A browser can deny storage (private mode, blocked cookies). That must not
 * break sign-in for the current tab — it only means the session will not survive
 * a reload — so the failure is recorded once and reported, never swallowed.
 */
let persistenceAvailable = true;

function readToken(key: string): string | null {
  if (!persistenceAvailable) return null;
  try {
    return window.localStorage.getItem(key);
  } catch (cause) {
    persistenceAvailable = false;
    console.warn(
      "凭据无法持久化：浏览器禁止了本地存储，本次会话仅在当前标签页有效",
      cause,
    );
    return null;
  }
}

function writeToken(key: string, value: string | null): void {
  if (!persistenceAvailable) return;
  try {
    if (value === null) window.localStorage.removeItem(key);
    else window.localStorage.setItem(key, value);
  } catch (cause) {
    persistenceAvailable = false;
    console.warn(
      "凭据无法持久化：浏览器禁止了本地存储，本次会话仅在当前标签页有效",
      cause,
    );
  }
}

/**
 * The signed-in session, the credentials that back it, and the state of the
 * last auth request.
 *
 * The store owns the session but not the transport: all HTTP goes through
 * `api/client`, which tests substitute at the `fetch` boundary, so the store's
 * own logic is exercised rather than mocked.
 */
export const useAuthStore = defineStore("auth", () => {
  const user = ref<UserProfile | null>(null);
  const accessToken = ref<string | null>(readToken(ACCESS_TOKEN_KEY));
  const refreshToken = ref<string | null>(readToken(REFRESH_TOKEN_KEY));
  const whoami = ref<WhoAmI | null>(null);

  const loading = ref(false);
  const errorCode = ref<ErrorCode | null>(null);
  const errorMessage = ref<string | null>(null);
  const fieldErrors = ref<Record<string, string>>({});
  /**
   * Seconds the backend asked us to wait, when the failure was a throttle or a
   * lockout. Surfaced so a view can show a real retry hint rather than prose.
   */
  const retryAfterSeconds = ref<number | null>(null);

  /** A session is present only when both a profile and a token are held. */
  const isAuthenticated = computed(
    () => user.value !== null && accessToken.value !== null,
  );

  function resetMessages(): void {
    errorCode.value = null;
    errorMessage.value = null;
    fieldErrors.value = {};
    retryAfterSeconds.value = null;
  }

  function applySession(session: AuthSession): void {
    user.value = session.user;
    applyTokens(session.tokens);
  }

  function applyTokens(tokens: TokenPair): void {
    accessToken.value = tokens.access_token;
    refreshToken.value = tokens.refresh_token;
    writeToken(ACCESS_TOKEN_KEY, tokens.access_token);
    writeToken(REFRESH_TOKEN_KEY, tokens.refresh_token);
  }

  /** Drop the local session and its persisted credentials. */
  function clear(): void {
    user.value = null;
    whoami.value = null;
    accessToken.value = null;
    refreshToken.value = null;
    writeToken(ACCESS_TOKEN_KEY, null);
    writeToken(REFRESH_TOKEN_KEY, null);
    resetMessages();
  }

  function applyError(cause: unknown): void {
    resetMessages();

    if (cause instanceof ApiError) {
      const detail = cause.body?.error ?? null;
      if (detail === null) {
        errorMessage.value = `请求失败（HTTP ${cause.status}）`;
        return;
      }
      errorCode.value = detail.code;
      errorMessage.value = detail.message;
      fieldErrors.value = fieldMessages(detail.fields);
      retryAfterSeconds.value = cause.retryAfterSeconds;
      return;
    }

    errorMessage.value = "无法连接服务器，请稍后重试";
  }

  /** Short-circuit a request when the client already knows the input is invalid. */
  function rejectLocally(problems: FieldError[]): boolean {
    if (problems.length === 0) return false;
    resetMessages();
    errorCode.value = "VALIDATION_FAILED";
    errorMessage.value = "请求参数无效";
    fieldErrors.value = fieldMessages(problems);
    return true;
  }

  /**
   * Ask the backend who we are, without deciding what a 401 means.
   *
   * A rejected access token is often just an expired one, and the refresh token
   * may still be good, so the caller decides whether to refresh or give up.
   */
  async function attemptCurrentUser(): Promise<Attempt> {
    if (accessToken.value === null) return "rejected";

    try {
      user.value = await apiGetAuthed<UserProfile>(
        "/auth/me",
        accessToken.value,
      );
      return "ok";
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 401) return "rejected";
      applyError(cause);
      return "failed";
    }
  }

  /** Exchange the refresh token for a new access token. */
  async function attemptRefresh(): Promise<Attempt> {
    if (refreshToken.value === null) return "rejected";

    try {
      const tokens = await apiPost<TokenPair>("/auth/refresh", {
        refresh_token: refreshToken.value,
      });
      applyTokens(tokens);
      return "ok";
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 401) return "rejected";
      applyError(cause);
      return "failed";
    }
  }

  /**
   * Make sure a usable session exists, refreshing silently when the access token
   * has merely expired.
   *
   * The session is discarded only when both tokens were explicitly rejected; an
   * unreachable backend leaves it intact.
   */
  async function ensureSession(): Promise<boolean> {
    const current = await attemptCurrentUser();
    if (current === "ok") return true;
    if (current === "failed") return false;

    const refreshed = await attemptRefresh();
    if (refreshed === "ok") return (await attemptCurrentUser()) === "ok";

    if (current === "rejected" && refreshed === "rejected") clear();
    return false;
  }

  /** Register, and sign in on success. Returns whether the account was created. */
  async function register(form: RegisterForm): Promise<boolean> {
    if (rejectLocally(validateRegistration(form))) return false;

    loading.value = true;
    resetMessages();
    try {
      const displayName = form.displayName.trim();
      const session = await apiPost<AuthSession>("/auth/register", {
        username: normalizeUsername(form.username),
        email: normalizeEmail(form.email),
        password: form.password,
        display_name: displayName === "" ? null : displayName,
      });
      applySession(session);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    } finally {
      loading.value = false;
    }
  }

  /** Log in. Returns whether the credentials were accepted. */
  async function login(form: LoginForm): Promise<boolean> {
    if (rejectLocally(validateLogin(form))) return false;

    loading.value = true;
    resetMessages();
    try {
      const session = await apiPost<AuthSession>("/auth/login", {
        email: normalizeEmail(form.email),
        password: form.password,
      });
      applySession(session);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    } finally {
      loading.value = false;
    }
  }

  /**
   * Revoke the session server-side, then drop it locally.
   *
   * Clearing happens in `finally`, so a failed request cannot leave the user
   * holding a session the UI says is gone — the server-side session still expires
   * on its own.
   */
  async function logout(): Promise<void> {
    const token = accessToken.value;
    try {
      if (token !== null) {
        await apiPostAuthedNoContent("/auth/logout", token);
      }
    } catch (cause) {
      console.warn("登出请求未送达服务器，该会话将在过期后失效", cause);
    } finally {
      clear();
    }
  }

  /** Load the authenticated user's profile, refreshing first when needed. */
  async function fetchCurrentUser(): Promise<boolean> {
    return ensureSession();
  }

  /** Call the protected `whoami` endpoint; the proof the session works. */
  async function fetchWhoAmI(): Promise<WhoAmI | null> {
    if (!(await ensureSession())) return null;
    if (accessToken.value === null) return null;

    try {
      whoami.value = await apiGetAuthed<WhoAmI>(
        "/auth/whoami",
        accessToken.value,
      );
      return whoami.value;
    } catch (cause) {
      applyError(cause);
      return null;
    }
  }

  /**
   * Re-establish the session after a reload.
   *
   * An access token that has simply expired is not a logged-out user: if the
   * refresh token still works, the session is restored silently.
   */
  async function restore(): Promise<void> {
    if (accessToken.value === null && refreshToken.value === null) return;
    await ensureSession();
  }

  return {
    user,
    accessToken,
    refreshToken,
    whoami,
    loading,
    errorCode,
    errorMessage,
    fieldErrors,
    retryAfterSeconds,
    isAuthenticated,
    register,
    login,
    logout,
    fetchCurrentUser,
    fetchWhoAmI,
    restore,
    clear,
  };
});
