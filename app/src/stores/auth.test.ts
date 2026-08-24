import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { useAuthStore } from "./auth";

const fetchMock = vi.fn();

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

beforeEach(() => {
  setActivePinia(createPinia());
  localStorage.clear();
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});

describe("auth store", () => {
  it("stores tokens and marks the session authed on successful login", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, {
        access_token: "a1",
        refresh_token: "r1",
        expires_in: 900,
        // Login now carries the profile (server-side change, ticket 08 fix).
        user_id: "01a03367-0000-7000-8000-00000000aaaa",
        username: "alice",
      }),
    );

    const store = useAuthStore();
    await store.login("alice@example.com", "password123");

    expect(store.accessToken).toBe("a1");
    expect(store.status).toBe("authed");
    expect(store.user?.userId).toBe("01a03367-0000-7000-8000-00000000aaaa");
    expect(store.user?.username).toBe("alice");
    expect(localStorage.getItem("jiuyue.refresh")).toBe("r1");
  });

  it("surfaces invalid_credentials and returns to anon when login fails", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(401, { error: "invalid_credentials", message: "nope" }),
    );

    const store = useAuthStore();

    await expect(
      store.login("alice@example.com", "wrong-password"),
    ).rejects.toMatchObject({
      machine: "invalid_credentials",
      status: 401,
    });
    expect(store.status).toBe("anon");
    expect(store.accessToken).toBeNull();
    expect(localStorage.getItem("jiuyue.refresh")).toBeNull();
  });

  it("refreshes once from storage and reuses the cached access token afterwards", async () => {
    localStorage.setItem("jiuyue.refresh", "old-refresh");
    fetchMock.mockResolvedValue(
      jsonResponse(200, {
        access_token: "a2",
        refresh_token: "r2",
        expires_in: 900,
      }),
    );

    const store = useAuthStore();
    const first = await store.ensureAccessToken();
    const second = await store.ensureAccessToken();

    expect(first).toBe("a2");
    expect(second).toBe("a2");
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(localStorage.getItem("jiuyue.refresh")).toBe("r2");
    expect(store.status).toBe("authed");
  });

  it("clears the local session when the stored refresh token is rejected", async () => {
    localStorage.setItem("jiuyue.refresh", "dead-token");
    fetchMock.mockResolvedValue(
      jsonResponse(401, { error: "invalid_credentials", message: "expired" }),
    );

    const store = useAuthStore();

    await expect(store.ensureAccessToken()).resolves.toBeNull();
    expect(localStorage.getItem("jiuyue.refresh")).toBeNull();
    expect(store.accessToken).toBeNull();
    expect(store.status).toBe("anon");
  });

  it("stays anonymous without touching the network when no refresh token exists", async () => {
    const store = useAuthStore();

    await expect(store.ensureAccessToken()).resolves.toBeNull();
    expect(fetchMock).not.toHaveBeenCalled();
    expect(store.status).toBe("anon");
  });
});
