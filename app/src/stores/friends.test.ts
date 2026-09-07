import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { ApiError } from "../lib/api/client";
import { i18n } from "../i18n";
import { friendApiErrorMessage } from "../lib/api/friends";
import { useAuthStore } from "./auth";
import { useFriendsStore } from "./friends";

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

/** Localized string for a key under the default (zh-CN) test locale. */
function tr(key: string): string {
  return i18n.global.t(key);
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

describe("friends store — 好友申请生命周期", () => {
  it("发送申请后进入待处理列表，请求体携带用户名", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(201, {
        request_id: "r1",
        to: { user_id: "u1", username: "alice" },
      }),
    );
    const store = useFriendsStore();

    await store.sendRequest("alice");

    expect(store.outgoing).toHaveLength(1);
    expect(store.outgoing[0]?.request_id).toBe("r1");
    expect(store.outgoing[0]?.to.username).toBe("alice");
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/friends/requests");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual({ username: "alice" });
  });

  it("接受申请后：好友出现、申请移除、角标归零", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, { friend: { user_id: "u2", username: "bob" } }),
    );
    const store = useFriendsStore();
    store.incoming.push({
      request_id: "r9",
      from: { user_id: "u2", username: "bob" },
      created_at: new Date().toISOString(),
    });

    await store.accept("r9");

    expect(store.incoming).toHaveLength(0);
    expect(store.friends).toHaveLength(1);
    expect(store.friends[0]?.username).toBe("bob");
    expect(store.pendingCount).toBe(0);
    const [path] = fetchMock.mock.calls[0] as [string];
    expect(path).toBe("/api/friends/requests/r9/accept");
  });

  it("拒绝与撤销分别清空对应列表并返回 204", async () => {
    fetchMock.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path.endsWith("/decline")) return emptyResponse(204);
      if (init?.method === "DELETE") return emptyResponse(204);
      return jsonResponse(400, { error: "bad_request" });
    });
    const store = useFriendsStore();
    store.incoming.push({
      request_id: "in-1",
      from: { user_id: "u3", username: "carol" },
      created_at: new Date().toISOString(),
    });
    store.outgoing.push({
      request_id: "out-1",
      to: { user_id: "u4", username: "dave" },
      created_at: new Date().toISOString(),
    });

    await store.decline("in-1");
    await store.cancel("out-1");

    expect(store.incoming).toHaveLength(0);
    expect(store.outgoing).toHaveLength(0);
  });

  it("loadAll 拉取好友与申请两个列表并置 loaded", async () => {
    fetchMock.mockImplementation(async (path: string) => {
      if (path === "/api/friends") {
        return jsonResponse(200, [
          { user_id: "u2", username: "bob", since: "2026-01-01T00:00:00Z" },
        ]);
      }
      if (path === "/api/friends/requests") {
        return jsonResponse(200, {
          incoming: [
            {
              request_id: "r5",
              from: { user_id: "u6", username: "erin" },
              created_at: "2026-01-02T00:00:00Z",
            },
          ],
          outgoing: [],
        });
      }
      return jsonResponse(404, { error: "not_found" });
    });
    const store = useFriendsStore();

    await store.loadAll();

    expect(store.loaded).toBe(true);
    expect(store.friends.map((f) => f.username)).toEqual(["bob"]);
    expect(store.incoming.map((r) => r.from.username)).toEqual(["erin"]);
    expect(store.pendingCount).toBe(1);
  });

  it("删除好友后从列表移除", async () => {
    fetchMock.mockResolvedValue(emptyResponse(204));
    const store = useFriendsStore();
    store.friends.push({
      user_id: "u2",
      username: "bob",
      since: "2026-01-01T00:00:00Z",
    });

    await store.unfriend("u2");

    expect(store.friends).toHaveLength(0);
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/friends/u2");
    expect(init.method).toBe("DELETE");
  });
});

describe("friends store — UID / username 用户搜索", () => {
  it("search 以 Bearer 调用 /api/users/search 并编码查询参数", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, [{ user_id: "u1", username: "alice", uid: 100001 }]),
    );
    const store = useFriendsStore();

    const results = await store.search("alice");

    expect(results).toEqual([
      { user_id: "u1", username: "alice", uid: 100001 },
    ]);
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/users/search?q=alice");
    expect(init.method).toBe("GET");
    expect((init.headers as Headers).get("Authorization")).toBe(
      "Bearer test-token",
    );
  });

  it("searchAndAdd 命中时向第一个结果的用户名发送申请", async () => {
    fetchMock.mockImplementation(async (path: string) => {
      if (path.startsWith("/api/users/search")) {
        return jsonResponse(200, [
          { user_id: "u1", username: "alice", uid: 100001 },
          { user_id: "u2", username: "alice_2", uid: 100002 },
        ]);
      }
      return jsonResponse(201, {
        request_id: "r1",
        to: { user_id: "u1", username: "alice", uid: 100001 },
      });
    });
    const store = useFriendsStore();

    const results = await store.searchAndAdd("alice");

    expect(results).toHaveLength(2);
    expect(store.outgoing).toHaveLength(1);
    expect(store.outgoing[0]?.to.username).toBe("alice");
    const [searchPath, searchInit] = fetchMock.mock.calls[0] as [
      string,
      RequestInit,
    ];
    expect(searchPath).toBe("/api/users/search?q=alice");
    expect(searchInit.method).toBe("GET");
    const [sendPath, sendInit] = fetchMock.mock.calls[1] as [
      string,
      RequestInit,
    ];
    expect(sendPath).toBe("/api/friends/requests");
    expect(JSON.parse(String(sendInit.body))).toEqual({ username: "alice" });
  });

  it("searchAndAdd 空结果时不发送申请，原样返回空数组", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []));
    const store = useFriendsStore();

    const results = await store.searchAndAdd("no_such_user");

    expect(results).toEqual([]);
    expect(store.outgoing).toHaveLength(0);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
});

describe("friends store — WS 帧驱动的本地变更", () => {
  it("friend.requested 推入申请并推高角标，重复帧去重", () => {
    const store = useFriendsStore();
    const payload = {
      request_id: "r7",
      from: { user_id: "u8", username: "frank" },
    } as const;

    store.onFriendRequested(payload);
    store.onFriendRequested(payload);

    expect(store.incoming).toHaveLength(1);
    expect(store.pendingCount).toBe(1);
    expect(store.incoming[0]?.from.username).toBe("frank");
  });

  it("friend.accepted 添加好友并清掉对应的外发申请", () => {
    const store = useFriendsStore();
    store.outgoing.push({
      request_id: "out-2",
      to: { user_id: "u1", username: "alice" },
      created_at: new Date().toISOString(),
    });

    store.onFriendAccepted({ friend: { user_id: "u1", username: "alice" } });

    expect(store.friends.map((f) => f.user_id)).toEqual(["u1"]);
    expect(store.outgoing).toHaveLength(0);
  });
});

describe("好友错误码映射", () => {
  it("sendRequest 透传后端机器码（如 peer_not_found）", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(404, { error: "peer_not_found", message: "nope" }),
    );
    const store = useFriendsStore();

    await expect(store.sendRequest("ghost")).rejects.toMatchObject({
      name: "ApiError",
      machine: "peer_not_found",
    });
  });

  it("好友专属机器码映射为本地化文案，未知码回退通用文案", () => {
    const cases: Array<[string, string]> = [
      ["self_request", "errors.self_request"],
      ["already_friends", "errors.already_friends"],
      ["request_already_pending", "errors.request_already_pending"],
      ["peer_not_found", "errors.peer_not_found"],
    ];
    for (const [machine, key] of cases) {
      const message = friendApiErrorMessage(
        new ApiError(400, machine, "x"),
        tr,
      );
      expect(message).toBe(tr(key));
      expect(message.length).toBeGreaterThan(0);
    }
    expect(friendApiErrorMessage(new ApiError(500, "whatever", "x"), tr)).toBe(
      tr("errors.unknown"),
    );
    expect(friendApiErrorMessage(new Error("boom"), tr)).toBe(
      tr("errors.unknown"),
    );
  });
});
