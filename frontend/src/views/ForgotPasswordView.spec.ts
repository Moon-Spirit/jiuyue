import { flushPromises, mount } from "@vue/test-utils";
import { createPinia } from "pinia";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMemoryHistory, createRouter, type Router } from "vue-router";
import ForgotPasswordView from "./ForgotPasswordView.vue";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function testRouter(): Router {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", name: "chat", component: { template: "<div />" } },
      {
        path: "/forgot-password",
        name: "forgot-password",
        component: ForgotPasswordView,
      },
      { path: "/login", name: "login", component: { template: "<div />" } },
    ],
  });
}

/**
 * Mount the view over a real router, faking only the network — the store, the
 * request shape and the rendering are all really exercised.
 */
async function mountView() {
  const router = testRouter();
  await router.push("/forgot-password");
  await router.isReady();

  return mount(ForgotPasswordView, {
    global: { plugins: [createPinia(), router] },
  });
}

type Wrapper = Awaited<ReturnType<typeof mountView>>;

async function submit(wrapper: Wrapper, email: string): Promise<void> {
  await wrapper.find("input[type='email']").setValue(email);
  await wrapper.find("form").trigger("submit");
  await flushPromises();
}

describe("ForgotPasswordView", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  it("shows the sent state and posts the normalised address", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(jsonResponse({ accepted: true }, 202));
    vi.stubGlobal("fetch", fetchMock);

    const wrapper = await mountView();
    await submit(wrapper, "  Alice@Example.COM ");

    expect(wrapper.find('[data-test="sent"]').exists()).toBe(true);
    expect(wrapper.find('[data-test="sent"]').text()).toContain("已发送");

    const call = fetchMock.mock.calls[0];
    expect(String(call?.[0])).toBe("/api/auth/forgot-password");
    const init = call?.[1];
    expect(JSON.parse(String(init?.body))).toEqual({
      email: "alice@example.com",
    });
  });

  it("rejects a malformed address without making a request", async () => {
    const fetchMock = vi.fn<typeof fetch>();
    vi.stubGlobal("fetch", fetchMock);

    const wrapper = await mountView();
    await submit(wrapper, "nope");

    expect(fetchMock).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("邮箱格式不正确");
    expect(wrapper.find('[data-test="sent"]').exists()).toBe(false);
  });

  it("shows the server message when the request is refused", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        jsonResponse(
          {
            error: {
              code: "TOO_MANY_ATTEMPTS",
              message: "请求过于频繁，请稍后再试",
              fields: [],
              retry_after_seconds: 30,
            },
          },
          429,
        ),
      ),
    );

    const wrapper = await mountView();
    await submit(wrapper, "alice@example.com");

    expect(wrapper.find('[data-test="form-error"]').text()).toContain(
      "请求过于频繁",
    );
    expect(wrapper.find('[data-test="sent"]').exists()).toBe(false);
  });
});
