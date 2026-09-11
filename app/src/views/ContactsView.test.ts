import { beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import type { DOMWrapper, VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import type { Pinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import type { Router } from "vue-router";
import { i18n } from "../i18n";
import { useAuthStore } from "../stores/auth";
import { useFriendsStore } from "../stores/friends";
import { useGroupsStore } from "../stores/groups";
import ContactsView from "./ContactsView.vue";

const fetchMock = vi.fn();

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

/** Fresh pinia + authed user; ws stays idle because status remains anon. */
function freshSession(): Pinia {
  const pinia = createPinia();
  setActivePinia(pinia);
  const auth = useAuthStore();
  auth.user = { userId: "7", username: "me", uid: 1000007 };
  // Cached token keeps friends actions off the refresh path (no network).
  auth.accessToken = "test-token";
  return pinia;
}

async function mountView(pinia: Pinia): Promise<VueWrapper> {
  const router: Router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/login", component: { template: "<div />" } },
      { path: "/chat", component: { template: "<div />" } },
      { path: "/contacts", component: ContactsView },
    ],
  });
  await router.push("/contacts");
  await router.isReady();

  return mount(ContactsView, {
    global: { plugins: [pinia, i18n, router] },
  });
}

/**
 * AppShell instantiates every slot once per breakpoint; queries must be
 * scoped to the desktop panes to see each element exactly once.
 */
function sessionsPane(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find('[data-testid="shell-sessions-desktop"]');
}

function mainPane(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find('[data-testid="shell-main-desktop"]');
}

/** Types a query into the search box and runs the search immediately. */
async function typeAndSearch(pane: DOMWrapper<Element>, query: string) {
  await pane.find('[data-testid="user-search-input"]').setValue(query);
  await pane.find('[data-testid="user-search-input"]').trigger("keydown.enter");
}

beforeEach(() => {
  localStorage.clear();
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});

describe("ContactsView — 通讯录面板", () => {
  it("渲染标题、搜索框与空态文案", async () => {
    const pinia = freshSession();
    // Mirrors a completed load so the "no friends yet" empty state shows.
    useFriendsStore().loaded = true;
    const wrapper = await mountView(pinia);

    expect(
      sessionsPane(wrapper).find('[data-testid="contacts-title"]').text(),
    ).toBe("通讯录");
    expect(
      sessionsPane(wrapper).find('[data-testid="user-search-input"]').exists(),
    ).toBe(true);
    expect(
      sessionsPane(wrapper).find('[data-testid="friends-empty"]').exists(),
    ).toBe(true);
    expect(
      mainPane(wrapper).find('[data-testid="contacts-empty-state"]').exists(),
    ).toBe(true);
  });

  it("好友与待处理申请渲染为列表，含接受/拒绝/发消息操作", async () => {
    const pinia = freshSession();
    const friends = useFriendsStore();
    friends.friends.push({
      user_id: "u1",
      username: "alice",
      uid: 100001,
      since: "2026-01-01T00:00:00Z",
    });
    friends.incoming.push({
      request_id: "r1",
      from: { user_id: "u2", username: "bob", uid: 100002 },
      created_at: new Date().toISOString(),
    });

    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);

    expect(pane.find('[data-testid="friend-item"]').text()).toContain("alice");
    expect(pane.find('[data-testid="friend-uid"]').text()).toContain("100001");
    expect(pane.find('[data-testid="message-friend"]').exists()).toBe(true);
    expect(pane.find('[data-testid="incoming-item"]').text()).toContain("bob");
    expect(pane.find('[data-testid="accept-request"]').exists()).toBe(true);
    expect(pane.find('[data-testid="decline-request"]').exists()).toBe(true);
  });

  it("搜索用户后渲染结果卡片（用户名 + UID），添加后按钮变为已发送", async () => {
    fetchMock.mockImplementation(async (path: string, init?: RequestInit) => {
      if (path.startsWith("/api/users/search")) {
        return jsonResponse(200, [
          { user_id: "u8", username: "carol", uid: 100023 },
        ]);
      }
      if (path === "/api/friends/requests" && init?.method === "POST") {
        return jsonResponse(201, {
          request_id: "r9",
          to: { user_id: "u8", username: "carol", uid: 100023 },
        });
      }
      return jsonResponse(404, { error: "not_found", message: "x" });
    });
    const pinia = freshSession();
    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);

    await typeAndSearch(pane, "carol");

    await vi.waitFor(() => {
      expect(pane.find('[data-testid="search-result-item"]').exists()).toBe(
        true,
      );
    });
    const item = pane.find('[data-testid="search-result-item"]');
    expect(item.find('[data-testid="search-result-name"]').text()).toBe(
      "carol",
    );
    expect(item.find('[data-testid="search-result-uid"]').text()).toContain(
      "100023",
    );

    // Sending the request flips the card to the disabled 已发送 state.
    await pane.find('[data-testid="search-result-add"]').trigger("click");
    await vi.waitFor(() => {
      expect(pane.find('[data-testid="search-result-sent"]').exists()).toBe(
        true,
      );
    });
    expect(useFriendsStore().outgoing.map((r) => r.to.username)).toEqual([
      "carol",
    ]);
  });

  it("已添加的好友不出现在搜索结果中（按钮禁用为已添加）", async () => {
    fetchMock.mockImplementation(async (path: string) => {
      if (path.startsWith("/api/users/search")) {
        return jsonResponse(200, [
          { user_id: "u1", username: "alice", uid: 100001 },
        ]);
      }
      return jsonResponse(404, { error: "not_found", message: "x" });
    });
    const pinia = freshSession();
    const friends = useFriendsStore();
    friends.friends.push({
      user_id: "u1",
      username: "alice",
      uid: 100001,
      since: "2026-01-01T00:00:00Z",
    });

    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);

    await typeAndSearch(pane, "alice");
    await vi.waitFor(() => {
      expect(
        pane.find('[data-testid="search-result-already-friend"]').exists(),
      ).toBe(true);
    });
    expect(pane.find('[data-testid="search-result-add"]').exists()).toBe(false);
  });

  it("搜索无结果时展示空态文案", async () => {
    fetchMock.mockResolvedValue(jsonResponse(200, []));
    const wrapper = await mountView(freshSession());
    const pane = sessionsPane(wrapper);

    await typeAndSearch(pane, "nobody_here");
    await vi.waitFor(() => {
      expect(pane.find('[data-testid="search-empty"]').exists()).toBe(true);
    });
  });

  it("发送好友申请返回 peer_not_found 时展示本地化错误", async () => {
    fetchMock.mockImplementation(async (path: string) => {
      if (path.startsWith("/api/users/search")) {
        return jsonResponse(200, [
          { user_id: "u9", username: "ghost", uid: 100099 },
        ]);
      }
      return jsonResponse(404, { error: "peer_not_found", message: "nope" });
    });
    const wrapper = await mountView(freshSession());
    const pane = sessionsPane(wrapper);

    await typeAndSearch(pane, "ghost");
    await vi.waitFor(() => {
      expect(pane.find('[data-testid="search-result-item"]').exists()).toBe(
        true,
      );
    });
    await pane.find('[data-testid="search-result-add"]').trigger("click");
    await vi.waitFor(() => {
      expect(pane.find('[data-testid="search-error"]').text()).toBe(
        i18n.global.t("errors.peer_not_found"),
      );
    });
  });

  it("删除好友需两步确认", async () => {
    const pinia = freshSession();
    const friends = useFriendsStore();
    friends.friends.push({
      user_id: "u1",
      username: "alice",
      uid: 100001,
      since: "2026-01-01T00:00:00Z",
    });

    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);

    // 第一步：按钮从「删除好友」变为「确认删除」，尚未发请求。
    await pane.find('[data-testid="unfriend-friend"]').trigger("click");
    expect(pane.find('[data-testid="confirm-unfriend"]').exists()).toBe(true);
    expect(fetchMock).not.toHaveBeenCalled();

    // 第二步：确认后才真正调用 DELETE 并移除条目。
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }));
    await pane.find('[data-testid="confirm-unfriend"]').trigger("click");
    await vi.waitFor(() => {
      expect(pane.find('[data-testid="friend-item"]').exists()).toBe(false);
    });
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/friends/u1");
    expect(init.method).toBe("DELETE");
  });
});

describe("ContactsView — 群聊邀请", () => {
  it("没有邀请时不渲染群聊邀请区块", async () => {
    const wrapper = await mountView(freshSession());
    expect(
      sessionsPane(wrapper).find('[data-testid="group-invites"]').exists(),
    ).toBe(false);
  });

  it("渲染群聊邀请列表并提供接受/拒绝操作", async () => {
    const pinia = freshSession();
    useGroupsStore().invites.push({
      invite_id: "i1",
      conversation_id: 9,
      group_name: "游戏群",
      from: { user_id: "u1", username: "alice", display_name: "Alice" },
    });

    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);

    expect(pane.find('[data-testid="group-invites"]').exists()).toBe(true);
    expect(pane.find('[data-testid="group-invite-name"]').text()).toBe(
      "游戏群",
    );
    expect(pane.find('[data-testid="group-invite-from"]').text()).toContain(
      "Alice",
    );
    expect(pane.find('[data-testid="group-invite-accept"]').exists()).toBe(
      true,
    );
    expect(pane.find('[data-testid="group-invite-decline"]').exists()).toBe(
      true,
    );
  });

  it("接受邀请后移除条目并跳转到聊天", async () => {
    fetchMock.mockImplementation(async (path: string) => {
      if (path === "/api/groups/invites/i1/accept") {
        return jsonResponse(200, { conversation_id: 9 });
      }
      if (path === "/api/conversations") return jsonResponse(200, []);
      return jsonResponse(404, { error: "not_found" });
    });
    const pinia = freshSession();
    useGroupsStore().invites.push({
      invite_id: "i1",
      conversation_id: 9,
      group_name: "游戏群",
      from: { user_id: "u1", username: "alice" },
    });

    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);
    await pane.find('[data-testid="group-invite-accept"]').trigger("click");

    await vi.waitFor(() => {
      expect(
        sessionsPane(wrapper).find('[data-testid="group-invites"]').exists(),
      ).toBe(false);
    });
    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/groups/invites/i1/accept");
    expect(init.method).toBe("POST");
  });

  it("拒绝邀请调用 decline 并移除条目", async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 204 }));
    const pinia = freshSession();
    useGroupsStore().invites.push({
      invite_id: "i2",
      conversation_id: 9,
      group_name: "游戏群",
      from: { user_id: "u1", username: "alice" },
    });

    const wrapper = await mountView(pinia);
    await sessionsPane(wrapper)
      .find('[data-testid="group-invite-decline"]')
      .trigger("click");

    await vi.waitFor(() => {
      expect(
        sessionsPane(wrapper).find('[data-testid="group-invites"]').exists(),
      ).toBe(false);
    });
    const [path] = fetchMock.mock.calls[0] as [string];
    expect(path).toBe("/api/groups/invites/i2/decline");
  });

  it("导航角标同时统计好友申请与群聊邀请", async () => {
    const pinia = freshSession();
    const friends = useFriendsStore();
    friends.incoming.push({
      request_id: "r1",
      from: { user_id: "u2", username: "bob" },
      created_at: new Date().toISOString(),
    });
    useGroupsStore().invites.push({
      invite_id: "i1",
      conversation_id: 9,
      group_name: "游戏群",
      from: { user_id: "u1", username: "alice" },
    });

    const wrapper = await mountView(pinia);
    expect(wrapper.find('[data-testid="nav-contacts-badge"]').text()).toBe("2");
  });
});
