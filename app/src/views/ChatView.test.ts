import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import type { DOMWrapper, VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import type { Pinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import type { Router } from "vue-router";
import { i18n } from "../i18n";
import { useAuthStore } from "../stores/auth";
import { useFriendsStore } from "../stores/friends";
import { useGroupsStore } from "../stores/groups";
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

  it("glides the session indicator between active rows", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const ws = useWsStore();
    ws.conversations.push(
      conversation(1, {
        peerUsername: "alice",
        lastActivityAt: isoAt(-2 * HOUR),
      }),
      conversation(2, {
        peerUsername: "bob",
        lastActivityAt: isoAt(-MINUTE),
      }),
    );

    // jsdom has no layout engine: fake the geometry the pill measures.
    const offsetTopSpy = vi
      .spyOn(HTMLElement.prototype, "offsetTop", "get")
      .mockImplementation(function (this: HTMLElement) {
        return this.getAttribute("data-conversation-id") === "1" ? 64 : 8;
      });
    const offsetHeightSpy = vi
      .spyOn(HTMLElement.prototype, "offsetHeight", "get")
      .mockReturnValue(56);

    try {
      const wrapper = await mountView(pinia);
      const list = sessionsPane(wrapper);
      const indicator = list.find('[data-testid="session-indicator"]');
      expect(indicator.exists()).toBe(true);
      // Hidden until a conversation is active.
      expect(indicator.attributes("style")).toContain("opacity: 0");

      const rows = list.findAll('[data-testid="session-item"]');
      expect(rows[0]?.attributes("data-conversation-id")).toBe("2");

      await rows[0]?.trigger("click");
      await flushPromises();
      await wrapper.vm.$nextTick();
      expect(indicator.attributes("style")).toContain("translateY(8px)");
      expect(indicator.attributes("style")).toContain("height: 56px");

      await rows[1]?.trigger("click");
      await flushPromises();
      await wrapper.vm.$nextTick();
      expect(ws.activeConversationId).toBe(1);
      expect(indicator.attributes("style")).toContain("translateY(64px)");
    } finally {
      offsetTopSpy.mockRestore();
      offsetHeightSpy.mockRestore();
    }
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
      // Optimistic bubble mirrors the source content for instant display.
      expect(forwarded?.body).toBe("来自对方的消息");
      expect(forwarded?.forwardedFromUsername).toBe("alice");
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

describe("ChatView — M8 media & emoji", () => {
  const imageMedia = {
    mediaId: "mid-img",
    kind: "image" as const,
    mime: "image/png",
    bytes: 2048,
    fileName: "p.png",
    width: 800,
    height: 600,
  };
  const videoMedia = {
    mediaId: "mid-vid",
    kind: "video" as const,
    mime: "video/mp4",
    bytes: 5_000_000,
    fileName: "v.mp4",
    width: 1280,
    height: 720,
  };

  async function mountWithMedia(): Promise<{
    wrapper: VueWrapper;
    ws: ReturnType<typeof useWsStore>;
  }> {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const ws = useWsStore();
    ws.conversations.push(
      conversation(1, {
        peerUsername: "alice",
        lastMessagePreview: "",
        lastMessageKind: "image",
      }),
    );
    ws.messagesByConversation[1] = [
      message({
        messageId: "m-img",
        seq: 1,
        senderId: "alice",
        body: "",
        media: imageMedia,
        sentAt: isoAt(-60_000),
      }),
      message({
        messageId: "m-vid",
        seq: 2,
        senderId: "7",
        body: "",
        media: videoMedia,
        mine: true,
        status: "delivered",
        sentAt: isoAt(-30_000),
      }),
    ];
    ws.openConversation(1);
    ws.status = "open";

    const wrapper = await mountView(pinia);
    return { wrapper, ws };
  }

  it("renders image and video bubbles with natural sizing metadata", async () => {
    const { wrapper } = await mountWithMedia();
    const main = mainPane(wrapper);

    const img = main.find('[data-testid="media-image"]');
    expect(img.exists()).toBe(true);
    expect(img.attributes("src")).toBe("/api/media/mid-img");
    expect(img.attributes("width")).toBe("800");
    expect(img.attributes("height")).toBe("600");

    const video = main.find('[data-testid="media-video"]');
    expect(video.exists()).toBe(true);
    expect(video.attributes("controls")).toBeDefined();
    expect(video.attributes("preload")).toBe("metadata");
    expect(video.attributes("src")).toBe("/api/media/mid-vid");
    // Video file-size caption.
    expect(main.find('[data-testid="media-size"]').text()).toBe("4.8 MB");

    // Media bubbles never render the plain-text bubble.
    expect(main.findAll('[data-testid="message-bubble"]')).toHaveLength(0);
  });

  it("lazily sizes video via width/height and keeps the image zoomable", async () => {
    const { wrapper } = await mountWithMedia();
    const img = mainPane(wrapper).find('[data-testid="media-image"]');
    expect(img.classes()).toContain("cursor-zoom-in");
  });

  it("opens the lightbox on image click and closes it on overlay click", async () => {
    const { wrapper } = await mountWithMedia();
    const main = mainPane(wrapper);

    expect(main.find('[data-testid="media-lightbox"]').exists()).toBe(false);

    await main.find('[data-testid="media-image"]').trigger("click");
    const lightbox = wrapper.find('[data-testid="media-lightbox"]');
    expect(lightbox.exists()).toBe(true);
    expect(
      lightbox.find('[data-testid="media-lightbox-image"]').attributes("src"),
    ).toBe("/api/media/mid-img");

    await lightbox.trigger("click");
    expect(wrapper.find('[data-testid="media-lightbox"]').exists()).toBe(false);
  });

  it("closes the lightbox on Escape", async () => {
    const { wrapper } = await mountWithMedia();
    await mainPane(wrapper)
      .find('[data-testid="media-image"]')
      .trigger("click");
    expect(wrapper.find('[data-testid="media-lightbox"]').exists()).toBe(true);

    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await wrapper.vm.$nextTick();
    expect(wrapper.find('[data-testid="media-lightbox"]').exists()).toBe(false);
  });

  it("shows a localized media placeholder in the session preview", async () => {
    const { wrapper } = await mountWithMedia();
    expect(
      sessionsPane(wrapper).find('[data-testid="session-preview"]').text(),
    ).toBe("[图片]");
  });

  it("toggles the emoji panel, appends choices to the draft, and closes on backdrop", async () => {
    const { wrapper } = await mountWithMedia();
    const main = mainPane(wrapper);

    expect(main.find('[data-testid="emoji-panel"]').exists()).toBe(false);
    await main.find('[data-testid="emoji-toggle"]').trigger("click");

    const panel = main.find('[data-testid="emoji-panel"]');
    expect(panel.exists()).toBe(true);
    const choices = panel.findAll('[data-testid="emoji-choice"]');
    expect(choices.length).toBeGreaterThanOrEqual(60);

    const first = choices[0]!;
    const firstText = first.text();
    await first.trigger("click");
    const input = main.find('[data-testid="composer-input"]')
      .element as HTMLTextAreaElement;
    expect(input.value).toBe(firstText);
    // Panel stays open for multiple inserts.
    expect(main.find('[data-testid="emoji-panel"]').exists()).toBe(true);

    const second = main
      .find('[data-testid="emoji-panel"]')
      .findAll('[data-testid="emoji-choice"]')[1]!;
    await second.trigger("click");
    expect(input.value).toBe(firstText + second.text());

    await main.find('[data-testid="emoji-backdrop"]').trigger("click");
    expect(main.find('[data-testid="emoji-panel"]').exists()).toBe(false);
  });

  it("renders the emoji toggle as a monochrome svg icon instead of a glyph", async () => {
    const { wrapper } = await mountWithMedia();
    const toggle = mainPane(wrapper).find('[data-testid="emoji-toggle"]');

    const icon = toggle.find("svg");
    expect(icon.exists()).toBe(true);
    // Line icon: drawn with currentColor, no colored emoji text remains.
    expect(icon.attributes("stroke")).toBe("currentColor");
    expect(toggle.text()).toBe("");
    expect(toggle.attributes("title")).toBeTruthy();
    expect(toggle.attributes("aria-label")).toBeTruthy();
  });

  it("rejects an oversized image locally before any upload", async () => {
    const { wrapper, ws } = await mountWithMedia();
    ws.status = "open";
    await wrapper.vm.$nextTick();
    const main = mainPane(wrapper);

    const file = new File(["x"], "big.png", { type: "image/png" });
    Object.defineProperty(file, "size", { value: 16 * 1024 * 1024 });
    const input = main.find('[data-testid="image-file-input"]')
      .element as HTMLInputElement;
    Object.defineProperty(input, "files", {
      value: [file],
      configurable: true,
    });

    await main.find('[data-testid="image-file-input"]').trigger("change");
    await wrapper.vm.$nextTick();

    const error = main.find('[data-testid="upload-error"]');
    expect(error.exists()).toBe(true);
    expect(error.text()).toContain("15 MB");
  });

  it("triggers a hidden file input from the attach buttons", async () => {
    const { wrapper } = await mountWithMedia();
    const main = mainPane(wrapper);

    // The shell re-instantiates each slot per breakpoint, so the ref resolves
    // to one of several hidden inputs — spy at the prototype level instead.
    const clickSpy = vi.spyOn(HTMLInputElement.prototype, "click");

    await main.find('[data-testid="attach-image"]').trigger("click");
    expect(clickSpy).toHaveBeenCalledTimes(1);
    await main.find('[data-testid="attach-video"]').trigger("click");
    expect(clickSpy).toHaveBeenCalledTimes(2);
    await main.find('[data-testid="attach-audio"]').trigger("click");
    expect(clickSpy).toHaveBeenCalledTimes(3);

    clickSpy.mockRestore();
  });
});

describe("ChatView — M9 voice, audio player & forward attribution", () => {
  class MockMediaRecorder {
    static instances: MockMediaRecorder[] = [];
    state = "inactive";
    mimeType = "audio/webm;codecs=opus";
    audioBitsPerSecond = 0;
    ondataavailable: ((event: { data: Blob }) => void) | null = null;
    onstop: (() => void) | null = null;
    onerror: (() => void) | null = null;
    constructor(
      _stream: unknown,
      options?: { mimeType?: string; audioBitsPerSecond?: number },
    ) {
      if (options?.mimeType !== undefined) this.mimeType = options.mimeType;
      if (options?.audioBitsPerSecond !== undefined) {
        this.audioBitsPerSecond = options.audioBitsPerSecond;
      }
      MockMediaRecorder.instances.push(this);
    }
    static isTypeSupported(): boolean {
      return true;
    }
    start(): void {
      this.state = "recording";
    }
    stop(): void {
      this.state = "inactive";
      this.ondataavailable?.({
        data: new Blob(["voice-bytes"], { type: "audio/webm" }),
      });
      this.onstop?.();
    }
  }

  class MockUploadXHR {
    static instances: MockUploadXHR[] = [];
    method = "";
    url = "";
    requestHeaders = new Map<string, string>();
    sentBody: unknown = null;
    status = 0;
    responseText = "";
    responseType = "";
    upload: {
      onprogress: ((event: { loaded: number; total: number }) => void) | null;
    } = { onprogress: null };
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    constructor() {
      MockUploadXHR.instances.push(this);
    }
    open(method: string, url: string): void {
      this.method = method;
      this.url = url;
    }
    setRequestHeader(name: string, value: string): void {
      this.requestHeaders.set(name, value);
    }
    send(body: unknown): void {
      this.sentBody = body;
      this.status = 201;
      this.responseText = JSON.stringify({
        media_id: "mid-voice",
        kind: "audio",
        mime: "audio/webm",
        bytes: 11,
        file_name: "voice.webm",
      });
      this.onload?.();
    }
  }

  class MockAudio {
    currentTime = 0;
    duration = 4.2;
    src = "";
    paused = true;
    constructor(src: string) {
      this.src = src;
    }
    addEventListener(): void {
      // timeupdate/ended/error listeners are irrelevant in jsdom.
    }
    play(): Promise<void> {
      this.paused = false;
      return Promise.resolve();
    }
    pause(): void {
      this.paused = true;
    }
  }

  const audioMedia = {
    mediaId: "mid-audio",
    kind: "audio" as const,
    mime: "audio/webm",
    bytes: 2048,
    fileName: "voice.webm",
    durationMs: 4200,
  };

  let originalMediaDevices: PropertyDescriptor | undefined;

  beforeEach(() => {
    MockMediaRecorder.instances = [];
    MockUploadXHR.instances = [];
    originalMediaDevices = Object.getOwnPropertyDescriptor(
      navigator,
      "mediaDevices",
    );
    vi.stubGlobal("MediaRecorder", MockMediaRecorder);
    vi.stubGlobal("XMLHttpRequest", MockUploadXHR);
    vi.stubGlobal("Audio", MockAudio);
    Object.defineProperty(navigator, "mediaDevices", {
      configurable: true,
      value: {
        getUserMedia: vi
          .fn()
          .mockResolvedValue({ getTracks: () => [{ stop: vi.fn() }] }),
      },
    });
  });

  afterEach(() => {
    if (originalMediaDevices === undefined) {
      delete (navigator as { mediaDevices?: unknown }).mediaDevices;
    } else {
      Object.defineProperty(navigator, "mediaDevices", originalMediaDevices);
    }
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  async function mountVoiceConversation(): Promise<{
    wrapper: VueWrapper;
    ws: ReturnType<typeof useWsStore>;
  }> {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    useAuthStore().accessToken = "tok";
    const ws = useWsStore();
    ws.conversations.push(conversation(1, { peerUsername: "alice" }));
    ws.openConversation(1);
    ws.status = "open";
    const wrapper = await mountView(pinia);
    return { wrapper, ws };
  }

  it("records, stops, uploads the webm and sends an audio message with duration_ms", async () => {
    const { wrapper, ws } = await mountVoiceConversation();
    const main = mainPane(wrapper);

    await main.find('[data-testid="voice-record"]').trigger("click");
    await flushPromises();

    // Recording UI replaces the composer and shows a live mm:ss timer.
    expect(main.find('[data-testid="voice-timer"]').exists()).toBe(true);
    expect(main.find('[data-testid="composer-input"]').exists()).toBe(false);

    // Clarity regression: the recorder must run at the high opus bitrate
    // (Chromium's default ~32 kbps sounded muffled).
    const recorder = MockMediaRecorder.instances.at(-1);
    expect(recorder?.audioBitsPerSecond).toBe(128000);

    await main.find('[data-testid="voice-stop"]').trigger("click");
    await flushPromises();
    await flushPromises();

    const xhr = MockUploadXHR.instances.at(-1);
    expect(xhr).toBeDefined();
    expect(xhr?.method).toBe("POST");
    expect(xhr?.url).toBe("/api/media");
    const uploaded = xhr?.sentBody as File;
    expect(uploaded).toBeInstanceOf(File);
    expect(uploaded.type).toBe("audio/webm");
    expect(uploaded.name).toMatch(/^voice-.*\.webm$/);

    const sent = ws.messagesByConversation[1]?.at(-1);
    expect(sent?.media?.kind).toBe("audio");
    expect(sent?.media?.mediaId).toBe("mid-voice");
    expect(typeof sent?.media?.durationMs).toBe("number");
    expect((sent?.media?.durationMs ?? -1) >= 0).toBe(true);

    // Recording UI is fully torn down after a successful send.
    expect(main.find('[data-testid="voice-timer"]').exists()).toBe(false);
    expect(main.find('[data-testid="composer-input"]').exists()).toBe(true);
  });

  it("cancels a recording without uploading anything", async () => {
    const { wrapper, ws } = await mountVoiceConversation();
    const main = mainPane(wrapper);

    await main.find('[data-testid="voice-record"]').trigger("click");
    await flushPromises();
    await main.find('[data-testid="voice-cancel"]').trigger("click");
    await flushPromises();
    await flushPromises();

    expect(MockUploadXHR.instances).toHaveLength(0);
    expect(ws.messagesByConversation[1] ?? []).toHaveLength(0);
    expect(main.find('[data-testid="composer-input"]').exists()).toBe(true);
  });

  it("shows a localized error when the microphone is denied", async () => {
    const getUserMedia = vi
      .fn()
      .mockRejectedValue(new Error("NotAllowedError"));
    Object.defineProperty(navigator, "mediaDevices", {
      configurable: true,
      value: { getUserMedia },
    });
    const { wrapper } = await mountVoiceConversation();
    const main = mainPane(wrapper);

    await main.find('[data-testid="voice-record"]').trigger("click");
    await flushPromises();

    const error = main.find('[data-testid="upload-error"]');
    expect(error.exists()).toBe(true);
    expect(error.text()).toContain("麦克风");
    // The composer is restored, never stuck in a recording state.
    expect(main.find('[data-testid="composer-input"]').exists()).toBe(true);
    expect(main.find('[data-testid="voice-timer"]').exists()).toBe(false);
  });

  it("renders a custom audio player with duration and toggles play/pause", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const ws = useWsStore();
    ws.conversations.push(conversation(1, { peerUsername: "alice" }));
    ws.messagesByConversation[1] = [
      message({
        messageId: "m-audio",
        seq: 1,
        senderId: "alice",
        body: "",
        media: audioMedia,
        sentAt: isoAt(-30_000),
      }),
    ];
    ws.openConversation(1);
    ws.status = "open";
    const wrapper = await mountView(pinia);
    const main = mainPane(wrapper);

    expect(main.find('[data-testid="media-audio"]').exists()).toBe(true);
    expect(main.find('[data-testid="audio-play"]').exists()).toBe(true);
    expect(main.find('[data-testid="audio-progress"]').exists()).toBe(true);
    expect(main.find('[data-testid="media-audio"]').text()).toContain("00:04");
    // No native audio chrome.
    expect(main.find("audio").exists()).toBe(false);

    await main.find('[data-testid="audio-play"]').trigger("click");
    await wrapper.vm.$nextTick();
    expect(main.find('[data-testid="audio-play"]').text()).toContain("❚❚");

    await main.find('[data-testid="audio-play"]').trigger("click");
    await wrapper.vm.$nextTick();
    expect(main.find('[data-testid="audio-play"]').text()).toContain("▶");
  });

  it("shows a localized voice session preview", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const ws = useWsStore();
    ws.conversations.push(
      conversation(1, {
        peerUsername: "alice",
        lastMessagePreview: "",
        lastMessageKind: "audio",
      }),
    );
    const wrapper = await mountView(pinia);
    expect(
      sessionsPane(wrapper).find('[data-testid="session-preview"]').text(),
    ).toBe("[语音]");
  });

  it("renders a 转发自 attribution badge on text and media bubbles", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const ws = useWsStore();
    ws.conversations.push(conversation(1, { peerUsername: "alice" }));
    ws.messagesByConversation[1] = [
      message({
        messageId: "m-fwd-text",
        seq: 1,
        senderId: "7",
        body: "转发的文字",
        forwardedFromUsername: "carol",
        mine: true,
        status: "delivered",
        sentAt: isoAt(-20_000),
      }),
      message({
        messageId: "m-fwd-media",
        seq: 2,
        senderId: "7",
        body: "",
        media: audioMedia,
        forwardedFromUsername: "dave",
        mine: true,
        status: "delivered",
        sentAt: isoAt(-10_000),
      }),
    ];
    ws.openConversation(1);
    ws.status = "open";
    const wrapper = await mountView(pinia);
    const main = mainPane(wrapper);

    const badges = main.findAll('[data-testid="forward-badge"]');
    expect(badges).toHaveLength(2);
    const texts = badges.map((b) => b.text());
    expect(texts.some((t) => t.includes("carol"))).toBe(true);
    expect(texts.some((t) => t.includes("dave"))).toBe(true);
  });

  it("hides attachment and voice buttons in secret conversations", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const ws = useWsStore();
    ws.conversations.push(
      conversation(1, { peerUsername: "alice", kind: "secret" }),
    );
    ws.openConversation(1);
    ws.status = "open";
    const wrapper = await mountView(pinia);
    const main = mainPane(wrapper);

    expect(main.find('[data-testid="attach-image"]').exists()).toBe(false);
    expect(main.find('[data-testid="attach-video"]').exists()).toBe(false);
    expect(main.find('[data-testid="attach-audio"]').exists()).toBe(false);
    expect(main.find('[data-testid="voice-record"]').exists()).toBe(false);
    // Emoji panel stays available in secret chats.
    expect(main.find('[data-testid="emoji-toggle"]').exists()).toBe(true);
  });
});

describe("ChatView — M11 groups", () => {
  function jsonResponse(status: number, body: unknown): Response {
    return new Response(JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    });
  }

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders a group session with its name and indigo initial avatar", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const ws = useWsStore();
    ws.conversations.push(
      conversation(3, {
        kind: "group",
        name: "团队",
        lastActivityAt: isoAt(-MINUTE),
      }),
    );

    const wrapper = await mountView(pinia);
    const row = sessionsPane(wrapper).find('[data-testid="session-item"]');
    expect(row.find('[data-testid="session-name"]').text()).toBe("团队");
    const avatar = row.find('[data-testid="session-avatar"]');
    expect(avatar.find('[data-testid="avatar-initial"]').text()).toBe("团");
    expect(avatar.find('[data-testid="avatar-initial"]').classes()).toContain(
      "bg-indigo-500",
    );
  });

  it("creates a group from the dialog and opens it", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(201, {
        conversation_id: 9,
        name: "团队",
        member_count: 2,
        invited: ["alice"],
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const pinia = createPinia();
    setActivePinia(pinia);
    const auth = useAuthStore();
    auth.user = { userId: "7", username: "me", uid: 1000007 };
    auth.accessToken = "tok";
    const friends = useFriendsStore();
    friends.loaded = true;
    friends.friends.push({
      user_id: "u1",
      username: "alice",
      uid: 100001,
      since: "2026-01-01T00:00:00Z",
    });

    const wrapper = await mountView(pinia);
    const pane = sessionsPane(wrapper);
    await pane.find('[data-testid="new-group-button"]').trigger("click");
    expect(wrapper.find('[data-testid="group-create-dialog"]').exists()).toBe(
      true,
    );

    await wrapper.find('[data-testid="group-name-input"]').setValue("团队");
    await wrapper
      .find('[data-testid="group-candidate"] button')
      .trigger("click");
    expect(
      wrapper.find('[data-testid="group-selected-count"]').text(),
    ).toContain("1");
    await wrapper.find('[data-testid="group-create-submit"]').trigger("click");
    await flushPromises();

    const ws = useWsStore();
    const created = ws.conversations.find((c) => c.conversationId === 9);
    expect(created?.kind).toBe("group");
    expect(created?.name).toBe("团队");
    expect(ws.activeConversationId).toBe(9);
    expect(wrapper.find('[data-testid="group-create-dialog"]').exists()).toBe(
      false,
    );

    const [path, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(path).toBe("/api/groups");
    expect(JSON.parse(String(init.body))).toEqual({
      name: "团队",
      invite_usernames: ["alice"],
    });
  });

  it("shows the uploaded audio file name but hides generated voice names", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const ws = useWsStore();
    ws.conversations.push(conversation(1, { peerUsername: "alice" }));
    ws.messagesByConversation[1] = [
      message({
        messageId: "m-audio-file",
        seq: 1,
        senderId: "alice",
        body: "",
        sentAt: isoAt(-60_000),
        media: {
          mediaId: "mid-1",
          kind: "audio",
          mime: "audio/mpeg",
          bytes: 2048,
          fileName: "meeting-notes.mp3",
          durationMs: 4200,
        },
      }),
      message({
        messageId: "m-audio-voice",
        seq: 2,
        senderId: "alice",
        body: "",
        sentAt: isoAt(-30_000),
        media: {
          mediaId: "mid-2",
          kind: "audio",
          mime: "audio/webm",
          bytes: 1024,
          fileName: "voice-1700000000000.webm",
          durationMs: 3000,
        },
      }),
    ];
    ws.openConversation(1);
    ws.status = "open";

    const wrapper = await mountView(pinia);
    const main = mainPane(wrapper);
    const names = main.findAll('[data-testid="audio-filename"]');
    expect(names).toHaveLength(1);
    expect(names[0]?.text()).toBe("meeting-notes.mp3");
  });

  function member(
    user_id: string,
    role: "owner" | "admin" | "member",
    username = user_id,
  ) {
    return {
      user_id,
      username,
      role,
      joined_at: "2026-01-01T00:00:00Z",
    };
  }

  async function mountGroupPanel(
    myId: string,
    myRole: "owner" | "admin" | "member",
    members: ReturnType<typeof member>[],
  ): Promise<VueWrapper> {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(jsonResponse(404, { error: "not_found" })),
    );
    const pinia = createPinia();
    setActivePinia(pinia);
    const auth = useAuthStore();
    auth.user = { userId: myId, username: "me", uid: 1000007 };
    auth.accessToken = "tok";
    const ws = useWsStore();
    ws.conversations.push(conversation(1, { kind: "group", name: "团队" }));
    useGroupsStore().applyGroupInfo({
      conversation_id: 1,
      name: "团队",
      my_role: myRole,
      member_count: members.length,
      members,
    });
    ws.openConversation(1);
    const wrapper = await mountView(pinia);
    await mainPane(wrapper)
      .find('[data-testid="group-info-button"]')
      .trigger("click");
    await wrapper.vm.$nextTick();
    return wrapper;
  }

  it("owner sees kick on every other member, plus role and transfer actions", async () => {
    const wrapper = await mountGroupPanel("7", "owner", [
      member("7", "owner"),
      member("u2", "admin"),
      member("u3", "member"),
    ]);
    const main = mainPane(wrapper);
    expect(main.find('[data-testid="group-info-panel"]').exists()).toBe(true);
    expect(main.findAll('[data-testid="group-member"]')).toHaveLength(3);
    expect(main.findAll('[data-testid="group-kick"]')).toHaveLength(2);
    expect(main.findAll('[data-testid="group-transfer"]')).toHaveLength(2);
    expect(main.find('[data-testid="group-demote"]').exists()).toBe(true);
    expect(main.find('[data-testid="group-appoint"]').exists()).toBe(true);
    // Owner cannot leave; the hint replaces the leave button.
    expect(main.find('[data-testid="group-leave"]').exists()).toBe(false);
    expect(main.find('[data-testid="group-owner-leave-hint"]').exists()).toBe(
      true,
    );
  });

  it("renders the group info as a right-side drawer with a frosted backdrop", async () => {
    const wrapper = await mountGroupPanel("7", "owner", [
      member("7", "owner"),
      member("u2", "member"),
    ]);
    const panel = mainPane(wrapper).find('[data-testid="group-info-panel"]');
    expect(panel.exists()).toBe(true);

    const drawer = panel.find('[data-testid="group-info-drawer"]');
    expect(drawer.exists()).toBe(true);
    const drawerClasses = drawer.classes();
    expect(drawerClasses).toContain("group-drawer-panel");
    expect(drawerClasses).toContain("right-0");
    expect(drawerClasses).toContain("max-w-[90vw]");
    expect(drawerClasses).toContain("overflow-y-auto");
    expect(drawerClasses).toContain("shadow-2xl");
    expect(drawer.attributes("role")).toBe("dialog");

    const backdrop = panel.find('[data-testid="group-info-backdrop"]');
    expect(backdrop.classes()).toContain("group-drawer-backdrop");
    expect(backdrop.classes()).toContain("bg-black/30");
    expect(backdrop.classes()).toContain("backdrop-blur-md");

    // Backdrop click still closes the panel.
    await backdrop.trigger("click");
    await wrapper.vm.$nextTick();
    expect(
      mainPane(wrapper).find('[data-testid="group-info-panel"]').exists(),
    ).toBe(false);
  });

  it("admin can only kick plain members and cannot leave-manage roles", async () => {
    const wrapper = await mountGroupPanel("7", "admin", [
      member("u1", "owner"),
      member("7", "admin"),
      member("u3", "member"),
    ]);
    const main = mainPane(wrapper);
    expect(main.findAll('[data-testid="group-kick"]')).toHaveLength(1);
    expect(main.find('[data-testid="group-transfer"]').exists()).toBe(false);
    expect(main.find('[data-testid="group-appoint"]').exists()).toBe(false);
    expect(main.find('[data-testid="group-demote"]').exists()).toBe(false);
    expect(main.find('[data-testid="group-leave"]').exists()).toBe(true);
    // Admin may invite.
    expect(main.find('[data-testid="group-invite-input"]').exists()).toBe(true);
  });

  it("plain members see no management actions at all", async () => {
    const wrapper = await mountGroupPanel("7", "member", [
      member("u1", "owner"),
      member("7", "member"),
    ]);
    const main = mainPane(wrapper);
    expect(main.find('[data-testid="group-kick"]').exists()).toBe(false);
    expect(main.find('[data-testid="group-transfer"]').exists()).toBe(false);
    expect(main.find('[data-testid="group-appoint"]').exists()).toBe(false);
    expect(main.find('[data-testid="group-demote"]').exists()).toBe(false);
    expect(main.find('[data-testid="group-invite-input"]').exists()).toBe(
      false,
    );
    expect(main.find('[data-testid="group-leave"]').exists()).toBe(true);
  });
});
