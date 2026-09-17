import { defineStore } from "pinia";
import { computed, ref } from "vue";
import {
  ApiError,
  apiGet,
  apiGetAuthed,
  apiPost,
  apiPostAuthed,
  apiPostAuthedNoContent,
  apiPostNoContent,
} from "../api/client";
import type { AuthSession } from "../generated/AuthSession";
import type { ErrorCode } from "../generated/ErrorCode";
import type { FieldError } from "../generated/FieldError";
import type { OAuthCallbackResponse } from "../generated/OAuthCallbackResponse";
import type { OAuthProvider } from "../generated/OAuthProvider";
import type { OAuthProviderInfo } from "../generated/OAuthProviderInfo";
import type { TokenPair } from "../generated/TokenPair";
import type { UserProfile } from "../generated/UserProfile";
import type { WhoAmI } from "../generated/WhoAmI";
import {
  fieldMessages,
  normalizeEmail,
  normalizeUsername,
  validateForgotPassword,
  validateLogin,
  validatePasswordReset,
  validateRegistration,
  validateUsername,
  type ForgotPasswordForm,
  type LoginForm,
  type RegisterForm,
  type ResetPasswordForm,
} from "../validation";

const ACCESS_TOKEN_KEY = "jiuyue.auth.access_token";
const REFRESH_TOKEN_KEY = "jiuyue.auth.refresh_token";

/**
 * Where the limited session from a first-time third-party sign-in lives.
 *
 * `sessionStorage` rather than `localStorage`, because the limited session is a
 * step in a flow rather than a credential to keep: it dies with the tab, and the
 * user can always begin again from the provider button. Access and refresh tokens
 * are the opposite — they belong in `localStorage` so a reload does not sign the
 * user out.
 */
const LIMITED_TOKEN_KEY = "jiuyue.auth.oauth_limited_token";

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
 * The limited session from a third-party sign-in, held for one tab.
 *
 * Deliberately a separate pair of readers from the access/refresh tokens: a
 * limited token is not a credential for the API, and mixing the two would make
 * "am I signed in?" answer yes for an account that has no username yet.
 */
function readLimitedToken(): string | null {
  try {
    return window.sessionStorage.getItem(LIMITED_TOKEN_KEY);
  } catch (cause) {
    // A browser that refuses session storage still gets the flow for this page
    // load; only the reload path is lost, and the user can redo it.
    console.warn("第三方登录的临时凭据无法持久化", cause);
    return null;
  }
}

function persistLimitedToken(value: string | null): void {
  try {
    if (value === null) window.sessionStorage.removeItem(LIMITED_TOKEN_KEY);
    else window.sessionStorage.setItem(LIMITED_TOKEN_KEY, value);
  } catch (cause) {
    console.warn("第三方登录的临时凭据无法持久化", cause);
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
  /**
   * The limited session a first-time third-party user holds until they choose a
   * username. It is not an access token, so {@link isAuthenticated} stays false
   * while it is the only thing held.
   */
  const limitedToken = ref<string | null>(readLimitedToken());
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

  /**
   * Whether the signed-in account's email is verified.
   *
   * The server enforces this; the flag exists so the UI can *explain* why a
   * restricted action is unavailable instead of letting it fail silently.
   */
  const emailVerified = computed(() => user.value?.email_verified ?? false);

  function resetMessages(): void {
    errorCode.value = null;
    errorMessage.value = null;
    fieldErrors.value = {};
    retryAfterSeconds.value = null;
  }

  /**
   * Set the limited session in the store and in this tab's storage together.
   *
   * Both halves matter: the ref is what the router guard and the username view
   * read, and storage is what survives the page reload a provider redirect
   * sometimes causes. Setting one without the other is how "sent to the username
   * step but told the state was lost" happens.
   */
  function rememberLimitedToken(value: string | null): void {
    limitedToken.value = value;
    persistLimitedToken(value);
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
    rememberLimitedToken(null);
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

  /**
   * Ask for a password-reset link.
   *
   * Returns whether the request was accepted — which is always true for a
   * well-formed address, because the server answers identically whether or not
   * an account exists. That is deliberate: the UI must show the same "已发送"
   * state either way, or it becomes the account-existence oracle the backend
   * refuses to be.
   */
  async function forgotPassword(form: ForgotPasswordForm): Promise<boolean> {
    if (rejectLocally(validateForgotPassword(form))) return false;

    loading.value = true;
    resetMessages();
    try {
      await apiPost<unknown>("/auth/forgot-password", {
        email: normalizeEmail(form.email),
      });
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    } finally {
      loading.value = false;
    }
  }

  /**
   * Ask for another verification link. Same uniform answer as {@link forgotPassword}.
   */
  async function resendVerification(
    form: ForgotPasswordForm,
  ): Promise<boolean> {
    if (rejectLocally(validateForgotPassword(form))) return false;

    loading.value = true;
    resetMessages();
    try {
      await apiPost<unknown>("/auth/resend-verification", {
        email: normalizeEmail(form.email),
      });
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    } finally {
      loading.value = false;
    }
  }

  /**
   * Redeem a verification link and return the updated profile.
   *
   * The link may be opened in a browser that is not signed in (mail clients open
   * links wherever), so the profile only replaces the local user when it is the
   * same account — a different account's verification must not hijack the tab.
   */
  async function verifyEmail(token: string): Promise<UserProfile | null> {
    loading.value = true;
    resetMessages();
    try {
      const profile = await apiPost<UserProfile>("/auth/verify-email", {
        token,
      });
      if (user.value === null || user.value.id === profile.id) {
        user.value = profile;
      }
      return profile;
    } catch (cause) {
      applyError(cause);
      return null;
    } finally {
      loading.value = false;
    }
  }

  /**
   * Redeem a reset link and set a new password.
   *
   * On success the local session is dropped: the server revokes every session as
   * part of the reset, so keeping the old tokens would only hold a dead session.
   */
  async function resetPassword(form: ResetPasswordForm): Promise<boolean> {
    if (rejectLocally(validatePasswordReset(form))) return false;

    loading.value = true;
    resetMessages();
    try {
      await apiPostNoContent("/auth/reset-password", {
        token: form.token,
        password: form.password,
      });
      clear();
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    } finally {
      loading.value = false;
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

  // --- Third-party sign-in -------------------------------------------------

  /**
   * The providers this instance can actually drive.
   *
   * Answered by the server, so a provider with no credentials is simply absent —
   * the page never renders a button that would fail on click. An instance with
   * none returns an empty list, and the login page shows no third-party section
   * at all.
   */
  async function fetchOAuthProviders(): Promise<OAuthProviderInfo[]> {
    try {
      const response = await apiGet<{ providers: OAuthProviderInfo[] }>(
        "/auth/oauth/providers",
      );
      return response.providers;
    } catch (cause) {
      // A missing provider list must not break email sign-in, so this is
      // reported softly and the section simply does not render.
      console.warn("第三方登录列表获取失败", cause);
      return [];
    }
  }

  /**
   * Ask the server where to send the browser for a third-party sign-in.
   *
   * The URL is the server's answer — it carries the `state` and the PKCE
   * challenge, and the client must never assemble it. The *navigation* is the
   * caller's: a store that redirected would be untestable without faking
   * `window.location`, and the view already owns where the user goes.
   */
  async function startOAuth(provider: OAuthProvider): Promise<string | null> {
    loading.value = true;
    resetMessages();
    try {
      const response = await apiPost<{ authorize_url: string }>(
        "/auth/oauth/start",
        { provider },
      );
      return response.authorize_url;
    } catch (cause) {
      applyError(cause);
      return null;
    } finally {
      loading.value = false;
    }
  }

  /**
   * Redeem the provider's redirect.
   *
   * Two outcomes, and the caller must handle both: a finished sign-in, or a
   * first-time user who must choose a username. The second is not an error — it
   * is a step, and the limited token that goes with it is kept for it.
   */
  async function completeOAuthCallback(
    provider: OAuthProvider,
    code: string,
    state: string,
  ): Promise<OAuthCallbackResponse | null> {
    loading.value = true;
    resetMessages();
    try {
      const response = await apiPost<OAuthCallbackResponse>(
        "/auth/oauth/callback",
        { provider, code, state },
      );

      if (response.session !== undefined) {
        applySession({
          user: response.session.user,
          tokens: response.session.tokens,
        });
        rememberLimitedToken(null);
        return response;
      }

      const limited = response.onboarding?.limited_token ?? null;
      rememberLimitedToken(limited);
      return response;
    } catch (cause) {
      rememberLimitedToken(null);
      applyError(cause);
      return null;
    } finally {
      loading.value = false;
    }
  }

  /**
   * Finish a first-time third-party sign-in by choosing a username.
   *
   * The limited token is the bearer credential, and the server spends it as it
   * claims the handle — so a username that lost a race comes back as a field
   * problem on the same token and the user simply picks another.
   */
  async function completeOAuthSignIn(
    username: string,
    displayName = "",
  ): Promise<boolean> {
    const limited = limitedToken.value;
    if (limited === null) {
      errorCode.value = "USERNAME_REQUIRED";
      errorMessage.value = "用户名设置已过期，请重新使用第三方登录";
      return false;
    }

    const problem = validateUsername(username);
    if (problem !== null) {
      rejectLocally([problem]);
      return false;
    }

    const trimmedDisplayName = displayName.trim();

    loading.value = true;
    resetMessages();
    try {
      const response = await apiPostAuthed<OAuthCallbackResponse>(
        "/auth/oauth/complete",
        {
          username: normalizeUsername(username),
          display_name: trimmedDisplayName === "" ? null : trimmedDisplayName,
        },
        limited,
      );

      const session = response.session;
      if (session === undefined) {
        // The server never answers an onboarding half from this endpoint; a
        // response without a session means the contract changed under us.
        errorMessage.value = "登录状态异常，请重新使用第三方登录";
        return false;
      }

      applySession({ user: session.user, tokens: session.tokens });
      rememberLimitedToken(null);
      return true;
    } catch (cause) {
      applyError(cause);
      return false;
    } finally {
      loading.value = false;
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
    limitedToken,
    whoami,
    loading,
    errorCode,
    errorMessage,
    fieldErrors,
    retryAfterSeconds,
    isAuthenticated,
    emailVerified,
    register,
    login,
    forgotPassword,
    resendVerification,
    verifyEmail,
    resetPassword,
    logout,
    fetchCurrentUser,
    fetchWhoAmI,
    fetchOAuthProviders,
    startOAuth,
    completeOAuthCallback,
    completeOAuthSignIn,
    restore,
    clear,
  };
});
