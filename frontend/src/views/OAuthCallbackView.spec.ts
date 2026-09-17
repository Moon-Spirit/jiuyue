import { flushPromises, mount } from "@vue/test-utils";
import { createPinia } from "pinia";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMemoryHistory, createRouter, type Router } from "vue-router";
import OAuthCallbackView from "./OAuthCallbackView.vue";
import OAuthUsernameView from "./OAuthUsernameView.vue";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function errorPayload(code: string, message: string): unknown {
  return { error: { code, message, fields: [], retry_after_seconds: null } };
}

type Json = Record<string, unknown>;

function sessionPayload(): Json {
  return {
    user: {
      id: "01JABC1234567890ABCDEFGHJ1",
      username: "alice",
      email: "alice@example.com",
      display_name: "alice",
      avatar_url: null,
      email_verified: true,
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

function testRouter(): Router {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", name: "chat", component: { template: "<div />" } },
      { path: "/login", name: "login", component: { template: "<div />" } },
      {
        path: "/oauth/callback",
        name: "oauth-callback",
        component: OAuthCallbackView,
      },
      {
        path: "/oauth/username",
        name: "oauth-username",
        component: OAuthUsernameView,
      },
    ],
  });
}

async function mountCallback(
  query: Record<string, string>,
): Promise<{ wrapper: Awaited<ReturnType<typeof mount>>; router: Router }> {
  const router = testRouter();
  await router.push({ path: "/oauth/callback", query });
  await router.isReady();

  const wrapper = mount(OAuthCallbackView, {
    global: { plugins: [createPinia(), router] },
  });
  await flushPromises();

  return { wrapper, router };
}

describe("OAuthCallbackView", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
    window.sessionStorage.clear();
  });

  it("redeems the code and routes a finished sign-in into the app", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(
        jsonResponse({ session: { ...sessionPayload(), redirect_path: null } }),
      );
    vi.stubGlobal("fetch", fetchMock);

    const { router } = await mountCallback({
      provider: "github",
      code: "the-code",
      state: "the-state",
    });

    expect(router.currentRoute.value.name).toBe("chat");

    const call = fetchMock.mock.calls[0];
    expect(String(call?.[0])).toBe("/api/auth/oauth/callback");
    expect(JSON.parse(String(call?.[1]?.body))).toEqual({
      provider: "github",
      code: "the-code",
      state: "the-state",
    });
  });

  it("routes a first-time sign-in to the username step", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        jsonResponse({
          onboarding: {
            limited_token: "limited-1",
            suggested_username: "octocat",
            email: null,
            redirect_path: null,
          },
        }),
      ),
    );

    const { router } = await mountCallback({
      provider: "github",
      code: "the-code",
      state: "the-state",
    });

    expect(router.currentRoute.value.name).toBe("oauth-username");
    expect(
      window.sessionStorage.getItem("jiuyue.auth.oauth_limited_token"),
    ).toBe("limited-1");
  });

  it("explains a malformed redirect instead of calling the server", async () => {
    const fetchMock = vi.fn<typeof fetch>();
    vi.stubGlobal("fetch", fetchMock);

    const { wrapper } = await mountCallback({ provider: "github" });

    expect(fetchMock).not.toHaveBeenCalled();
    expect(wrapper.find('[data-test="oauth-error"]').exists()).toBe(true);
  });

  it("shows the refusal when the account-take-over guard fires", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockResolvedValue(
          jsonResponse(
            errorPayload(
              "OAUTH_ACCOUNT_EXISTS",
              "该邮箱已注册，请用原来的方式登录后在设置中绑定第三方账号",
            ),
            409,
          ),
        ),
    );

    const { wrapper } = await mountCallback({
      provider: "github",
      code: "the-code",
      state: "the-state",
    });

    expect(wrapper.find('[data-test="oauth-error"]').text()).toContain(
      "该邮箱已注册",
    );
  });

  it("shows the refusal when the state was replayed", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockResolvedValue(
          jsonResponse(
            errorPayload(
              "OAUTH_STATE_INVALID",
              "登录请求已失效，请重新发起第三方登录",
            ),
            400,
          ),
        ),
    );

    const { wrapper } = await mountCallback({
      provider: "github",
      code: "the-code",
      state: "stale",
    });

    expect(wrapper.find('[data-test="oauth-error"]').text()).toContain(
      "登录请求已失效",
    );
  });
});
