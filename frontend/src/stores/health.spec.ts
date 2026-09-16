import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useHealthStore } from "./health";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("useHealthStore", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("loads status and version from GET /api/health", async () => {
    const fetchMock = vi
      .fn<typeof fetch>()
      .mockResolvedValue(jsonResponse({ status: "ok", version: "0.1.0" }));
    vi.stubGlobal("fetch", fetchMock);

    const store = useHealthStore();
    await store.fetchHealth();

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledWith("/api/health", {
      headers: { Accept: "application/json" },
    });
    expect(store.status).toBe("ok");
    expect(store.version).toBe("0.1.0");
    expect(store.error).toBeNull();
    expect(store.loading).toBe(false);
  });

  it("reports an unreachable backend without throwing", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>().mockRejectedValue(new TypeError("Failed to fetch")),
    );

    const store = useHealthStore();
    await store.fetchHealth();

    expect(store.status).toBeNull();
    expect(store.version).toBeNull();
    expect(store.error).toBe("无法连接后端服务");
    expect(store.loading).toBe(false);
  });

  it("reports a non-2xx backend response", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn<typeof fetch>()
        .mockResolvedValue(jsonResponse({ error: "boom" }, 500)),
    );

    const store = useHealthStore();
    await store.fetchHealth();

    expect(store.status).toBeNull();
    expect(store.version).toBeNull();
    expect(store.error).toBe("服务端响应异常（HTTP 500）");
    expect(store.loading).toBe(false);
  });
});
