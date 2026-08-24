import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import { i18n } from "../i18n";
import RegisterView from "./RegisterView.vue";

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
});
