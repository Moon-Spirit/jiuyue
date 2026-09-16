import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useAuthStore } from "./auth";

const ACCESS_KEY = "jiuyue.auth.access_token";
const REFRESH_KEY = "jiuyue.auth.refresh_token";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

/** A session as the backend returns it. */
function sessionPayload(): unknown {
  return {
    user: {
      id: "01JABC1234567890ABCDEFGHJ1",
      username: "alice",
      email: "alice@example.com",
      display_name: "alice",
      avatar_url: null,
      email_verified: false,
      created_at_ms: 1_700_000_000_000,
    },
    tokens: {
      access_token: "access-1",
      refresh_token: "refresh-1",
      token_type: "Bearer",
      expires_in: 900,
    },
  };
}

function profilePayload(): unknown {
  const session = sessionPayload() as { user: unknown };
  return session.user;
}

/** An `ErrorBody` as the backend returns it. */
function errorPayload(
  code: string,
  message: string,
  fields: readonly { field: string; code: string; message: string }[] = [],
  retryAfterSeconds: number | null = null,
): unknown {
  return {
    error: { code, message, fields, retry_after_seconds: retryAfterSeconds },
  };
}

type Responder = () => Response;

/**
 * Substitute `fetch` with a queue of responders per URL.
 *
 * The boundary mocked is the HTTP one — no store function is stubbed — so the
 * request shape, the error mapping and the persistence are all really exercised.
 */
function stubFetch(
  routes: Record<string, readonly Responder[]>,
): ReturnType<typeof vi.fn<typeof fetch>> {
  const queues: Record<string, Responder[]> = {};
  for (const [url, responders] of Object.entries(routes)) {
    queues[url] = [...responders];
  }

  const mock = vi.fn<typeof fetch>(async (input) => {
    const url = String(input);
    const responder = queues[url]?.shift();
    if (responder === undefined) {
      throw new Error(`unexpected request: ${url}`);
    }
    return responder();
  });

  vi.stubGlobal("fetch", mock);
  return mock;
}

/** The URL and init of one recorded fetch call. */
function callAt(
  mock: ReturnType<typeof vi.fn<typeof fetch>>,
  index: number,
): { url: string; init: RequestInit | undefined } {
  const call = mock.mock.calls[index];
  if (call === undefined) throw new Error(`fetch call ${index} is missing`);
  return { url: String(call[0]), init: call[1] };
}

describe("useAuthStore", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    window.localStorage.clear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("logs in, stores the session and persists the tokens", async () => {
    const fetchMock = stubFetch({
      "/api/auth/login": [() => jsonResponse(sessionPayload())],
    });

    const store = useAuthStore();
    const ok = await store.login({
      email: "  Alice@Example.COM ",
      password: "secret123",
    });

    expect(ok).toBe(true);
    expect(store.user?.username).toBe("alice");
    expect(store.isAuthenticated).toBe(true);
    expect(window.localStorage.getItem(ACCESS_KEY)).toBe("access-1");
    expect(window.localStorage.getItem(REFRESH_KEY)).toBe("refresh-1");

    const { url, init } = callAt(fetchMock, 0);
    expect(url).toBe("/api/auth/login");
    expect(init?.method).toBe("POST");
    const body = init?.body;
    expect(typeof body === "string" ? JSON.parse(body) : null).toEqual({
      email: "alice@example.com",
      password: "secret123",
    });
  });

  it("maps a server field error onto the named field", async () => {
    stubFetch({
      "/api/auth/login": [
        () =>
          jsonResponse(
            errorPayload("VALIDATION_FAILED", "请求参数无效", [
              {
                field: "email",
                code: "INVALID_FORMAT",
                message: "邮箱格式不正确",
              },
            ]),
            422,
          ),
      ],
    });

    const store = useAuthStore();
    const ok = await store.login({
      email: "alice@example.com",
      password: "secret123",
    });

    expect(ok).toBe(false);
    expect(store.errorCode).toBe("VALIDATION_FAILED");
    expect(store.fieldErrors["email"]).toBe("邮箱格式不正确");
    expect(store.isAuthenticated).toBe(false);
  });

  it("surfaces the throttle code and the retry hint from a 429", async () => {
    stubFetch({
      "/api/auth/login": [
        () =>
          jsonResponse(
            errorPayload(
              "TOO_MANY_ATTEMPTS",
              "登录尝试过于频繁，请稍后再试",
              [],
              42,
            ),
            429,
          ),
      ],
    });

    const store = useAuthStore();
    const ok = await store.login({
      email: "alice@example.com",
      password: "secret123",
    });

    expect(ok).toBe(false);
    expect(store.errorCode).toBe("TOO_MANY_ATTEMPTS");
    expect(store.retryAfterSeconds).toBe(42);
    expect(store.isAuthenticated).toBe(false);
  });

  it("surfaces a lockout distinctly and drops the hint on the next attempt", async () => {
    stubFetch({
      "/api/auth/login": [
        () =>
          jsonResponse(
            errorPayload("LOCKED_OUT", "登录失败次数过多，请稍后再试", [], 900),
            423,
          ),
        () =>
          jsonResponse(
            errorPayload("INVALID_CREDENTIALS", "邮箱或密码不正确"),
            401,
          ),
      ],
    });

    const store = useAuthStore();
    await store.login({ email: "alice@example.com", password: "secret123" });
    expect(store.errorCode).toBe("LOCKED_OUT");
    expect(store.retryAfterSeconds).toBe(900);

    await store.login({ email: "alice@example.com", password: "secret123" });
    expect(store.errorCode).toBe("INVALID_CREDENTIALS");
    expect(store.retryAfterSeconds).toBeNull();
  });

  it("distinguishes a duplicate email from a generic failure", async () => {
    stubFetch({
      "/api/auth/register": [
        () =>
          jsonResponse(
            errorPayload("EMAIL_TAKEN", "该邮箱已被注册", [
              { field: "email", code: "TAKEN", message: "该邮箱已被注册" },
            ]),
            409,
          ),
      ],
    });

    const store = useAuthStore();
    const ok = await store.register({
      username: "alice",
      email: "alice@example.com",
      password: "secret123",
      displayName: "",
    });

    expect(ok).toBe(false);
    expect(store.errorCode).toBe("EMAIL_TAKEN");
    expect(store.fieldErrors["email"]).toBe("该邮箱已被注册");
    expect(store.isAuthenticated).toBe(false);
  });

  it("rejects a weak password before making any request", async () => {
    const fetchMock = stubFetch({});

    const store = useAuthStore();
    const ok = await store.register({
      username: "alice",
      email: "alice@example.com",
      password: "short",
      displayName: "",
    });

    expect(ok).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(store.errorCode).toBe("VALIDATION_FAILED");
    expect(store.fieldErrors["password"]).toBe("密码至少 8 个字符");
  });

  it("revokes on the server and clears local state even when the request fails", async () => {
    const fetchMock = stubFetch({
      "/api/auth/login": [() => jsonResponse(sessionPayload())],
      "/api/auth/logout": [
        () => {
          throw new TypeError("network down");
        },
      ],
    });
    vi.spyOn(console, "warn").mockImplementation(() => undefined);

    const store = useAuthStore();
    await store.login({ email: "alice@example.com", password: "secret123" });
    await store.logout();

    expect(store.isAuthenticated).toBe(false);
    expect(store.user).toBeNull();
    expect(window.localStorage.getItem(ACCESS_KEY)).toBeNull();
    expect(window.localStorage.getItem(REFRESH_KEY)).toBeNull();

    const { url, init } = callAt(fetchMock, 1);
    expect(url).toBe("/api/auth/logout");
    expect(init?.method).toBe("POST");
    expect(init?.headers).toEqual({
      Accept: "application/json",
      Authorization: "Bearer access-1",
    });
  });

  it("restores a persisted session and uses the stored token", async () => {
    window.localStorage.setItem(ACCESS_KEY, "stored-access");
    const fetchMock = stubFetch({
      "/api/auth/me": [() => jsonResponse(profilePayload())],
    });

    const store = useAuthStore();
    await store.restore();

    expect(store.isAuthenticated).toBe(true);
    expect(store.user?.id).toBe("01JABC1234567890ABCDEFGHJ1");

    const { url, init } = callAt(fetchMock, 0);
    expect(url).toBe("/api/auth/me");
    expect(init?.headers).toEqual({
      Accept: "application/json",
      Authorization: "Bearer stored-access",
    });
  });

  it("refreshes an expired access token during restore", async () => {
    window.localStorage.setItem(ACCESS_KEY, "expired-access");
    window.localStorage.setItem(REFRESH_KEY, "stored-refresh");

    const fetchMock = stubFetch({
      "/api/auth/me": [
        () =>
          jsonResponse(
            errorPayload("UNAUTHENTICATED", "登录状态已失效，请重新登录"),
            401,
          ),
        () => jsonResponse(profilePayload()),
      ],
      "/api/auth/refresh": [
        () =>
          jsonResponse({
            access_token: "access-2",
            refresh_token: "refresh-2",
            token_type: "Bearer",
            expires_in: 900,
          }),
      ],
    });

    const store = useAuthStore();
    await store.restore();

    expect(store.isAuthenticated).toBe(true);
    expect(store.accessToken).toBe("access-2");
    expect(window.localStorage.getItem(ACCESS_KEY)).toBe("access-2");

    const refresh = callAt(fetchMock, 1);
    expect(refresh.url).toBe("/api/auth/refresh");
    expect(JSON.parse(String(refresh.init?.body))).toEqual({
      refresh_token: "stored-refresh",
    });
  });

  it("keeps the session when the backend is unreachable", async () => {
    window.localStorage.setItem(ACCESS_KEY, "stored-access");
    window.localStorage.setItem(REFRESH_KEY, "stored-refresh");

    stubFetch({
      "/api/auth/me": [
        () => {
          throw new TypeError("Failed to fetch");
        },
      ],
    });

    const store = useAuthStore();
    await store.restore();

    expect(store.accessToken).toBe("stored-access");
    expect(window.localStorage.getItem(ACCESS_KEY)).toBe("stored-access");
    expect(store.errorMessage).toBe("无法连接服务器，请稍后重试");
  });

  it("discards the session when both tokens are rejected", async () => {
    window.localStorage.setItem(ACCESS_KEY, "dead-access");
    window.localStorage.setItem(REFRESH_KEY, "dead-refresh");

    stubFetch({
      "/api/auth/me": [
        () =>
          jsonResponse(
            errorPayload("UNAUTHENTICATED", "登录状态已失效，请重新登录"),
            401,
          ),
      ],
      "/api/auth/refresh": [
        () =>
          jsonResponse(
            errorPayload("UNAUTHENTICATED", "登录状态已失效，请重新登录"),
            401,
          ),
      ],
    });

    const store = useAuthStore();
    await store.restore();

    expect(store.isAuthenticated).toBe(false);
    expect(store.accessToken).toBeNull();
    expect(window.localStorage.getItem(ACCESS_KEY)).toBeNull();
    expect(window.localStorage.getItem(REFRESH_KEY)).toBeNull();
  });

  it("sends a reset request and treats every well-formed address the same", async () => {
    const fetchMock = stubFetch({
      "/api/auth/forgot-password": [
        () => jsonResponse({ accepted: true }, 202),
        () => jsonResponse({ accepted: true }, 202),
      ],
    });

    const store = useAuthStore();
    const known = await store.forgotPassword({ email: "  Alice@Example.COM " });
    const unknown = await store.forgotPassword({ email: "nobody@example.com" });

    expect(known).toBe(true);
    expect(unknown).toBe(true);
    expect(store.errorCode).toBeNull();

    const { url, init } = callAt(fetchMock, 0);
    expect(url).toBe("/api/auth/forgot-password");
    expect(JSON.parse(String(init?.body))).toEqual({
      email: "alice@example.com",
    });
  });

  it("rejects a malformed recovery address before any request", async () => {
    const fetchMock = stubFetch({});

    const store = useAuthStore();
    const ok = await store.forgotPassword({ email: "nope" });

    expect(ok).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(store.fieldErrors["email"]).toBe("邮箱格式不正确");
  });

  it("verifies an address and adopts the returned profile", async () => {
    const fetchMock = stubFetch({
      "/api/auth/verify-email": [
        () =>
          jsonResponse({
            id: "01JABC1234567890ABCDEFGHJ1",
            username: "alice",
            email: "alice@example.com",
            display_name: "alice",
            avatar_url: null,
            email_verified: true,
            created_at_ms: 1_700_000_000_000,
          }),
      ],
    });

    const store = useAuthStore();
    const profile = await store.verifyEmail("tok");

    expect(profile?.email_verified).toBe(true);
    expect(store.user?.email_verified).toBe(true);
    expect(store.emailVerified).toBe(true);

    const { url, init } = callAt(fetchMock, 0);
    expect(url).toBe("/api/auth/verify-email");
    expect(JSON.parse(String(init?.body))).toEqual({ token: "tok" });
  });

  it("surfaces an expired verification link distinctly", async () => {
    stubFetch({
      "/api/auth/verify-email": [
        () =>
          jsonResponse(
            errorPayload("TOKEN_EXPIRED", "链接已过期，请重新获取一封邮件"),
            410,
          ),
      ],
    });

    const store = useAuthStore();
    const profile = await store.verifyEmail("stale");

    expect(profile).toBeNull();
    expect(store.errorCode).toBe("TOKEN_EXPIRED");
  });

  it("resets the password, drops the dead session and never sends the confirmation", async () => {
    const fetchMock = stubFetch({
      "/api/auth/login": [() => jsonResponse(sessionPayload())],
      "/api/auth/reset-password": [() => new Response(null, { status: 204 })],
    });

    const store = useAuthStore();
    await store.login({ email: "alice@example.com", password: "secret123" });

    const ok = await store.resetPassword({
      token: "tok",
      password: "brand-new-1",
      confirm: "brand-new-1",
    });

    expect(ok).toBe(true);
    expect(store.isAuthenticated).toBe(false);
    expect(window.localStorage.getItem(ACCESS_KEY)).toBeNull();

    const { url, init } = callAt(fetchMock, 1);
    expect(url).toBe("/api/auth/reset-password");
    expect(JSON.parse(String(init?.body))).toEqual({
      token: "tok",
      password: "brand-new-1",
    });
  });

  it("rejects a mismatched confirmation without a request", async () => {
    const fetchMock = stubFetch({});

    const store = useAuthStore();
    const ok = await store.resetPassword({
      token: "tok",
      password: "secret123",
      confirm: "secret124",
    });

    expect(ok).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(store.fieldErrors["confirm"]).toBe("两次输入的密码不一致");
  });

  it("re-sends a verification link", async () => {
    const fetchMock = stubFetch({
      "/api/auth/resend-verification": [
        () => jsonResponse({ accepted: true }, 202),
      ],
    });

    const store = useAuthStore();
    const ok = await store.resendVerification({ email: "alice@example.com" });

    expect(ok).toBe(true);
    expect(callAt(fetchMock, 0).url).toBe("/api/auth/resend-verification");
  });

  it("records the whoami response from the protected endpoint", async () => {
    stubFetch({
      "/api/auth/login": [() => jsonResponse(sessionPayload())],
      // `fetchWhoAmI` confirms the session first, then calls the protected path.
      "/api/auth/me": [() => jsonResponse(profilePayload())],
      "/api/auth/whoami": [
        () =>
          jsonResponse({
            user_id: "01JABC1234567890ABCDEFGHJ1",
            session_id: "01JABC1234567890ABCDEFGHJ2",
            username: "alice",
          }),
      ],
    });

    const store = useAuthStore();
    await store.login({ email: "alice@example.com", password: "secret123" });
    const who = await store.fetchWhoAmI();

    expect(who?.session_id).toBe("01JABC1234567890ABCDEFGHJ2");
    expect(store.whoami?.user_id).toBe("01JABC1234567890ABCDEFGHJ1");
  });
});
