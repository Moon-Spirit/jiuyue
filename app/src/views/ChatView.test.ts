import { beforeEach, describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import type { DOMWrapper, VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import type { Pinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import type { Router } from "vue-router";
import { i18n } from "../i18n";
import { useAuthStore } from "../stores/auth";
import { useWsStore } from "../stores/ws";
import type { ChatMessage, Conversation } from "../stores/ws";
import ChatView from "./ChatView.vue";

function isoAt(offsetMs: number): string {
  return new Date(Date.now() + offsetMs).toISOString();
}

const HOUR = 3_600_000;
const MINUTE = 60_000;

function conversation(
  conversationId: number,
  overrides: Partial<Conversation> = {},
): Conversation {
  return {
    conversationId,
    peerUserId: 100 + conversationId,
    peerUsername: `peer${conversationId}`,
    lastMessagePreview: null,
    lastActivityAt: "",
    unread: 0,
    lastSeenSeq: 0,
    maxSeq: 0,
    ...overrides,
  };
}

function message(overrides: Partial<ChatMessage>): ChatMessage {
  return {
    clientMsgId: "",
    messageId: "m-x",
    conversationId: 1,
    seq: 1,
    senderId: "peer-1",
    body: "hello",
    sentAt: isoAt(0),
    mine: false,
    status: "delivered",
    ...overrides,
  };
}

async function mountView(pinia: Pinia): Promise<VueWrapper> {
  const router: Router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/login", component: { template: "<div />" } },
      { path: "/chat", component: ChatView },
    ],
  });
  await router.push("/chat");
  await router.isReady();

  return mount(ChatView, {
    global: { plugins: [pinia, i18n, router] },
  });
}

/**
 * AppShell instantiates every slot once per breakpoint (desktop / tablet /
 * mobile), so queries must be scoped to the desktop panes to see each
 * element exactly once.
 */
function sessionsPane(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find('[data-testid="shell-sessions-desktop"]');
}

function mainPane(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find('[data-testid="shell-main-desktop"]');
}

beforeEach(() => {
  localStorage.clear();
});

describe("ChatView — sessions pane", () => {
  it("lists conversations by recent activity with preview, relative time and unread badge", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: 7, username: "me" };
    const ws = useWsStore();
    ws.conversations.push(
      conversation(1, {
        peerUsername: "alice",
        lastMessagePreview: "早",
        lastActivityAt: isoAt(-2 * HOUR),
      }),
      conversation(2, {
        peerUsername: "bob",
        lastMessagePreview: "x".repeat(60),
        lastActivityAt: isoAt(-3 * MINUTE),
        unread: 2,
        lastSeenSeq: 2,
        maxSeq: 4,
      }),
    );

    const wrapper = await mountView(pinia);
    const items = sessionsPane(wrapper).findAll('[data-testid="session-item"]');
    expect(items).toHaveLength(2);

    // Most recent activity first.
    expect(items[0]?.attributes("data-conversation-id")).toBe("2");
    expect(items[1]?.attributes("data-conversation-id")).toBe("1");

    // Preview truncates at 40 chars with an ellipsis.
    const preview = items[0]?.find('[data-testid="session-preview"]');
    expect(preview?.text()).toHaveLength(41);
    expect(preview?.text().endsWith("…")).toBe(true);

    // Relative time renders through Intl.RelativeTimeFormat (zh-CN default).
    const aliceTime = items[1]?.find('[data-testid="session-time"]');
    expect(aliceTime?.text()).toContain("小时前");

    // Unread badge only on the conversation that has one.
    expect(items[0]?.find('[data-testid="unread-badge"]').text()).toBe("2");
    expect(items[1]?.find('[data-testid="unread-badge"]').exists()).toBe(false);
  });

  it("shows the empty state when no conversation exists", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);

    const wrapper = await mountView(pinia);
    expect(
      sessionsPane(wrapper).find('[data-testid="sessions-empty"]').exists(),
    ).toBe(true);
    expect(
      sessionsPane(wrapper).find('[data-testid="session-list"]').exists(),
    ).toBe(false);
  });
});

describe("ChatView — message thread", () => {
  async function mountWithThread(): Promise<{
    wrapper: VueWrapper;
    ws: ReturnType<typeof useWsStore>;
  }> {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: 7, username: "me" };
    const ws = useWsStore();
    ws.conversations.push(conversation(1, { peerUsername: "alice" }));
    ws.messagesByConversation[1] = [
      message({
        messageId: "m-y",
        seq: 1,
        senderId: "alice",
        body: "昨天见",
        sentAt: isoAt(-24 * HOUR),
      }),
      message({
        clientMsgId: "c-1",
        messageId: null,
        seq: null,
        senderId: "7",
        body: "待发送",
        sentAt: isoAt(-30_000),
        mine: true,
        status: "sending",
      }),
      message({
        clientMsgId: "c-2",
        messageId: "m-2",
        seq: 2,
        senderId: "7",
        body: "已送达的消息",
        sentAt: isoAt(-20_000),
        mine: true,
        status: "delivered",
      }),
      message({
        clientMsgId: "c-3",
        messageId: null,
        seq: null,
        senderId: "7",
        body: "失败的消息",
        sentAt: isoAt(-10_000),
        mine: true,
        status: "failed",
      }),
    ];
    ws.openConversation(1);

    const wrapper = await mountView(pinia);
    return { wrapper, ws };
  }

  it("renders date separators and bubbles with per-message delivery status", async () => {
    const { wrapper } = await mountWithThread();
    const main = mainPane(wrapper);

    const separators = main.findAll('[data-testid="date-separator"]');
    const labels = separators.map((s) => s.text());
    expect(labels).toContain("昨天");
    expect(labels).toContain("今天");

    const items = main.findAll('[data-testid="message-item"]');
    expect(items).toHaveLength(4);

    // Theirs: left slate bubble with sender name; mine: right indigo bubble.
    const theirsBubble = items[0]?.find('[data-testid="message-bubble"]');
    expect(theirsBubble?.classes()).toContain("bg-slate-100");
    expect(items[0]?.find('[data-testid="message-sender"]').text()).toBe(
      "alice",
    );
    const mineBubble = items[1]?.find('[data-testid="message-bubble"]');
    expect(mineBubble?.classes()).toContain("bg-indigo-600");
    expect(items[1]?.find('[data-testid="message-sender"]').exists()).toBe(
      false,
    );

    const statuses = main.findAll('[data-testid="message-status"]');
    expect(statuses.map((s) => s.text())).toEqual([
      expect.stringContaining("发送中"),
      expect.stringContaining("已送达"),
    ]);
    expect(statuses[0]?.classes()).toContain("animate-pulse");
  });

  it("offers a retry affordance for failed sends that flips them back to sending", async () => {
    const { wrapper, ws } = await mountWithThread();

    const retry = mainPane(wrapper).find('[data-testid="retry-button"]');
    expect(retry.exists()).toBe(true);
    expect(retry.text()).toContain("失败");

    await retry.trigger("click");

    const failed = ws.messagesByConversation[1]?.find(
      (m) => m.clientMsgId === "c-3",
    );
    expect(failed?.status).toBe("sending");
    expect(
      mainPane(wrapper).find('[data-testid="retry-button"]').exists(),
    ).toBe(false);
  });
});

describe("ChatView — unread and composer", () => {
  async function mountWithUnread(): Promise<{
    wrapper: VueWrapper;
    ws: ReturnType<typeof useWsStore>;
  }> {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: 7, username: "me" };
    const ws = useWsStore();
    ws.conversations.push(
      conversation(2, {
        peerUsername: "bob",
        lastActivityAt: isoAt(-MINUTE),
        unread: 2,
        lastSeenSeq: 2,
        maxSeq: 4,
      }),
    );
    ws.messagesByConversation[2] = [
      message({
        messageId: "m-3",
        conversationId: 2,
        seq: 3,
        senderId: "bob",
        body: "第一条",
        sentAt: isoAt(-2 * MINUTE),
      }),
      message({
        messageId: "m-4",
        conversationId: 2,
        seq: 4,
        senderId: "bob",
        body: "第二条",
        sentAt: isoAt(-MINUTE),
      }),
    ];

    const wrapper = await mountView(pinia);
    return { wrapper, ws };
  }

  it("clears the unread badge when the conversation is opened", async () => {
    const { wrapper, ws } = await mountWithUnread();

    const item = sessionsPane(wrapper).find('[data-testid="session-item"]');
    expect(item.find('[data-testid="unread-badge"]').text()).toBe("2");
    // No thread before opening.
    expect(
      mainPane(wrapper).find('[data-testid="message-list"]').exists(),
    ).toBe(false);

    await item.trigger("click");

    expect(ws.activeConversationId).toBe(2);
    expect(ws.conversations[0]?.unread).toBe(0);
    expect(ws.conversations[0]?.lastSeenSeq).toBe(4);
    await wrapper.vm.$nextTick();
    const reopened = sessionsPane(wrapper).find('[data-testid="session-item"]');
    expect(reopened.find('[data-testid="unread-badge"]').exists()).toBe(false);
    expect(
      mainPane(wrapper).find('[data-testid="message-list"]').exists(),
    ).toBe(true);
  });

  it("disables the composer with a reconnect banner while disconnected", async () => {
    const { wrapper, ws } = await mountWithUnread();
    await sessionsPane(wrapper)
      .find('[data-testid="session-item"]')
      .trigger("click");

    // Store starts idle → disconnected.
    expect(
      mainPane(wrapper).find('[data-testid="ws-banner"]').text(),
    ).toContain("重连中");
    expect(
      mainPane(wrapper)
        .find('[data-testid="composer-input"]')
        .attributes("disabled"),
    ).toBeDefined();

    ws.status = "open";
    await wrapper.vm.$nextTick();
    expect(mainPane(wrapper).find('[data-testid="ws-banner"]').exists()).toBe(
      false,
    );
    expect(
      mainPane(wrapper)
        .find('[data-testid="composer-input"]')
        .attributes("disabled"),
    ).toBeUndefined();
  });

  it("sends optimistically on Enter and clears the draft", async () => {
    const { wrapper, ws } = await mountWithUnread();
    await sessionsPane(wrapper)
      .find('[data-testid="session-item"]')
      .trigger("click");
    ws.status = "open";
    await wrapper.vm.$nextTick();

    const composer = mainPane(wrapper).find('[data-testid="composer-input"]');
    await composer.setValue("你好");
    await composer.trigger("keydown.enter");

    const messages = ws.messagesByConversation[2] ?? [];
    const optimistic = messages.at(-1);
    expect(optimistic?.body).toBe("你好");
    expect(optimistic?.status).toBe("sending");
    expect(optimistic?.mine).toBe(true);
    expect((composer.element as HTMLTextAreaElement).value).toBe("");

    const items = mainPane(wrapper).findAll('[data-testid="message-item"]');
    expect(items).toHaveLength(3);
    expect(items.at(-1)?.text()).toContain("发送中");
  });
});
