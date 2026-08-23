import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { createI18n } from "vue-i18n";
import HomeView from "./HomeView.vue";

const i18n = createI18n({
  legacy: false,
  locale: "zh-CN",
  messages: {
    "zh-CN": { app: { name: "JiuYue", tagline: "t", toggleLocale: "s" } },
  },
});

describe("HomeView", () => {
  it("renders the product name", () => {
    const wrapper = mount(HomeView, { global: { plugins: [i18n] } });
    expect(wrapper.text()).toContain("JiuYue");
  });
});
