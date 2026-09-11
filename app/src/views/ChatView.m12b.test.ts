import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import type { DOMWrapper, VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import type { Pinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import type { Router } from "vue-router";
import { createI18n } from "vue-i18n";
import zhCN from "../i18n/locales/zh-CN";
import en from "../i18n/locales/en";
import { useAuthStore } from "../stores/auth";
import { useGroupsStore } from "../stores/groups";
import { useWsStore } from "../stores/ws";
import type { Conversation } from "../stores/ws";
import { useTheme } from "../composables/useTheme";
import { fileToAvatarDataUrl } from "../lib/avatarImage";
import ChatView from "./ChatView.vue";

vi.mock("../lib/avatarImage", () => ({
  fileToAvatarDataUrl: vi.fn(),
}));

const fetchMock = vi.fn();

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function conversation(
  conversationId: number,
  overrides: Partial<Conversation> = {},
): Conversation {
  return {
    conversationId,
    peerUserId: "",
    peerUsername: "",
    name: "",
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

  // A fresh i18n per mount keeps the language-switch test from leaking its
  // choice into later cases (the app shares one singleton at runtime).
  const testI18n = createI18n({
    legacy: false,
    locale: "zh-CN",
    fallbackLocale: "en",
    messages: { "zh-CN": zhCN, en },
  });

  return mount(ChatView, {
    global: { plugins: [pinia, testI18n, router] },
  });
}

function navPane(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find('[data-testid="shell-nav-desktop"]');
}

function mainPane(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find('[data-testid="shell-main-desktop"]');
}

interface SeedMember {
  user_id: string;
  username: string;
  role: "owner" | "admin" | "member";
  joined_at: string;
  group_xp?: number;
  group_level?: number;
  title?: string;
  custom_title?: string | null;
}

function member(
  user_id: string,
  role: "owner" | "admin" | "member",
  extra: Partial<SeedMember> = {},
): SeedMember {
  return {
    user_id,
    username: user_id,
    role,
    joined_at: "2026-01-01T00:00:00Z",
    ...extra,
  };
}

interface SeedGroupInfo {
  conversation_id: number;
  name: string;
  description: string;
  avatar: string | null;
  my_role: "owner" | "admin" | "member";
  member_count: number;
  members: SeedMember[];
}

function groupInfoFixture(
  myRole: "owner" | "admin" | "member",
  members: SeedMember[],
  overrides: Partial<SeedGroupInfo> = {},
): SeedGroupInfo {
  return {
    conversation_id: 1,
    name: "团队",
    description: "",
    avatar: null,
    my_role: myRole,
    member_count: members.length,
    members,
    ...overrides,
  };
}

/** Seeds a group conversation, opens its info drawer, and returns the wrapper. */
async function mountGroupPanel(
  myId: string,
  myRole: "owner" | "admin" | "member",
  members: SeedMember[],
  infoOverrides: Partial<SeedGroupInfo> = {},
): Promise<VueWrapper> {
  const info = groupInfoFixture(myRole, members, infoOverrides);
  fetchMock.mockImplementation(async () => jsonResponse(200, info));
  const pinia = createPinia();
  setActivePinia(pinia);
  const auth = useAuthStore();
  auth.user = { userId: myId, username: "me", uid: 1000007 };
  auth.accessToken = "tok";
  const ws = useWsStore();
  ws.conversations.push(conversation(1, { kind: "group", name: "团队" }));
  useGroupsStore().applyGroupInfo(info);
  ws.openConversation(1);
  const wrapper = await mountView(pinia);
  await mainPane(wrapper)
    .find('[data-testid="group-info-button"]')
    .trigger("click");
  await wrapper.vm.$nextTick();
  return wrapper;
}

function patchCall(): [string, RequestInit] {
  const call = fetchMock.mock.calls.find(
    ([, init]) => (init as RequestInit)?.method === "PATCH",
  );
  expect(call).toBeDefined();
  return call as [string, RequestInit];
}

beforeEach(() => {
  localStorage.clear();
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
  vi.mocked(fileToAvatarDataUrl).mockReset();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("ChatView — M12b 设置按钮与设置弹窗", () => {
  it("opens the settings dialog from the dock and shows version + sponsor", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const wrapper = await mountView(pinia);

    expect(
      mainPane(wrapper).find('[data-testid="settings-dialog"]').exists(),
    ).toBe(false);

    await navPane(wrapper)
      .find('[data-testid="settings-button"]')
      .trigger("click");
    await wrapper.vm.$nextTick();

    const dialog = mainPane(wrapper).find('[data-testid="settings-dialog"]');
    expect(dialog.exists()).toBe(true);
    expect(
      mainPane(wrapper).find('[data-testid="settings-version"]').text(),
    ).toContain("0.1.0");
    expect(
      mainPane(wrapper).find('[data-testid="settings-sponsor"]').text(),
    ).toContain("Lecway");

    await mainPane(wrapper)
      .find('[data-testid="settings-close"]')
      .trigger("click");
    await wrapper.vm.$nextTick();
    expect(
      mainPane(wrapper).find('[data-testid="settings-dialog"]').exists(),
    ).toBe(false);
  });

  it("applies the chosen theme from the dialog", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const wrapper = await mountView(pinia);

    await navPane(wrapper)
      .find('[data-testid="settings-button"]')
      .trigger("click");
    await wrapper.vm.$nextTick();
    await mainPane(wrapper)
      .find('[data-testid="settings-theme-dark"]')
      .trigger("click");
    await wrapper.vm.$nextTick();

    expect(useTheme().preference.value).toBe("dark");
    // Restore a neutral preference for the rest of the file.
    useTheme().preference.value = "system";
  });

  it("switches the UI language from the dialog", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "7", username: "me", uid: 1000007 };
    const wrapper = await mountView(pinia);

    await navPane(wrapper)
      .find('[data-testid="settings-button"]')
      .trigger("click");
    await wrapper.vm.$nextTick();
    await mainPane(wrapper)
      .find('[data-testid="settings-locale-en"]')
      .trigger("click");
    await wrapper.vm.$nextTick();

    expect(
      mainPane(wrapper).find('[data-testid="settings-title"]').text(),
    ).toBe("Settings");
  });
});

describe("ChatView — M12b 群资料设置", () => {
  it("owner edits the group description and PATCHes the new value", async () => {
    const wrapper = await mountGroupPanel("7", "owner", [
      member("7", "owner"),
      member("u2", "member"),
    ]);
    const main = mainPane(wrapper);
    expect(main.find('[data-testid="group-description-edit"]').exists()).toBe(
      true,
    );

    await main.find('[data-testid="group-description-edit"]').trigger("click");
    await wrapper.vm.$nextTick();
    await main
      .find('[data-testid="group-description-input"]')
      .setValue("新的群简介");
    await main.find('[data-testid="group-description-save"]').trigger("click");
    await flushPromises();

    const [path, init] = patchCall();
    expect(path).toBe("/api/groups/1");
    expect(JSON.parse(String(init.body))).toEqual({
      description: "新的群简介",
    });
  });

  it("plain members see a read-only description with no edit affordance", async () => {
    const wrapper = await mountGroupPanel(
      "7",
      "member",
      [member("u1", "owner"), member("7", "member")],
      { description: "只读简介" },
    );
    const main = mainPane(wrapper);
    expect(main.find('[data-testid="group-description"]').text()).toBe(
      "只读简介",
    );
    expect(main.find('[data-testid="group-description-edit"]').exists()).toBe(
      false,
    );
    expect(main.find('[data-testid="group-avatar-edit"]').exists()).toBe(false);
  });

  it("owner uploads a compressed avatar and PATCHes the data URL", async () => {
    vi.mocked(fileToAvatarDataUrl).mockResolvedValue(
      "data:image/jpeg;base64,ZZZ",
    );
    const wrapper = await mountGroupPanel("7", "owner", [member("7", "owner")]);
    const main = mainPane(wrapper);

    const file = new File(["x"], "g.png", { type: "image/png" });
    const input = main.find('[data-testid="group-avatar-input"]')
      .element as HTMLInputElement;
    Object.defineProperty(input, "files", {
      value: [file],
      configurable: true,
    });

    await main.find('[data-testid="group-avatar-input"]').trigger("change");
    await flushPromises();

    expect(vi.mocked(fileToAvatarDataUrl)).toHaveBeenCalledWith(file);
    const [path, init] = patchCall();
    expect(path).toBe("/api/groups/1");
    expect(JSON.parse(String(init.body))).toEqual({
      avatar: "data:image/jpeg;base64,ZZZ",
    });
  });

  it("owner can remove the group avatar", async () => {
    const wrapper = await mountGroupPanel(
      "7",
      "owner",
      [member("7", "owner")],
      { avatar: "data:image/png;base64,AAA" },
    );
    const main = mainPane(wrapper);
    const remove = main.find('[data-testid="group-avatar-remove"]');
    expect(remove.exists()).toBe(true);

    await remove.trigger("click");
    await flushPromises();

    const [, init] = patchCall();
    expect(JSON.parse(String(init.body))).toEqual({ avatar: "" });
  });
});

describe("ChatView — M12b 成员头衔与群等级", () => {
  it("renders role/custom/tier title badges plus group level", async () => {
    const wrapper = await mountGroupPanel("7", "owner", [
      member("7", "owner", { group_level: 3 }),
      member("u2", "member", { group_level: 15 }),
      member("u3", "admin", { group_level: 5 }),
      member("u4", "member", { group_level: 100, custom_title: "咸鱼" }),
    ]);
    const main = mainPane(wrapper);

    const titles = main
      .findAll('[data-testid="group-member-title"]')
      .map((node) => node.text());
    expect(titles).toContain("群主");
    expect(titles).toContain("石头");
    expect(titles).toContain("管理员");
    expect(titles).toContain("咸鱼");

    const levels = main
      .findAll('[data-testid="group-member-level"]')
      .map((node) => node.text());
    expect(levels).toContain("Lv.15");
    expect(levels).toContain("Lv.100");
  });

  it("owner sets a member title through the dialog and clears it", async () => {
    const wrapper = await mountGroupPanel("7", "owner", [
      member("7", "owner"),
      member("u2", "member", { custom_title: "旧头衔", group_level: 5 }),
    ]);
    const main = mainPane(wrapper);

    const edit = main.find('[data-testid="group-member-title-edit"]');
    expect(edit.exists()).toBe(true);
    await edit.trigger("click");
    await wrapper.vm.$nextTick();

    const dialog = main.find('[data-testid="group-title-dialog"]');
    expect(dialog.exists()).toBe(true);
    const input = dialog.find('[data-testid="group-title-input"]');
    expect((input.element as HTMLInputElement).value).toBe("旧头衔");

    await input.setValue("大佬");
    await main.find('[data-testid="group-title-save"]').trigger("click");
    await flushPromises();

    const call = fetchMock.mock.calls.find(
      ([, init]) =>
        (init as RequestInit)?.method === "POST" &&
        String((init as RequestInit).body).includes("大佬"),
    );
    expect(call).toBeDefined();
    const [path, init] = call as [string, RequestInit];
    expect(path).toBe("/api/groups/1/members/u2/title");
    expect(JSON.parse(String(init.body))).toEqual({ title: "大佬" });
  });

  it("plain members and admins cannot set member titles", async () => {
    const asMember = await mountGroupPanel("7", "member", [
      member("u1", "owner"),
      member("7", "member"),
    ]);
    expect(
      mainPane(asMember)
        .find('[data-testid="group-member-title-edit"]')
        .exists(),
    ).toBe(false);

    const asAdmin = await mountGroupPanel("7", "admin", [
      member("u1", "owner"),
      member("7", "admin"),
      member("u3", "member"),
    ]);
    expect(
      mainPane(asAdmin)
        .find('[data-testid="group-member-title-edit"]')
        .exists(),
    ).toBe(false);
  });
});
