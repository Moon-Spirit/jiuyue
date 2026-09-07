import { describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import { i18n } from "../i18n";
import RegisterView from "./RegisterView.vue";

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

async function mountView() {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/login", component: { template: "<div />" } },
      { path: "/chat", component: { template: "<div />" } },
      { path: "/register", component: RegisterView },
    ],
  });
  await router.push("/register");
  await router.isReady();

  return mount(RegisterView, {
    global: { plugins: [createPinia(), i18n, router] },
  });
}

describe("RegisterView", () => {
  it("flags an invalid username inline and keeps submit disabled", async () => {
    const wrapper = await mountView();

    await wrapper.find('[data-testid="register-username"]').setValue("AB");

    const hint = wrapper.find('[data-testid="register-username-hint"]');
    expect(hint.classes()).toContain("text-red-500");
    expect(hint.text().length).toBeGreaterThan(0);
    expect(
      wrapper.find('[data-testid="register-submit"]').attributes("disabled"),
    ).toBeDefined();
  });

  it("enables submit once every field passes client-side validation", async () => {
    const wrapper = await mountView();

    await wrapper
      .find('[data-testid="register-target"]')
      .setValue("user@example.com");
    await wrapper.find('[data-testid="register-code"]').setValue("000000");
    await wrapper
      .find('[data-testid="register-username"]')
      .setValue("alice_01");
    await wrapper
      .find('[data-testid="register-password"]')
      .setValue("super-secret-8");

    expect(
      wrapper.find('[data-testid="register-submit"]').attributes("disabled"),
    ).toBeUndefined();
  });

  it("switches target semantics when the phone channel tab is selected", async () => {
    const wrapper = await mountView();

    // Email channel by default: a bare number is not a valid email yet.
    await wrapper
      .find('[data-testid="register-target"]')
      .setValue("13800138000");
    expect(
      wrapper.find('[data-testid="send-code"]').attributes("disabled"),
    ).toBeDefined();

    await wrapper.find('[data-testid="channel-phone"]').trigger("click");
    expect(
      wrapper.find('[data-testid="send-code"]').attributes("disabled"),
    ).toBeUndefined();
  });

  it("shows the freshly-assigned UID after a successful registration", async () => {
    const fetchMock = vi.fn();
    fetchMock.mockResolvedValue(
      jsonResponse(201, {
        access_token: "a1",
        refresh_token: "r1",
        expires_in: 900,
        user_id: "01a03367-0000-7000-8000-00000000cccc",
        username: "carol",
        uid: 100456,
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    setActivePinia(createPinia());

    const wrapper = await mountView();

    await wrapper
      .find('[data-testid="register-target"]')
      .setValue("carol@example.com");
    await wrapper.find('[data-testid="register-code"]').setValue("000000");
    await wrapper
      .find('[data-testid="register-username"]')
      .setValue("carol_01");
    await wrapper
      .find('[data-testid="register-password"]')
      .setValue("super-secret-8");
    await wrapper.find("form").trigger("submit");

    await vi.waitFor(() => {
      expect(wrapper.find('[data-testid="register-success"]').exists()).toBe(
        true,
      );
    });
    expect(wrapper.find('[data-testid="register-uid"]').text()).toContain(
      "100456",
    );
    expect(wrapper.find('[data-testid="register-uid"]').text()).toContain(
      i18n.global.t("register.uidIntro", { uid: "100456" }),
    );
    vi.unstubAllGlobals();
  });
});
