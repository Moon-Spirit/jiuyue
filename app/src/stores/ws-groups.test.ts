import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { useAuthStore } from "./auth";
import { useGroupsStore } from "./groups";
import { useWsStore } from "./ws";

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

describe("ws store — M11 建群", () => {
  it("createGroup 落地群会话、发送邀请用户名并自动打开", async () => {
    fetchMock.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path === "/api/groups" && init?.method === "POST") {
        return jsonResponse(201, {
          conversation_id: 21,
          name: "团队",
          member_count: 2,
          invited: ["alice"],
        });
      }
      return jsonResponse(404, { error: "not_found" });
    });
    const ws = useWsStore();

    const conversation = await ws.createGroup("团队", ["alice"]);

    expect(conversation.kind).toBe("group");
    expect(conversation.name).toBe("团队");
    expect(conversation.peerUsername).toBe("");
    expect(ws.activeConversationId).toBe(21);
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/groups");
    expect(JSON.parse(String(init.body))).toEqual({
      name: "团队",
      invite_usernames: ["alice"],
    });
  });

  it("重复 conversation_id 时就地升级为群会话", async () => {
    fetchMock.mockImplementation(async (path: string) => {
      if (path === "/api/groups") {
        return jsonResponse(201, {
          conversation_id: 21,
          name: "团队",
          member_count: 2,
          invited: [],
        });
      }
      return jsonResponse(404, { error: "not_found" });
    });
    const ws = useWsStore();
    ws.conversations.push({
      conversationId: 21,
      peerUserId: "",
      peerUsername: "",
      lastMessagePreview: null,
      lastActivityAt: "",
      unread: 0,
      lastSeenSeq: 0,
      maxSeq: 0,
      peerTypingUntil: null,
    });

    await ws.createGroup("团队");

    expect(ws.conversations).toHaveLength(1);
    expect(ws.conversations[0]?.kind).toBe("group");
    expect(ws.conversations[0]?.name).toBe("团队");
  });
});

describe("ws store → groups store 帧委派", () => {
  it("group.invited 帧即时追加邀请并推高角标", async () => {
    const ws = useWsStore();
    await ws.connect();
    lastSocket().serverOpen();

    lastSocket().serverFrame({
      v: 1,
      t: "group.invited",
      d: {
        invite_id: "i1",
        conversation_id: 9,
        group_name: "游戏群",
        from: { user_id: "u1", username: "alice" },
      },
    });

    const groups = useGroupsStore();
    expect(groups.invites).toHaveLength(1);
    expect(groups.pendingInviteCount).toBe(1);
  });

  it("group.updated 帧触发会话列表刷新", async () => {
    const ws = useWsStore();
    await ws.connect();
    lastSocket().serverOpen();
    const spy = vi.spyOn(ws, "refreshListing");

    lastSocket().serverFrame({
      v: 1,
      t: "group.updated",
      d: { conversation_id: 9 },
    });

    await vi.waitFor(() => {
      expect(spy).toHaveBeenCalledWith();
    });
    spy.mockRestore();
  });
});
