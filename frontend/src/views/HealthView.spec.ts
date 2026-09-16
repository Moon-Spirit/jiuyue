import { flushPromises, mount } from "@vue/test-utils";
import { createPinia } from "pinia";
import { afterEach, describe, expect, it, vi } from "vitest";
import HealthView from "./HealthView.vue";

function mountHealthView() {
  return mount(HealthView, { global: { plugins: [createPinia()] } });
}

describe("HealthView", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders the backend status and version when the backend answers", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockResolvedValue(
        new Response(JSON.stringify({ status: "ok", version: "0.1.0" }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
      ),
    );

    const wrapper = mountHealthView();
    await flushPromises();

    expect(wrapper.text()).toContain("ok");
    expect(wrapper.text()).toContain("0.1.0");
  });

  it("renders a visible error state when the backend is unreachable", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockRejectedValue(new TypeError("Failed to fetch")),
    );

    const wrapper = mountHealthView();
    await flushPromises();

    expect(wrapper.text()).toContain("连接失败");
    expect(wrapper.text()).toContain("无法连接后端服务");
  });
});
