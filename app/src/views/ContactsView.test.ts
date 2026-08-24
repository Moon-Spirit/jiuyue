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
  auth.user = { userId: "7", username: "me" };
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

beforeEach(() => {
  localStorage.clear();
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});

describe("ContactsView — 通讯录面板", () => {
  it("渲染标题、添加好友表单与空态文案", async () => {
    const wrapper = await mountView(freshSession());

    expect(
      sessionsPane(wrapper).find('[data-testid="contacts-title"]').text(),
    ).toBe("通讯录");
    expect(
      sessionsPane(wrapper).find('[data-testid="add-friend-input"]').exists(),
    ).toBe(true);
    expect(
      sessionsPane(wrapper).find('[data-testid="add-friend-submit"]').exists(),
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
      since: "2026-01-01T00:00:00Z",
    });
    friends.incoming.push({
      request_id: "r1",
      from: { user_id: "u2", username: "bob" },
      created_at: new Date().toISOString(),
    });

    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);

    expect(pane.find('[data-testid="friend-item"]').text()).toContain("alice");
    expect(pane.find('[data-testid="message-friend"]').exists()).toBe(true);
    expect(pane.find('[data-testid="incoming-item"]').text()).toContain("bob");
    expect(pane.find('[data-testid="accept-request"]').exists()).toBe(true);
    expect(pane.find('[data-testid="decline-request"]').exists()).toBe(true);
  });

  it("提交合法用户名后发送申请并清空输入框", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(201, {
        request_id: "r9",
        to: { user_id: "u8", username: "carol" },
      }),
    );
    const pinia = freshSession();
    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);

    await pane.find('[data-testid="add-friend-input"]').setValue("carol");
    await pane.find("form").trigger("submit");
    await vi.waitFor(() => {
      expect(
        (
          pane.find('[data-testid="add-friend-input"]')
            .element as HTMLInputElement
        ).value,
      ).toBe("");
    });

    expect(useFriendsStore().outgoing.map((r) => r.to.username)).toEqual([
      "carol",
    ]);
  });

  it("后端返回 peer_not_found 时展示本地化错误", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(404, { error: "peer_not_found", message: "nope" }),
    );
    const wrapper = await mountView(freshSession());
    const pane = sessionsPane(wrapper);

    await pane.find('[data-testid="add-friend-input"]').setValue("ghost");
    await pane.find("form").trigger("submit");
    await vi.waitFor(() => {
      expect(pane.find('[data-testid="add-friend-error"]').text()).toBe(
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
