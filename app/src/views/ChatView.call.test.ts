import { beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import type { VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import type { Pinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import type { Router } from "vue-router";

import { i18n } from "../i18n";
import { useAuthStore } from "../stores/auth";
import { useCallStore } from "../stores/call";
import { useWsStore } from "../stores/ws";
import type { Conversation } from "../stores/ws";
import ChatView from "./ChatView.vue";

function conversation(
  conversationId: number,
  overrides: Partial<Conversation> = {},
): Conversation {
  return {
    conversationId,
    peerUserId: "u-peer",
    peerUsername: "peer",
    peerDisplayName: "Peer",
    lastMessagePreview: null,
    lastActivityAt: "",
    unread: 0,
    lastSeenSeq: 0,
    maxSeq: 0,
    peerTypingUntil: null,
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
  return mount(ChatView, { global: { plugins: [pinia, i18n, router] } });
}

/** AppShell renders the main slot once per breakpoint; scope to desktop. */
function mainPane(wrapper: VueWrapper) {
  return wrapper.find('[data-testid="shell-main-desktop"]');
}

beforeEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
});

describe("ChatView — call affordances", () => {
  it("shows the call button in a direct conversation", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "u-me", username: "me", uid: 1 };
    const ws = useWsStore();
    ws.conversations.push(conversation(1));
    ws.activeConversationId = 1;

    const wrapper = await mountView(pinia);

    expect(mainPane(wrapper).find('[data-testid="call-start"]').exists()).toBe(
      true,
    );
  });

  it("hides the call button in a secret conversation", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "u-me", username: "me", uid: 1 };
    const ws = useWsStore();
    ws.conversations.push(conversation(1, { kind: "secret" }));
    ws.activeConversationId = 1;

    const wrapper = await mountView(pinia);

    expect(mainPane(wrapper).find('[data-testid="call-start"]').exists()).toBe(
      false,
    );
    // Sanity: the secret thread still renders.
    expect(mainPane(wrapper).find('[data-testid="thread-lock"]').exists()).toBe(
      true,
    );
  });

  it("clicking call-start invokes startCall for the active conversation", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "u-me", username: "me", uid: 1 };
    const ws = useWsStore();
    ws.conversations.push(conversation(1));
    ws.activeConversationId = 1;
    const calls = useCallStore();
    const startSpy = vi.spyOn(calls, "startCall").mockImplementation(() => {});

    const wrapper = await mountView(pinia);
    await mainPane(wrapper).find('[data-testid="call-start"]').trigger("click");

    expect(startSpy).toHaveBeenCalledWith(1);
  });
});

describe("ChatView — incoming call dialog", () => {
  it("renders the caller and routes accept/reject to the store", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "u-me", username: "me", uid: 1 };
    useWsStore().conversations.push(conversation(1));
    const calls = useCallStore();
    calls.status = "incoming";
    calls.peer = { userId: "u-peer", username: "peer", displayName: "Peer" };
    calls.conversationId = 1;
    calls.callId = "call-1";
    const acceptSpy = vi
      .spyOn(calls, "acceptIncoming")
      .mockResolvedValue(undefined);
    const rejectSpy = vi
      .spyOn(calls, "rejectIncoming")
      .mockImplementation(() => {});

    const wrapper = await mountView(pinia);
    const dialog = mainPane(wrapper).find('[data-testid="incoming-call"]');
    expect(dialog.exists()).toBe(true);
    expect(dialog.find('[data-testid="incoming-caller"]').text()).toContain(
      "Peer",
    );

    await dialog.find('[data-testid="incoming-accept"]').trigger("click");
    expect(acceptSpy).toHaveBeenCalledTimes(1);

    await dialog.find('[data-testid="incoming-reject"]').trigger("click");
    expect(rejectSpy).toHaveBeenCalledTimes(1);
  });
});

describe("ChatView — screen-share quality picker", () => {
  async function activeCallView(): Promise<{
    wrapper: VueWrapper;
    calls: ReturnType<typeof useCallStore>;
  }> {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "u-me", username: "me", uid: 1 };
    useWsStore().conversations.push(conversation(1));
    useWsStore().activeConversationId = 1;
    const calls = useCallStore();
    calls.status = "active";
    calls.conversationId = 1;
    calls.callId = "call-1";
    calls.startedAt = Date.now();
    calls.peer = { userId: "u-peer", username: "peer", displayName: "Peer" };
    return { wrapper: await mountView(pinia), calls };
  }

  it("shows the panel and renders all resolution/framerate presets", async () => {
    const { wrapper } = await activeCallView();
    const panel = mainPane(wrapper).find('[data-testid="call-panel"]');
    expect(panel.exists()).toBe(true);

    await panel.find('[data-testid="call-share"]').trigger("click");

    const picker = panel.find('[data-testid="share-quality"]');
    expect(picker.exists()).toBe(true);
    const resolutions = picker.findAll('[data-testid="share-resolution"]');
    expect(resolutions).toHaveLength(4);
    expect(resolutions.map((r) => r.attributes("data-value"))).toEqual([
      "480p",
      "720p",
      "1080p",
      "1440p",
    ]);
    expect(picker.findAll('[data-testid="share-fps"]')).toHaveLength(3);
  });

  it("calls startShare with the selected quality", async () => {
    const { wrapper, calls } = await activeCallView();
    const shareSpy = vi.spyOn(calls, "startShare").mockResolvedValue(undefined);

    const panel = mainPane(wrapper).find('[data-testid="call-panel"]');
    await panel.find('[data-testid="call-share"]').trigger("click");
    const picker = panel.find('[data-testid="share-quality"]');
    await picker
      .find('[data-testid="share-resolution"][data-value="1080p"]')
      .trigger("click");
    await picker
      .find('[data-testid="share-fps"][data-value="60"]')
      .trigger("click");
    await picker.find('[data-testid="share-confirm"]').trigger("click");

    expect(shareSpy).toHaveBeenCalledWith({
      label: "1080p",
      width: 1920,
      height: 1080,
      frameRate: 60,
    });
  });

  it("mute and hangup buttons drive the store", async () => {
    const { wrapper, calls } = await activeCallView();
    const muteSpy = vi.spyOn(calls, "toggleMute").mockImplementation(() => {});
    const hangupSpy = vi.spyOn(calls, "hangup").mockImplementation(() => {});

    const panel = mainPane(wrapper).find('[data-testid="call-panel"]');
    await panel.find('[data-testid="call-mute"]').trigger("click");
    await panel.find('[data-testid="call-hangup"]').trigger("click");

    expect(muteSpy).toHaveBeenCalledTimes(1);
    expect(hangupSpy).toHaveBeenCalledTimes(1);
  });
});

describe("ChatView — call status + screen stage", () => {
  it("renders a non-empty status label in every in-call panel state", async () => {
    for (const status of ["outgoing", "connecting", "active"] as const) {
      const pinia = createPinia();
      setActivePinia(pinia);
      useAuthStore().user = { userId: "u-me", username: "me", uid: 1 };
      const ws = useWsStore();
      ws.conversations.push(conversation(1));
      ws.activeConversationId = 1;
      const calls = useCallStore();
      calls.status = status;
      calls.conversationId = 1;
      calls.callId = "call-1";
      calls.startedAt = Date.now();
      calls.peer = { userId: "u-peer", username: "peer", displayName: "Peer" };

      const wrapper = await mountView(pinia);
      const label = mainPane(wrapper).find('[data-testid="call-status"]');
      expect(label.exists()).toBe(true);
      expect(label.text().trim().length).toBeGreaterThan(0);
      wrapper.unmount();
    }
  });

  it("renders the screen stage when the call conversation is active and a peer shares", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "u-me", username: "me", uid: 1 };
    const ws = useWsStore();
    ws.conversations.push(conversation(1));
    ws.activeConversationId = 1;
    const calls = useCallStore();
    calls.status = "active";
    calls.conversationId = 1;
    calls.callId = "call-1";
    calls.startedAt = Date.now();
    calls.peer = { userId: "u-peer", username: "peer", displayName: "Peer" };
    calls.remoteVideoUsers = ["u-peer"];
    calls.remoteStreams = { "u-peer": {} as unknown as MediaStream };

    const wrapper = await mountView(pinia);
    const pane = mainPane(wrapper);

    expect(pane.find('[data-testid="screen-stage"]').exists()).toBe(true);
    expect(pane.find('[data-testid="screen-remote-video"]').exists()).toBe(
      true,
    );
  });
});
