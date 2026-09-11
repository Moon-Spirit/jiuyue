import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { useAuthStore } from "./auth";
import { useGroupsStore } from "./groups";
import { useWsStore } from "./ws";

const fetchMock = vi.fn();

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function emptyResponse(status: number): Response {
  return new Response(null, { status });
}

beforeEach(() => {
  localStorage.clear();
  setActivePinia(createPinia());
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
  const auth = useAuthStore();
  auth.accessToken = "test-token";
  auth.user = { userId: "me-1", username: "me", uid: 1000007 };
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const groupInfo = {
  conversation_id: 5,
  name: "团队",
  my_role: "owner" as const,
  member_count: 2,
  members: [
    {
      user_id: "me-1",
      username: "me",
      display_name: "我",
      avatar: "💎",
      role: "owner" as const,
      joined_at: "2026-01-01T00:00:00Z",
    },
    {
      user_id: "u2",
      username: "alice",
      role: "member" as const,
      joined_at: "2026-01-02T00:00:00Z",
    },
  ],
};

describe("groups store — 群信息缓存与成员身份映射", () => {
  it("fetchInfo 缓存名册并生成成员名称/头像映射", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, groupInfo));
    const groups = useGroupsStore();

    const info = await groups.fetchInfo(5);

    expect(info?.name).toBe("团队");
    expect(groups.infos[5]?.my_role).toBe("owner");
    // display_name wins for the sender label; username is the fallback.
    expect(groups.memberNames[5]?.["me-1"]).toBe("我");
    expect(groups.memberNames[5]?.["u2"]).toBe("alice");
    expect(groups.memberAvatars[5]?.["me-1"]).toBe("💎");
    expect(groups.memberAvatars[5]?.["u2"]).toBeNull();
  });

  it("成员操作后自动重新拉取名册", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, groupInfo));
    const groups = useGroupsStore();

    await groups.kickMember(5, "u2");

    const paths = fetchMock.mock.calls.map(([p]) => p);
    expect(paths).toEqual(["/api/groups/5/members/u2/kick", "/api/groups/5"]);
  });

  it("updateGroup 先 PATCH 设置再重新拉取群信息", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, groupInfo));
    const groups = useGroupsStore();

    await groups.updateGroup(5, { description: "新简介" });

    const calls = fetchMock.mock.calls.map(
      ([p, i]) => [p, (i as RequestInit).method] as const,
    );
    expect(calls).toEqual([
      ["/api/groups/5", "PATCH"],
      ["/api/groups/5", "GET"],
    ]);
    const body = JSON.parse(
      String((fetchMock.mock.calls[0]?.[1] as RequestInit).body),
    );
    expect(body).toEqual({ description: "新简介" });
  });

  it("setMemberTitle 先 POST 头衔再重新拉取群信息", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, groupInfo));
    const groups = useGroupsStore();

    await groups.setMemberTitle(5, "u2", "大佬");

    const calls = fetchMock.mock.calls.map(
      ([p, i]) => [p, (i as RequestInit).method] as const,
    );
    expect(calls).toEqual([
      ["/api/groups/5/members/u2/title", "POST"],
      ["/api/groups/5", "GET"],
    ]);
    const body = JSON.parse(
      String((fetchMock.mock.calls[0]?.[1] as RequestInit).body),
    );
    expect(body).toEqual({ title: "大佬" });
  });
});

describe("groups store — 邀请生命周期", () => {
  it("loadInvites 拉取待处理邀请", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, [
        {
          invite_id: "i1",
          conversation_id: 9,
          group_name: "游戏群",
          from: { user_id: "u1", username: "alice", display_name: "Alice" },
        },
      ]),
    );
    const groups = useGroupsStore();

    await groups.loadInvites();

    expect(groups.invites).toHaveLength(1);
    expect(groups.pendingInviteCount).toBe(1);
    expect(groups.invitesLoaded).toBe(true);
  });

  it("group.invited 帧即时追加邀请并去重", () => {
    const groups = useGroupsStore();
    const payload = {
      invite_id: "i2",
      conversation_id: 9,
      group_name: "游戏群",
      from: { user_id: "u1", username: "alice" },
    } as const;

    groups.onInvited(payload);
    groups.onInvited(payload);

    expect(groups.invites).toHaveLength(1);
    expect(groups.pendingInviteCount).toBe(1);
  });

  it("接受邀请后移除邀请、刷新会话并打开群聊", async () => {
    fetchMock.mockImplementation(async (path: string) => {
      if (path === "/api/groups/invites/i3/accept") {
        return jsonResponse(200, { conversation_id: 12 });
      }
      if (path === "/api/conversations") {
        return jsonResponse(200, [
          {
            conversation_id: 12,
            kind: "group",
            peer: null,
            name: "新群",
            last_seq: 0,
            last_delivered_seq: 0,
          },
        ]);
      }
      return jsonResponse(404, { error: "not_found" });
    });
    const groups = useGroupsStore();
    groups.invites = [
      {
        invite_id: "i3",
        conversation_id: 12,
        group_name: "新群",
        from: { user_id: "u1", username: "alice" },
      },
    ];

    const id = await groups.acceptInvite("i3");

    expect(id).toBe(12);
    expect(groups.invites).toHaveLength(0);
    const ws = useWsStore();
    expect(ws.activeConversationId).toBe(12);
    expect(ws.conversations.find((c) => c.conversationId === 12)?.name).toBe(
      "新群",
    );
  });

  it("拒绝邀请后移除条目", async () => {
    fetchMock.mockResolvedValue(emptyResponse(204));
    const groups = useGroupsStore();
    groups.invites = [
      {
        invite_id: "i4",
        conversation_id: 12,
        group_name: "新群",
        from: { user_id: "u1", username: "alice" },
      },
    ];

    await groups.declineInvite("i4");

    expect(groups.invites).toHaveLength(0);
    const [path] = fetchMock.mock.calls[0] as [string];
    expect(path).toBe("/api/groups/invites/i4/decline");
  });

  it("退出群聊后清理本地缓存并遗忘会话", async () => {
    fetchMock.mockResolvedValue(emptyResponse(204));
    const groups = useGroupsStore();
    groups.applyGroupInfo(groupInfo);
    const ws = useWsStore();
    ws.conversations.push({
      conversationId: 5,
      peerUserId: "",
      peerUsername: "",
      name: "团队",
      lastMessagePreview: null,
      lastActivityAt: "",
      unread: 0,
      lastSeenSeq: 0,
      maxSeq: 0,
      peerTypingUntil: null,
      kind: "group",
    });

    await groups.leave(5);

    expect(groups.infos[5]).toBeUndefined();
    expect(ws.conversations).toHaveLength(0);
  });
});

describe("groups store — M13b 群文件缓存与实时刷新", () => {
  const fileListing = {
    usage_bytes: 1536,
    quota_bytes: 1024 * 1024 * 1024,
    files: [
      {
        file_id: "f1",
        name: "报告.pdf",
        mime: "application/pdf",
        bytes: 1536,
        uploader: { user_id: "u2", username: "alice" },
        created_at: "2026-09-01T00:00:00Z",
        expires_at: null,
      },
    ],
  };

  function route(): void {
    fetchMock.mockImplementation(async (path: string) => {
      if (path === "/api/groups/5/files") return jsonResponse(200, fileListing);
      if (path === "/api/conversations") return jsonResponse(200, []);
      return jsonResponse(200, groupInfo);
    });
  }

  it("hostFilesView 拉取并缓存文件列表", async () => {
    route();
    const groups = useGroupsStore();

    groups.hostFilesView(5);

    await vi.waitFor(() => {
      expect(groups.filesFor(5)?.loaded).toBe(true);
    });
    expect(groups.filesFor(5)?.files).toHaveLength(1);
    expect(groups.filesFor(5)?.usageBytes).toBe(1536);
  });

  it("group.updated 在文件视图打开时重新拉取列表", async () => {
    route();
    const groups = useGroupsStore();
    groups.hostFilesView(5);
    await vi.waitFor(() => {
      expect(groups.filesFor(5)?.loaded).toBe(true);
    });
    fetchMock.mockClear();

    await groups.onUpdated({ conversation_id: 5 });

    await vi.waitFor(() => {
      expect(
        fetchMock.mock.calls.filter(([p]) => p === "/api/groups/5/files")
          .length,
      ).toBeGreaterThanOrEqual(1);
    });
  });

  it("文件视图关闭后 group.updated 不再拉取列表", async () => {
    route();
    const groups = useGroupsStore();
    groups.hostFilesView(5);
    await vi.waitFor(() => {
      expect(groups.filesFor(5)?.loaded).toBe(true);
    });
    groups.unhostFilesView();
    fetchMock.mockClear();

    await groups.onUpdated({ conversation_id: 5 });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(
      fetchMock.mock.calls.some(([p]) => p === "/api/groups/5/files"),
    ).toBe(false);
  });
});
