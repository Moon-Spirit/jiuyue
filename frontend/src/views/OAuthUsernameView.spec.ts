import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createMemoryHistory, createRouter, type Router } from "vue-router";
import OAuthUsernameView from "./OAuthUsernameView.vue";
import { useAuthStore } from "../stores/auth";

const LIMITED_KEY = "jiuyue.auth.oauth_limited_token";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function errorPayload(
  code: string,
  message: string,
  fields: readonly { field: string; code: string; message: string }[] = [],
): unknown {
  return { error: { code, message, fields, retry_after_seconds: null } };
}

function sessionPayload(): unknown {
  return {
    user: {
      id: "01JABC1234567890ABCDEFGHJ1",
      username: "chosen_name",
      email: "octocat@example.com",
      display_name: "chosen_name",
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
        path: "/oauth/username",
        name: "oauth-username",
        component: OAuthUsernameView,
      },
    ],
  });
}

async function mountUsername(): Promise<{
  wrapper: Awaited<ReturnType<typeof mount>>;
  router: Router;
}> {
  const router = testRouter();
  await router.push("/oauth/username");
  await router.isReady();

  const wrapper = mount(OAuthUsernameView, {
    global: { plugins: [createPinia(), router] },
  });
  await flushPromises();

  return { wrapper, router };
}

describe("OAuthUsernameView", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    window.localStorage.clear();
    window.sessionStorage.clear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("claims the username with the limited token and enters the app", async () => {
    window.sessionStorage.setItem(LIMITED_KEY, "limited-1");
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(jsonResponse({ session: sessionPayload() }));
    vi.stubGlobal("fetch", fetchMock);

    const { wrapper, router } = await mountUsername();

    await wrapper.find('[data-test="username-input"]').setValue("chosen_name");
    await wrapper.find("form").trigger("submit");
    await flushPromises();

    expect(router.currentRoute.value.name).toBe("chat");

    const call = fetchMock.mock.calls[0];
    expect(String(call?.[0])).toBe("/api/auth/oauth/complete");
    expect(JSON.parse(String(call?.[1]?.body))).toEqual({
      username: "chosen_name",
      display_name: null,
    });
    expect(window.sessionStorage.getItem(LIMITED_KEY)).toBeNull();
  });

  it("shows a taken username as a field problem and keeps the limited token", async () => {
    window.sessionStorage.setItem(LIMITED_KEY, "limited-1");
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockResolvedValue(
          jsonResponse(
            errorPayload("USERNAME_TAKEN", "该用户名已被占用", [
              { field: "username", code: "TAKEN", message: "该用户名已被占用" },
            ]),
            409,
          ),
        ),
    );

    const { wrapper, router } = await mountUsername();

    await wrapper.find('[data-test="username-input"]').setValue("taken_name");
    await wrapper.find("form").trigger("submit");
    await flushPromises();

    expect(wrapper.find('[data-test="username-error"]').text()).toBe(
      "该用户名已被占用",
    );
    expect(router.currentRoute.value.name).toBe("oauth-username");
    expect(window.sessionStorage.getItem(LIMITED_KEY)).toBe("limited-1");
  });

  it("rejects a malformed username without calling the server", async () => {
    window.sessionStorage.setItem(LIMITED_KEY, "limited-1");
    const fetchMock = vi.fn<typeof fetch>();
    vi.stubGlobal("fetch", fetchMock);

    const { wrapper } = await mountUsername();

    await wrapper.find('[data-test="username-input"]').setValue("Has Space");
    await wrapper.find("form").trigger("submit");
    await flushPromises();

    expect(fetchMock).not.toHaveBeenCalled();
    expect(wrapper.find('[data-test="username-error"]').exists()).toBe(true);
  });

  it("hides the form when the limited session was lost", async () => {
    const fetchMock = vi.fn<typeof fetch>();
    vi.stubGlobal("fetch", fetchMock);

    const { wrapper } = await mountUsername();

    expect(wrapper.find('[data-test="missing-token"]').exists()).toBe(true);
    expect(wrapper.find("form").exists()).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("keeps the store's limited token in step with session storage", async () => {
    window.sessionStorage.setItem(LIMITED_KEY, "limited-1");
    const store = useAuthStore();
    expect(store.limitedToken).toBe("limited-1");

    store.clear();
    expect(store.limitedToken).toBeNull();
    expect(window.sessionStorage.getItem(LIMITED_KEY)).toBeNull();
  });
});
