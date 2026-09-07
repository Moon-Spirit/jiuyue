import { beforeEach, describe, expect, it, vi } from "vitest";
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
    peerUserId: String(100 + conversationId),
    peerUsername: `peer${conversationId}`,
    lastMessagePreview: null,
    lastActivityAt: "",
    unread: 0,
    lastSeenSeq: 0,
    maxSeq: 0,
    peerTypingUntil: null,
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
    recalled: false,
    replyToMessageId: null,
    replyToSenderId: null,
    replyToBodyPreview: null,
    forwardedFromUsername: null,
    ...overrides,
  };
}

async function mountView(pinia: Pinia): Promise<VueWrapper> {
  const router: Router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/login", component: { template: "<div />" } },
      { path: "/chat", component: ChatView },
      { path: "/profile", component: { template: "<div />" } },
      { path: "/profile/:userId", component: { template: "<div />" } },
      { path: "/contacts", component: { template: "<div />" } },
    ],
  });
  await router.push("/chat");
  await router.isReady();

  return mount(ChatView, {
    global: { plugins: [pinia, i18n, router] },
  });
}

/** Nav appears once per breakpoint; scope to the desktop rail. */
function navPane(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find('[data-testid="shell-nav-desktop"]');
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
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
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
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
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
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
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

  describe("ChatView — profile entry points", () => {
    it("renders the self avatar in the nav and routes to /profile on click", async () => {
      const pinia = createPinia();
      setActivePinia(pinia);
      useAuthStore().user = {
        userId: "7",
        username: "me",
        uid: 1000007,
        displayName: "Me Myself",
        avatar: "💎",
      };
      const wrapper = await mountView(pinia);

      const selfButton = navPane(wrapper).find('[data-testid="nav-self"]');
      expect(selfButton.exists()).toBe(true);
      // Avatar prefers the emoji when set.
      expect(selfButton.find('[data-testid="avatar-emoji"]').text()).toBe("💎");
      // Label under the avatar shows the username.
      expect(selfButton.find('[data-testid="nav-username"]').text()).toBe("me");

      await selfButton.trigger("click");
      await vi.waitFor(() => {
        expect(wrapper.vm.$route.path).toBe("/profile");
      });
    });

    it("session-row avatar navigates to the peer profile without opening the chat", async () => {
      const pinia = createPinia();
      setActivePinia(pinia);
      useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
      const ws = useWsStore();
      ws.conversations.push(
        conversation(1, {
          peerUserId: "peer-1",
          peerUsername: "alice",
          peerDisplayName: "Alice",
          peerAvatar: "💎",
          lastActivityAt: isoAt(-MINUTE),
        }),
      );
      const wrapper = await mountView(pinia);

      const row = sessionsPane(wrapper).find('[data-testid="session-item"]');
      // Rendered name prefers the peer display name.
      expect(row.find('[data-testid="session-name"]').text()).toBe("Alice");
      const avatar = row.find('[data-testid="session-avatar"]');
      expect(avatar.find('[data-testid="avatar-emoji"]').text()).toBe("💎");

      // Avatar click: navigate to the peer's profile, conversation stays closed.
      await avatar.trigger("click");
      await vi.waitFor(() => {
        expect(wrapper.vm.$route.path).toBe("/profile/peer-1");
      });
      expect(ws.activeConversationId).toBeNull();

      // Row click (outside the avatar) still opens the conversation.
      await row.trigger("click");
      expect(ws.activeConversationId).toBe(1);
    });

    it("falls back to the username initial circle when the peer has no avatar", async () => {
      const pinia = createPinia();
      setActivePinia(pinia);
      useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
      const ws = useWsStore();
      ws.conversations.push(
        conversation(1, {
          peerUserId: "peer-2",
          peerUsername: "bob",
          lastActivityAt: isoAt(-MINUTE),
        }),
      );
      const wrapper = await mountView(pinia);

      const avatar = sessionsPane(wrapper).find(
        '[data-testid="session-avatar"]',
      );
      expect(avatar.find('[data-testid="avatar-initial"]').text()).toBe("B");
    });
  });

  describe("ChatView — M2 message experience", () => {
    async function mountWithM2Thread(): Promise<{
      wrapper: VueWrapper;
      ws: ReturnType<typeof useWsStore>;
    }> {
      const pinia = createPinia();
      setActivePinia(pinia);
      useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
      const ws = useWsStore();
      ws.conversations.push(
        conversation(1, { peerUsername: "alice" }),
        conversation(2, { peerUsername: "bob" }),
      );
      ws.messagesByConversation[1] = [
        message({
          messageId: "m-theirs",
          seq: 1,
          senderId: "alice",
          body: "来自对方的消息",
          sentAt: isoAt(-60_000),
        }),
        message({
          messageId: "m-mine-recent",
          seq: 2,
          senderId: "7",
          body: "我刚发的",
          sentAt: isoAt(-10_000),
          mine: true,
          status: "delivered",
        }),
        message({
          messageId: "m-gone",
          seq: 3,
          senderId: "alice",
          body: "",
          sentAt: isoAt(-5_000),
          recalled: true,
        }),
      ];
      ws.openConversation(1);

      const wrapper = await mountView(pinia);
      return { wrapper, ws };
    }

    it("renders recall tombstones as a localized italic placeholder", async () => {
      const { wrapper } = await mountWithM2Thread();
      const main = mainPane(wrapper);

      const placeholder = main.find('[data-testid="recalled-placeholder"]');
      expect(placeholder.exists()).toBe(true);
      expect(placeholder.text()).toBe("消息已撤回");
      // The tombstone renders NO normal bubble and NO action row.
      const items = main.findAll('[data-testid="message-item"]');
      const goneItem = items.find((i) =>
        i.find('[data-testid="recalled-placeholder"]').exists(),
      );
      expect(goneItem?.find('[data-testid="message-bubble"]').exists()).toBe(
        false,
      );
      expect(goneItem?.find('[data-testid="action-forward"]').exists()).toBe(
        false,
      );
    });

    it("offers reply/forward on every bubble and recall only on own recent ones", async () => {
      const { wrapper } = await mountWithM2Thread();
      const main = mainPane(wrapper);

      const items = main.findAll('[data-testid="message-item"]');
      const theirs = items.find(
        (i) =>
          i.find('[data-testid="message-bubble"]').text() === "来自对方的消息",
      );
      expect(theirs?.find('[data-testid="action-reply"]').exists()).toBe(true);
      expect(theirs?.find('[data-testid="action-forward"]').exists()).toBe(
        true,
      );
      expect(theirs?.find('[data-testid="action-recall"]').exists()).toBe(
        false,
      );

      const mineRecent = items.find(
        (i) => i.find('[data-testid="message-bubble"]').text() === "我刚发的",
      );
      expect(mineRecent?.find('[data-testid="action-recall"]').exists()).toBe(
        true,
      );
    });

    it("opens the reply context above the composer and cancels it", async () => {
      const { wrapper, ws } = await mountWithM2Thread();
      const main = mainPane(wrapper);

      expect(main.find('[data-testid="reply-context"]').exists()).toBe(false);

      const items = main.findAll('[data-testid="message-item"]');
      const theirs = items.find(
        (i) =>
          i.find('[data-testid="message-bubble"]').text() === "来自对方的消息",
      );
      await theirs?.find('[data-testid="action-reply"]').trigger("click");

      const context = main.find('[data-testid="reply-context"]');
      expect(context.exists()).toBe(true);
      expect(context.find('[data-testid="reply-context-preview"]').text()).toBe(
        "来自对方的消息",
      );
      expect(ws.replyContext?.messageId).toBe("m-theirs");

      await context.find('[data-testid="reply-cancel"]').trigger("click");
      expect(main.find('[data-testid="reply-context"]').exists()).toBe(false);
      expect(ws.replyContext).toBeNull();
    });

    it("forwards a message through the in-app picker into another conversation", async () => {
      const { wrapper, ws } = await mountWithM2Thread();
      const main = mainPane(wrapper);

      const items = main.findAll('[data-testid="message-item"]');
      const theirs = items.find(
        (i) =>
          i.find('[data-testid="message-bubble"]').text() === "来自对方的消息",
      );
      await theirs?.find('[data-testid="action-forward"]').trigger("click");

      const picker = wrapper.find('[data-testid="forward-picker"]');
      expect(picker.exists()).toBe(true);

      // Only the OTHER conversation is offered (no self-target).
      const targets = picker.findAll('[data-testid="forward-target"]');
      expect(targets).toHaveLength(1);
      expect(targets[0]?.attributes("data-conversation-id")).toBe("2");

      await targets[0]?.trigger("click");
      expect(wrapper.find('[data-testid="forward-picker"]').exists()).toBe(
        false,
      );

      const forwarded = ws.messagesByConversation[2]?.at(-1);
      expect(forwarded?.body).toBe("[转发] 来自对方的消息");
      expect(forwarded?.mine).toBe(true);
    });

    it("shows the peer typing indicator while the typing deadline is active", async () => {
      const { wrapper, ws } = await mountWithM2Thread();

      expect(
        mainPane(wrapper).find('[data-testid="typing-indicator"]').exists(),
      ).toBe(false);

      ws.conversations[0]!.peerTypingUntil = Date.now() + 60_000;
      await wrapper.vm.$nextTick();
      const indicator = mainPane(wrapper).find(
        '[data-testid="typing-indicator"]',
      );
      expect(indicator.exists()).toBe(true);
      expect(indicator.text()).toContain("对方正在输入");
    });

    it("renders the reply quote inside bubbles from server metadata", async () => {
      const pinia = createPinia();
      setActivePinia(pinia);
      useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
      const ws = useWsStore();
      ws.conversations.push(conversation(1, { peerUsername: "alice" }));
      ws.messagesByConversation[1] = [
        message({
          messageId: "m-reply",
          seq: 9,
          senderId: "alice",
          body: "这是回复",
          replyToMessageId: "m-orig",
          replyToSenderId: "7",
          replyToBodyPreview: "原始内容预览",
          sentAt: isoAt(-30_000),
        }),
      ];
      ws.openConversation(1);

      const wrapper = await mountView(pinia);
      const quote = mainPane(wrapper).find('[data-testid="reply-quote"]');
      expect(quote.exists()).toBe(true);
      expect(quote.text()).toBe("原始内容预览");
    });
  });
});
