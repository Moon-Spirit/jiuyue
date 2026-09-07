import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { useAuthStore } from "./auth";
import { useFriendsStore } from "./friends";
import { useWsStore } from "./ws";

/**
 * Verifies the ws store delegates friend.* frames to the friends store:
 * `friend.requested` must bump the pending badge (pendingCount) and
 * `friend.accepted` must materialize the friendship locally.
 */

class MockWebSocket {
  static instances: MockWebSocket[] = [];

  url: string;
  readyState = 0;
  sent: string[] = [];
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;

  constructor(url: string | URL) {
    this.url = String(url);
    MockWebSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.onclose?.({});
  }

  serverOpen(): void {
    this.readyState = 1;
    this.onopen?.({});
  }

  serverFrame(frame: unknown): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
}

const fetchMock = vi.fn();

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function lastSocket(): MockWebSocket {
  const sock = MockWebSocket.instances.at(-1);
  if (sock === undefined) throw new Error("no MockWebSocket instance");
  return sock;
}

beforeEach(() => {
  localStorage.clear();
  MockWebSocket.instances = [];
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("WebSocket", MockWebSocket);
  setActivePinia(createPinia());
  // Ticket + conversation bootstrap both succeed cheaply.
  fetchMock.mockImplementation(async (path: string) => {
    if (path === "/api/auth/ws-ticket")
      return jsonResponse(200, { ticket: "t" });
    if (path === "/api/conversations") return jsonResponse(200, []);
    return jsonResponse(404, { error: "not_found" });
  });
  const auth = useAuthStore();
  auth.accessToken = "test-token";
  auth.user = { userId: "me-1", username: "me", uid: 1000007 };
});

afterEach(() => {
  useWsStore().dispose();
  vi.unstubAllGlobals();
});

describe("ws store → friends store 帧委派", () => {
  it("friend.requested 帧推高待处理角标", async () => {
    const ws = useWsStore();
    await ws.connect();
    lastSocket().serverOpen();

    lastSocket().serverFrame({
      v: 1,
      t: "friend.requested",
      d: { request_id: "r1", from: { user_id: "u1", username: "alice" } },
    });

    const friends = useFriendsStore();
    expect(friends.incoming).toHaveLength(1);
    expect(friends.incoming[0]?.from.username).toBe("alice");
    expect(friends.pendingCount).toBe(1);
  });

  it("friend.accepted 帧落地好友关系并清空外发申请", async () => {
    const ws = useWsStore();
    await ws.connect();
    lastSocket().serverOpen();

    const friends = useFriendsStore();
    friends.outgoing.push({
      request_id: "out-1",
      to: { user_id: "u2", username: "bob" },
      created_at: new Date().toISOString(),
    });

    lastSocket().serverFrame({
      v: 1,
      t: "friend.accepted",
      d: { friend: { user_id: "u2", username: "bob" } },
    });

    expect(friends.friends.map((f) => f.username)).toEqual(["bob"]);
    expect(friends.outgoing).toHaveLength(0);
  });
});
