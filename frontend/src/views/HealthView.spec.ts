import { flushPromises, mount } from "@vue/test-utils";
import { createPinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "../testing/fake-websocket";
import HealthView from "./HealthView.vue";

function mountHealthView() {
  return mount(HealthView, { global: { plugins: [createPinia()] } });
}

/** A server envelope as it appears on the wire, produced from the contract. */
function pingEnvelope(seq: number, timeMs: number): string {
  return JSON.stringify({
    v: 1,
    s: seq,
    ts: timeMs,
    e: { t: "Ping", d: { seq, time_ms: timeMs } },
  });
}

describe("HealthView", () => {
  beforeEach(() => {
    FakeWebSocket.reset();
    // The view opens the realtime channel on mount; substitute the network
    // boundary so no real socket is created.
    vi.stubGlobal("WebSocket", FakeWebSocket);
    // The socket authenticates with the session's access token.
    window.localStorage.setItem("jiuyue.auth.access_token", "test-access");
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
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

  it("renders the ping received over the realtime channel", async () => {
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

    const socket = FakeWebSocket.latest();
    socket.emitOpen();
    socket.emitMessage(pingEnvelope(1, 1_750_000_000_000));
    await flushPromises();

    expect(wrapper.find('[data-test="realtime-panel"]').exists()).toBe(true);
    expect(wrapper.find('[data-test="last-sequence"]').text()).toBe("1");
    expect(wrapper.find('[data-test="last-ping"]').text()).not.toBe("—");
    expect(wrapper.text()).toContain("已连接");
  });
});
