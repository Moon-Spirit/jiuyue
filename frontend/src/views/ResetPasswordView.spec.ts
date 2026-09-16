import { flushPromises, mount } from "@vue/test-utils";
import { createPinia } from "pinia";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMemoryHistory, createRouter, type Router } from "vue-router";
import ResetPasswordView from "./ResetPasswordView.vue";

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
        path: "/reset-password",
        name: "reset-password",
        component: ResetPasswordView,
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

async function mountView(query = "?token=tok") {
  const router = testRouter();
  await router.push(`/reset-password${query}`);
  await router.isReady();

  return mount(ResetPasswordView, {
    global: { plugins: [createPinia(), router] },
  });
}

type Wrapper = Awaited<ReturnType<typeof mountView>>;

async function fillAndSubmit(
  wrapper: Wrapper,
  password: string,
  confirm: string,
): Promise<void> {
  const inputs = wrapper.findAll("input[type='password']");
  await inputs[0]?.setValue(password);
  await inputs[1]?.setValue(confirm);
  await wrapper.find("form").trigger("submit");
  await flushPromises();
}

describe("ResetPasswordView", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  it("submits the token and the new password, then shows the done state", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetchMock);

    const wrapper = await mountView();
    await fillAndSubmit(wrapper, "brand-new-1", "brand-new-1");

    expect(wrapper.find('[data-test="done"]').exists()).toBe(true);

    const call = fetchMock.mock.calls[0];
    expect(String(call?.[0])).toBe("/api/auth/reset-password");
    expect(JSON.parse(String(call?.[1]?.body))).toEqual({
      token: "tok",
      password: "brand-new-1",
    });
  });

  it("rejects a mismatched confirmation without a request", async () => {
    const fetchMock = vi.fn<typeof fetch>();
    vi.stubGlobal("fetch", fetchMock);

    const wrapper = await mountView();
    await fillAndSubmit(wrapper, "secret123", "secret124");

    expect(fetchMock).not.toHaveBeenCalled();
    expect(wrapper.text()).toContain("两次输入的密码不一致");
    expect(wrapper.find('[data-test="done"]').exists()).toBe(false);
  });

  it("explains a spent or expired link and offers a fresh one", async () => {
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

    const wrapper = await mountView();
    await fillAndSubmit(wrapper, "brand-new-1", "brand-new-1");

    expect(wrapper.find('[data-test="form-error"]').text()).toContain(
      "链接已过期",
    );
    const link = wrapper.find("a[href='/forgot-password']");
    expect(link.exists()).toBe(true);
  });

  it("refuses to render the form when the link carried no token", async () => {
    const fetchMock = vi.fn<typeof fetch>();
    vi.stubGlobal("fetch", fetchMock);

    const wrapper = await mountView("");

    expect(wrapper.find('[data-test="missing-token"]').exists()).toBe(true);
    expect(wrapper.find("form").exists()).toBe(false);
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
