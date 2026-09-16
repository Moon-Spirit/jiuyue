import { flushPromises, mount } from "@vue/test-utils";
import { createPinia } from "pinia";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMemoryHistory, createRouter, type Router } from "vue-router";
import VerifyEmailView from "./VerifyEmailView.vue";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function profile(verified: boolean): unknown {
  return {
    id: "01JABC1234567890ABCDEFGHJ1",
    username: "alice",
    email: "alice@example.com",
    display_name: "alice",
    avatar_url: null,
    email_verified: verified,
    created_at_ms: 1_700_000_000_000,
  };
}

function testRouter(): Router {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", name: "chat", component: { template: "<div />" } },
      {
        path: "/verify-email",
        name: "verify-email",
        component: VerifyEmailView,
      },
      {
        path: "/forgot-password",
        name: "forgot-password",
        component: { template: "<div />" },
      },
      { path: "/login", name: "login", component: { template: "<div />" } },
    ],
  });
}

async function mountAt(query = "") {
  const router = testRouter();
  await router.push(`/verify-email${query}`);
  await router.isReady();

  const wrapper = mount(VerifyEmailView, {
    global: { plugins: [createPinia(), router] },
  });
  await flushPromises();

  return wrapper;
}

describe("VerifyEmailView", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  it("redeems the token on mount and reports success", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(jsonResponse(profile(true)));
    vi.stubGlobal("fetch", fetchMock);

    const wrapper = await mountAt("?token=tok");

    expect(wrapper.find('[data-test="verified"]').exists()).toBe(true);
    const call = fetchMock.mock.calls[0];
    expect(String(call?.[0])).toBe("/api/auth/verify-email");
    expect(JSON.parse(String(call?.[1]?.body))).toEqual({ token: "tok" });
  });

  it("shows the server's reason when the link cannot be redeemed", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        jsonResponse(
          {
            error: {
              code: "TOKEN_EXPIRED",
              message: "链接已过期，请重新获取一封邮件",
              fields: [],
              retry_after_seconds: null,
            },
          },
          410,
        ),
      ),
    );

    const wrapper = await mountAt("?token=stale");

    expect(wrapper.find('[data-test="verify-error"]').exists()).toBe(true);
    expect(wrapper.find('[data-test="verify-error"]').text()).toContain(
      "链接已过期",
    );
  });

  it("shows the inbox state and re-sends a link on request", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(jsonResponse({ accepted: true }, 202));
    vi.stubGlobal("fetch", fetchMock);

    const wrapper = await mountAt();
    expect(wrapper.find('[data-test="inbox"]').exists()).toBe(true);

    await wrapper.find("input[type='email']").setValue("alice@example.com");
    await wrapper.find("form").trigger("submit");
    await flushPromises();

    expect(wrapper.find('[data-test="resent"]').exists()).toBe(true);
    const call = fetchMock.mock.calls[0];
    expect(String(call?.[0])).toBe("/api/auth/resend-verification");
  });
});
