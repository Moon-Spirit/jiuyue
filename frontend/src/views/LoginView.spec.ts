import { flushPromises, mount } from "@vue/test-utils";
import { createPinia } from "pinia";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMemoryHistory, createRouter, type Router } from "vue-router";
import LoginView from "./LoginView.vue";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

/** An `ErrorBody` as the backend returns it, retry hint included. */
function errorPayload(
  code: string,
  message: string,
  retryAfterSeconds: number | null = null,
): unknown {
  return {
    error: {
      code,
      message,
      fields: [],
      retry_after_seconds: retryAfterSeconds,
    },
  };
}

function testRouter(): Router {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", name: "home", component: { template: "<div />" } },
      { path: "/login", name: "login", component: LoginView },
      {
        path: "/register",
        name: "register",
        component: { template: "<div />" },
      },
      {
        // The view links here; the route must resolve for the mount to work.
        path: "/forgot-password",
        name: "forgot-password",
        component: { template: "<div />" },
      },
    ],
  });
}

/**
 * Mount the login view over a real router with the network as the only fake —
 * the store, the request shape and the rendering are all really exercised.
 */
async function mountLogin() {
  const router = testRouter();
  await router.push("/login");
  await router.isReady();

  return mount(LoginView, {
    global: { plugins: [createPinia(), router] },
  });
}

type LoginWrapper = Awaited<ReturnType<typeof mountLogin>>;

async function submitLogin(wrapper: LoginWrapper): Promise<void> {
  await wrapper.find("input[type='email']").setValue("alice@example.com");
  await wrapper.find("input[type='password']").setValue("secret123");
  await wrapper.find("form").trigger("submit");
  await flushPromises();
}

/**
 * The provider list the login page fetches on mount.
 *
 * The existing tests below stub one responder for every request, so this serves
 * the mount-time call as well as any other; the OAuth tests install their own
 * fetch that answers both paths deliberately.
 */
function providersPayload(): unknown {
  return {
    providers: [
      {
        provider: "github",
        display_name: "GitHub",
        authorize_url: "https://github.com/login/oauth/authorize?state=s",
      },
    ],
  };
}

describe("LoginView", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
    window.sessionStorage.clear();
  });

  it("renders a button for each provider the server offers", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockImplementation(async () => jsonResponse(providersPayload())),
    );

    const wrapper = await mountLogin();
    await flushPromises();

    const section = wrapper.find('[data-test="oauth-providers"]');
    expect(section.exists()).toBe(true);
    expect(section.text()).toContain("GitHub");
    expect(wrapper.find('[data-test="oauth-github"]').exists()).toBe(true);
  });

  it("renders no third-party section when the instance offers no provider", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockImplementation(async () => jsonResponse({ providers: [] })),
    );

    const wrapper = await mountLogin();
    await flushPromises();

    expect(wrapper.find('[data-test="oauth-providers"]').exists()).toBe(false);
  });

  it("asks the server to start a sign-in when a provider button is clicked", async () => {
    const assign = vi.fn();
    const originalLocation = window.location;
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { assign, href: originalLocation.href },
    });

    const fetchMock = vi
      .fn<typeof fetch>()
      .mockImplementation(async (input) => {
        const url = String(input);
        if (url === "/api/auth/oauth/providers") {
          return jsonResponse(providersPayload());
        }
        if (url === "/api/auth/oauth/start") {
          return jsonResponse({
            authorize_url: "https://github.com/login/oauth/authorize?state=s2",
          });
        }
        throw new Error(`unexpected request: ${url}`);
      });
    vi.stubGlobal("fetch", fetchMock);

    try {
      const wrapper = await mountLogin();
      await flushPromises();

      await wrapper.find('[data-test="oauth-github"]').trigger("click");
      await flushPromises();

      expect(assign).toHaveBeenCalledWith(
        "https://github.com/login/oauth/authorize?state=s2",
      );
    } finally {
      Object.defineProperty(window, "location", {
        configurable: true,
        value: originalLocation,
      });
    }
  });

  it("shows the retry hint when the backend throttles the login", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockImplementation(async () =>
          jsonResponse(
            errorPayload(
              "TOO_MANY_ATTEMPTS",
              "登录尝试过于频繁，请稍后再试",
              42,
            ),
            429,
          ),
        ),
    );

    const wrapper = await mountLogin();
    await submitLogin(wrapper);

    const hint = wrapper.find('[data-test="retry-hint"]');
    expect(hint.exists()).toBe(true);
    expect(hint.text()).toContain("42");
    expect(wrapper.find('[data-test="form-error"]').text()).toContain(
      "登录尝试过于频繁",
    );
  });

  it("shows the retry hint for a lockout too", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockImplementation(async () =>
          jsonResponse(
            errorPayload("LOCKED_OUT", "登录失败次数过多，请稍后再试", 900),
            423,
          ),
        ),
    );

    const wrapper = await mountLogin();
    await submitLogin(wrapper);

    const hint = wrapper.find('[data-test="retry-hint"]');
    expect(hint.exists()).toBe(true);
    expect(hint.text()).toContain("900");
  });

  it("shows no retry hint for an ordinary rejection", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockImplementation(async () =>
          jsonResponse(
            errorPayload("INVALID_CREDENTIALS", "邮箱或密码不正确"),
            401,
          ),
        ),
    );

    const wrapper = await mountLogin();
    await submitLogin(wrapper);

    expect(wrapper.find('[data-test="form-error"]').text()).toBe(
      "邮箱或密码不正确",
    );
    expect(wrapper.find('[data-test="retry-hint"]').exists()).toBe(false);
  });
});
