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
import ChatView from "./ChatView.vue";

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

function mainPane(wrapper: VueWrapper): DOMWrapper<Element> {
  return wrapper.find('[data-testid="shell-main-desktop"]');
}

interface SeedFile {
  file_id: string;
  name: string;
  mime: string;
  bytes: number;
  uploader: { user_id: string; username: string; display_name?: string };
  created_at: string;
  expires_at?: string | null;
}

function groupFile(overrides: Partial<SeedFile> = {}): SeedFile {
  return {
    file_id: "f1",
    name: "报告.pdf",
    mime: "application/pdf",
    bytes: 1536,
    uploader: { user_id: "u2", username: "alice", display_name: "Alice" },
    created_at: "2026-09-01T00:00:00Z",
    expires_at: null,
    ...overrides,
  };
}

const GROUP_INFO = {
  conversation_id: 1,
  name: "团队",
  description: "",
  avatar: null,
  my_role: "owner" as const,
  member_count: 2,
  members: [
    {
      user_id: "7",
      username: "me",
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

const QUOTA = 1024 * 1024 * 1024;

function listing(files: SeedFile[], usageBytes = 0) {
  return { usage_bytes: usageBytes, quota_bytes: QUOTA, files };
}

/** Minimal XHR double: a send immediately "uploads" the file (201). */
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
      file_id: "fid-new",
      name: "new.bin",
      mime: "application/octet-stream",
      bytes: 3,
      created_at: "2026-09-01T00:00:00Z",
      expires_at: null,
    });
    this.onload?.();
  }
}

function installFetch(info: unknown, files: SeedFile[], usageBytes: number) {
  fetchMock.mockImplementation(
    async (path: string, init?: RequestInit): Promise<Response> => {
      const method = init?.method ?? "GET";
      if (path === "/api/groups/1/files" && method === "GET") {
        return jsonResponse(200, listing(files, usageBytes));
      }
      if (path.startsWith("/api/groups/files/") && method === "POST") {
        return new Response(null, { status: 204 });
      }
      return jsonResponse(200, info);
    },
  );
}

/** Seeds a group thread, opens the drawer, loads the files listing. */
async function mountGroupFiles(
  myId: string,
  myRole: "owner" | "admin" | "member",
  files: SeedFile[],
  usageBytes = 0,
): Promise<VueWrapper> {
  const info = { ...GROUP_INFO, my_role: myRole };
  installFetch(info, files, usageBytes);
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
  await flushPromises();
  return wrapper;
}

beforeEach(() => {
  localStorage.clear();
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
  MockUploadXHR.instances = [];
  vi.stubGlobal("XMLHttpRequest", MockUploadXHR);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("ChatView — M13b 群文件", () => {
  it("renders the file list with name, size, uploader, usage and expiry badge", async () => {
    const wrapper = await mountGroupFiles("7", "owner", [
      groupFile({ expires_at: "2026-09-08T00:00:00Z" }),
    ]);
    const main = mainPane(wrapper);

    expect(main.find('[data-testid="group-file-title"]').text()).toBe("群文件");
    expect(main.findAll('[data-testid="group-file"]')).toHaveLength(1);
    expect(main.find('[data-testid="group-file-name"]').text()).toBe(
      "报告.pdf",
    );
    expect(main.find('[data-testid="group-file-size"]').text()).toBe("1.5 KB");
    expect(main.find('[data-testid="group-file-uploader"]').text()).toBe(
      "Alice",
    );
    expect(main.find('[data-testid="group-file-expiry"]').text()).toBe(
      "7天有效",
    );
    expect(main.find('[data-testid="group-file-usage"]').text()).toContain(
      "已用 0 B / 1.0 GB",
    );
  });

  it("shows the over-quota hint once usage reaches the quota", async () => {
    const wrapper = await mountGroupFiles("7", "owner", [groupFile()], QUOTA);
    expect(
      mainPane(wrapper).find('[data-testid="group-file-over-quota"]').exists(),
    ).toBe(true);
  });

  it("deletes a file through the confirmation overlay", async () => {
    const wrapper = await mountGroupFiles("7", "owner", [groupFile()]);
    const main = mainPane(wrapper);

    await main.find('[data-testid="group-file-delete"]').trigger("click");
    await wrapper.vm.$nextTick();
    const confirm = main.find('[data-testid="group-file-delete-confirm"]');
    expect(confirm.exists()).toBe(true);
    expect(confirm.text()).toContain("报告.pdf");

    await main
      .find('[data-testid="group-file-delete-accept"]')
      .trigger("click");
    await flushPromises();

    const deleteCall = fetchMock.mock.calls.find(
      ([path, init]) =>
        (init as RequestInit)?.method === "POST" &&
        path === "/api/groups/files/f1/delete",
    );
    expect(deleteCall).toBeDefined();
    // The listing is refreshed after the delete.
    expect(
      fetchMock.mock.calls.some(
        ([path, init]) =>
          (init as RequestInit)?.method === "GET" &&
          path === "/api/groups/1/files",
      ),
    ).toBe(true);
  });

  it("uploads dropped files sequentially and highlights the drop zone", async () => {
    const wrapper = await mountGroupFiles("7", "owner", []);
    const main = mainPane(wrapper);
    const dropzone = main.find('[data-testid="group-file-dropzone"]');
    expect(dropzone.exists()).toBe(true);

    await dropzone.trigger("dragenter");
    await wrapper.vm.$nextTick();
    expect(main.find('[data-testid="group-file-drop-hint"]').exists()).toBe(
      true,
    );

    const fileA = new File(["a"], "a.txt", { type: "text/plain" });
    const fileB = new File(["bb"], "b.txt", { type: "text/plain" });
    await dropzone.trigger("drop", {
      dataTransfer: { files: [fileA, fileB] },
    });
    await flushPromises();

    expect(MockUploadXHR.instances).toHaveLength(2);
    expect(MockUploadXHR.instances[0]?.url).toBe("/api/groups/1/files");
    expect(MockUploadXHR.instances[0]?.sentBody).toBe(fileA);
    expect(MockUploadXHR.instances[1]?.sentBody).toBe(fileB);
    // Highlight cleared once the drop is handled.
    expect(main.find('[data-testid="group-file-drop-hint"]').exists()).toBe(
      false,
    );
  });

  it("owner can delete another member's file", async () => {
    const wrapper = await mountGroupFiles("7", "owner", [
      groupFile({
        file_id: "f-other",
        uploader: { user_id: "u2", username: "alice" },
      }),
    ]);
    expect(
      mainPane(wrapper).findAll('[data-testid="group-file-delete"]'),
    ).toHaveLength(1);
  });

  it("plain member only sees delete on their own file", async () => {
    const wrapper = await mountGroupFiles("7", "member", [
      groupFile({
        file_id: "f-mine",
        uploader: { user_id: "7", username: "me" },
      }),
      groupFile({
        file_id: "f-other",
        uploader: { user_id: "u2", username: "alice" },
      }),
    ]);
    const deletes = mainPane(wrapper).findAll(
      '[data-testid="group-file-delete"]',
    );
    expect(deletes).toHaveLength(1);
    expect(
      deletes[0]?.element
        .closest("[data-file-id]")
        ?.getAttribute("data-file-id"),
    ).toBe("f-mine");
  });
});
