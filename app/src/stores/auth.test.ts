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
        uid: 100023,
      }),
    );

    const store = useAuthStore();
    await store.login("alice@example.com", "password123");

    expect(store.accessToken).toBe("a1");
    expect(store.status).toBe("authed");
    expect(store.user?.userId).toBe("01a03367-0000-7000-8000-00000000aaaa");
    expect(store.user?.username).toBe("alice");
    expect(store.user?.uid).toBe(100023);
    expect(localStorage.getItem("jiuyue.refresh")).toBe("r1");
  });

  it("register stores the freshly-assigned uid in the user profile", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(201, {
        access_token: "a-reg",
        refresh_token: "r-reg",
        expires_in: 900,
        user_id: "01a03367-0000-7000-8000-00000000bbbb",
        username: "newbie",
        uid: 100456,
      }),
    );

    const store = useAuthStore();
    await store.register({
      channel: "email",
      target: "new@example.com",
      code: "123456",
      username: "newbie",
      password: "password123",
    });

    expect(store.user?.uid).toBe(100456);
    expect(localStorage.getItem("jiuyue.user")).toContain("100456");
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

  it("restores a pre-UID persisted profile with uid 0 (backfilled next login)", async () => {
    // Profile written before the UID field existed — must not break boot.
    localStorage.setItem(
      "jiuyue.user",
      JSON.stringify({ userId: "u-old", username: "oldbie" }),
    );
    localStorage.setItem("jiuyue.refresh", "old-refresh");
    fetchMock.mockResolvedValue(
      jsonResponse(200, {
        access_token: "a3",
        refresh_token: "r3",
        expires_in: 900,
      }),
    );

    const store = useAuthStore();
    await store.ensureAccessToken();

    expect(store.user).toEqual({
      userId: "u-old",
      username: "oldbie",
      uid: 0,
      displayName: "",
      avatar: null,
      bio: "",
    });
    expect(store.status).toBe("authed");
  });

  it("round-trips the editable profile fields through persisted USER_KEY", async () => {
    localStorage.setItem(
      "jiuyue.user",
      JSON.stringify({
        userId: "u-r",
        username: "roundtrip",
        uid: 4242,
        displayName: "Round Trip",
        avatar: "🐳",
        bio: "正在环游世界",
      }),
    );
    localStorage.setItem("jiuyue.refresh", "old-refresh");
    fetchMock.mockResolvedValue(
      jsonResponse(200, {
        access_token: "a4",
        refresh_token: "r4",
        expires_in: 900,
      }),
    );

    const store = useAuthStore();
    await store.ensureAccessToken();

    expect(store.user?.displayName).toBe("Round Trip");
    expect(store.user?.avatar).toBe("🐳");
    expect(store.user?.bio).toBe("正在环游世界");

    // A fresh store boots from the same persisted JSON and sees them again.
    const revived = useAuthStore();
    revived.user = null;
    await revived.ensureAccessToken();
    expect(revived.user).toEqual(store.user);
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
  it("logout wipes the previous account's ws/friends data and scoped storage", async () => {
    const auth = useAuthStore();
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/auth/ws-ticket"))
        return jsonResponse(200, { ticket: "t1" });
      return jsonResponse(200, {
        access_token: "a1",
        refresh_token: "r1",
        expires_in: 900,
        user_id: "u-1",
        username: "alice",
      });
    });
    await auth.login("alice", "password123");

    const { useWsStore } = await import("./ws");
    const { useFriendsStore } = await import("./friends");
    const ws = useWsStore();
    const friends = useFriendsStore();
    ws.conversations.push({
      conversationId: 1,
      peerUserId: "p1",
      peerUsername: "bob",
      lastMessagePreview: "hi",
      lastActivityAt: "",
      unread: 0,
      lastSeenSeq: 0,
      maxSeq: 0,
      kind: "direct",
      peerTypingUntil: null,
    });
    ws.messagesByConversation[1] = [];
    localStorage.setItem(
      "jiuyue.convos",
      JSON.stringify([{ conversationId: 1 }]),
    );
    localStorage.setItem("jiuyue.msgs.1", "[]");
    localStorage.setItem("jiuyue.e2ee.identity", "identity-bytes");
    friends.friends = [{ user_id: "p1", username: "bob", since: "2026" }];
    friends.loaded = true;

    await auth.logout();

    expect(auth.status).toBe("anon");
    expect(ws.conversations).toHaveLength(0);
    expect(friends.friends).toHaveLength(0);
    expect(friends.loaded).toBe(false);
    expect(localStorage.getItem("jiuyue.convos")).toBeNull();
    expect(localStorage.getItem("jiuyue.msgs.1")).toBeNull();
    expect(localStorage.getItem("jiuyue.e2ee.identity")).toBeNull();
  });
});
