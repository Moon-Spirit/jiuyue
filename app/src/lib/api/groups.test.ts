import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "./client";
import {
  acceptGroupInvite,
  createGroup,
  declineGroupInvite,
  getGroup,
  groupApiErrorMessage,
  inviteToGroup,
  kickGroupMember,
  leaveGroup,
  listGroupInvites,
  setGroupMemberRole,
  transferGroupOwnership,
} from "./groups";
import { i18n } from "../../i18n";

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

function tr(key: string): string {
  return i18n.global.t(key);
}

beforeEach(() => {
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("groups API — 建群与成员管理", () => {
  it("createGroup 发送名称与受邀用户名，省略空数组", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(201, {
        conversation_id: 5,
        name: "团队",
        member_count: 2,
        invited: ["alice"],
      }),
    );

    const result = await createGroup("tok", "团队", ["alice"]);

    expect(result.conversation_id).toBe(5);
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/groups");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual({
      name: "团队",
      invite_usernames: ["alice"],
    });
    expect((init.headers as Headers).get("Authorization")).toBe("Bearer tok");
  });

  it("createGroup 在无好友时只发送群名", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(201, {
        conversation_id: 6,
        name: "Solo",
        member_count: 1,
        invited: [],
      }),
    );

    await createGroup("tok", "Solo");

    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(JSON.parse(String(init.body))).toEqual({ name: "Solo" });
  });

  it("getGroup 请求成员名册信息", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, {
        conversation_id: 5,
        name: "团队",
        my_role: "owner",
        member_count: 1,
        members: [],
      }),
    );

    const info = await getGroup("tok", 5);

    expect(info.my_role).toBe("owner");
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/groups/5");
    expect(init.method).toBe("GET");
  });

  it("邀请和成员操作命中正确的路径与方法", async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }));
    fetchMock.mockResolvedValueOnce(jsonResponse(201, { invite_id: "inv1" }));

    await inviteToGroup("tok", 5, "carol");
    await kickGroupMember("tok", 5, "u2");
    await setGroupMemberRole("tok", 5, "u2", "admin");
    await transferGroupOwnership("tok", 5, "u3");
    await leaveGroup("tok", 5);

    const calls = fetchMock.mock.calls.map(
      ([p, i]) => [p, (i as RequestInit).method] as const,
    );
    expect(calls).toEqual([
      ["/api/groups/5/invites", "POST"],
      ["/api/groups/5/members/u2/kick", "POST"],
      ["/api/groups/5/members/u2/role", "POST"],
      ["/api/groups/5/transfer", "POST"],
      ["/api/groups/5/leave", "POST"],
    ]);
    const roleBody = JSON.parse(
      String((fetchMock.mock.calls[2]?.[1] as RequestInit).body),
    );
    expect(roleBody).toEqual({ role: "admin" });
    const transferBody = JSON.parse(
      String((fetchMock.mock.calls[3]?.[1] as RequestInit).body),
    );
    expect(transferBody).toEqual({ user_id: "u3" });
  });
});

describe("groups API — 群聊邀请", () => {
  it("listGroupInvites 拉取待处理邀请", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, [
        {
          invite_id: "i1",
          conversation_id: 9,
          group_name: "游戏群",
          from: { user_id: "u1", username: "alice" },
        },
      ]),
    );

    const invites = await listGroupInvites("tok");

    expect(invites).toHaveLength(1);
    expect(invites[0]?.group_name).toBe("游戏群");
  });

  it("acceptGroupInvite 返回加入的会话 id，decline 返回 204", async () => {
    fetchMock.mockResolvedValueOnce(jsonResponse(200, { conversation_id: 9 }));
    const accepted = await acceptGroupInvite("tok", "i1");
    expect(accepted.conversation_id).toBe(9);

    fetchMock.mockResolvedValueOnce(emptyResponse(204));
    await declineGroupInvite("tok", "i2");
    const [path, init] = fetchMock.mock.calls[1] as [string, RequestInit];
    expect(path).toBe("/api/groups/invites/i2/decline");
    expect(init.method).toBe("POST");
  });
});

describe("群聊错误码映射", () => {
  it("已知群聊机器码映射为本地化文案，未知回退通用文案", () => {
    expect(
      groupApiErrorMessage(new ApiError(403, "owner_cannot_leave", "x"), tr),
    ).toBe(tr("errors.owner_cannot_leave"));
    expect(
      groupApiErrorMessage(new ApiError(403, "not_group_member", "x"), tr),
    ).toBe(tr("errors.not_group_member"));
    expect(groupApiErrorMessage(new ApiError(403, "forbidden", "x"), tr)).toBe(
      tr("errors.forbidden"),
    );
    expect(groupApiErrorMessage(new ApiError(500, "whatever", "x"), tr)).toBe(
      tr("errors.unknown"),
    );
    expect(groupApiErrorMessage(new Error("boom"), tr)).toBe(
      tr("errors.unknown"),
    );
  });
});
