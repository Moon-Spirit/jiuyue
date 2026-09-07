import { beforeEach, describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import type { VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import type { Pinia } from "pinia";
import { createMemoryHistory, createRouter } from "vue-router";
import type { Router } from "vue-router";
import { i18n } from "../i18n";
import { useAuthStore } from "../stores/auth";
import { useProfileStore } from "../stores/profile";
import type { UserProfile } from "../lib/api/profile";
import ProfileView from "./ProfileView.vue";

const fetchMock = vi.fn();

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function makeProfile(overrides: Partial<UserProfile> = {}): UserProfile {
  return {
    user_id: "u-self",
    username: "alice",
    uid: 100007,
    display_name: "Alice",
    bio: "冒险家",
    avatar: "🐱",
    level: 12,
    title: "木头",
    xp: 120,
    xp_to_next: 55,
    ...overrides,
  };
}

const SELF = makeProfile();

async function mountView(pinia: Pinia, path: string): Promise<VueWrapper> {
  const router: Router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/chat", component: { template: "<div />" } },
      { path: "/profile", component: ProfileView },
      { path: "/profile/:userId", component: ProfileView },
    ],
  });
  await router.push(path);
  await router.isReady();

  return mount(ProfileView, {
    global: { plugins: [pinia, i18n, router] },
  });
}

/** Seeds the signed-in user and the profile-store self cache; stubs GET. */
function selfSession(): Pinia {
  const pinia = createPinia();
  setActivePinia(pinia);
  const auth = useAuthStore();
  auth.user = {
    userId: "u-self",
    username: "alice",
    uid: 100007,
    displayName: "Alice",
    avatar: "🐱",
    bio: "冒险家",
  };
  auth.accessToken = "test-token";
  useProfileStore().me = { ...SELF };
  return pinia;
}

beforeEach(() => {
  localStorage.clear();
  fetchMock.mockReset();
  fetchMock.mockResolvedValue(jsonResponse(200, SELF));
  vi.stubGlobal("fetch", fetchMock);
});

describe("ProfileView — self mode (/profile)", () => {
  it("renders the avatar, display name, @username, level chip and block title", async () => {
    const wrapper = await mountView(selfSession(), "/profile");

    expect(wrapper.find('[data-testid="profile-avatar-self"]').exists()).toBe(
      true,
    );
    // Avatar inner emoji (self header).
    expect(
      wrapper
        .find('[data-testid="profile-card"] [data-testid="avatar-emoji"]')
        .exists(),
    ).toBe(true);
    expect(wrapper.find('[data-testid="profile-display-name"]').text()).toBe(
      "Alice",
    );
    expect(wrapper.find('[data-testid="profile-card"]').text()).toContain(
      "@alice",
    );
    expect(wrapper.find('[data-testid="profile-level-chip"]').text()).toBe(
      "Lv.12",
    );
    // Level 12 sits in the 木头 band (6-10? no — 11-15 stone) → 石头.
    expect(wrapper.find('[data-testid="profile-title-chip"]').text()).toBe(
      "石头",
    );
    expect(wrapper.find('[data-testid="profile-bio"]').text()).toBe("冒险家");
  });

  it("shows the edit form and persists changes through a mocked PATCH", async () => {
    const wrapper = await mountView(selfSession(), "/profile");

    await wrapper.find('[data-testid="profile-edit"]').trigger("click");
    expect(wrapper.find('[data-testid="profile-edit-form"]').exists()).toBe(
      true,
    );

    // Prefilled from the profile.
    const nameInput = wrapper.find<HTMLInputElement>(
      '[data-testid="profile-name-input"]',
    );
    expect(nameInput.element.value).toBe("Alice");

    // Choose a new avatar via the picker.
    await wrapper.find('[data-testid="profile-pick-avatar"]').trigger("click");
    expect(wrapper.find('[data-testid="profile-avatar-picker"]').exists()).toBe(
      true,
    );
    const choices = wrapper.findAll('[data-testid="avatar-choice"]');
    expect(choices.length).toBeGreaterThanOrEqual(30);
    await choices.find((c) => c.text() === "💎")?.trigger("click");

    await nameInput.setValue("Alice 改");
    await wrapper
      .find('[data-testid="profile-bio-input"]')
      .setValue("新的签名");

    // Mock the PATCH response (server returns the updated profile).
    fetchMock.mockResolvedValueOnce(
      jsonResponse(200, {
        ...SELF,
        display_name: "Alice 改",
        avatar: "💎",
        bio: "新的签名",
      }),
    );

    // An untrusted synthetic click on a submit button does not run jsdom's
    // form activation behavior — submit the form explicitly.
    await wrapper.find('[data-testid="profile-edit-form"]').trigger("submit");
    await vi.waitFor(() => {
      expect(wrapper.find('[data-testid="profile-edit-form"]').exists()).toBe(
        false,
      );
    });

    // The call carried the right body and Bearer header.
    const [, init] = fetchMock.mock.calls.find(([url]) =>
      String(url).endsWith("/api/users/profile"),
    ) as [string, RequestInit];
    expect(init.method).toBe("PATCH");
    expect(JSON.parse(String(init.body))).toEqual({
      display_name: "Alice 改",
      bio: "新的签名",
      avatar: "💎",
    });
    expect((init.headers as Headers).get("Authorization")).toBe(
      "Bearer test-token",
    );

    // Local auth store mirrors the edited display fields.
    expect(useAuthStore().user?.displayName).toBe("Alice 改");
    expect(useAuthStore().user?.avatar).toBe("💎");
    expect(useProfileStore().me?.bio).toBe("新的签名");
    expect(wrapper.find('[data-testid="profile-display-name"]').text()).toBe(
      "Alice 改",
    );
  });

  it("blocks saving when the bio exceeds 200 characters", async () => {
    const wrapper = await mountView(selfSession(), "/profile");
    await wrapper.find('[data-testid="profile-edit"]').trigger("click");

    await wrapper
      .find('[data-testid="profile-bio-input"]')
      .setValue("x".repeat(201));

    const save = wrapper.find<HTMLButtonElement>(
      '[data-testid="profile-save"]',
    );
    expect(save.attributes("disabled")).toBeDefined();
    expect(wrapper.find('[data-testid="profile-bio-error"]').exists()).toBe(
      true,
    );
    expect(
      wrapper.find('[data-testid="profile-bio-counter"]').text(),
    ).toContain("201/200");
  });
});

describe("ProfileView — peer mode (/profile/:userId)", () => {
  it("renders the peer profile read-only with no save affordance", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    const auth = useAuthStore();
    auth.user = { userId: "u-self", username: "alice", uid: 100007 };
    auth.accessToken = "test-token";
    const profileStore = useProfileStore();

    const peer = makeProfile({
      user_id: "u-bob",
      username: "bob",
      uid: 200001,
      display_name: "Bob Builder",
      avatar: "💎",
      bio: "盖房子中",
      level: 44,
      title: "龙蛋",
      xp: 20,
      xp_to_next: 3000,
    });
    fetchMock.mockResolvedValue(jsonResponse(200, peer));
    profileStore.peers["u-bob"] = peer;

    const wrapper = await mountView(pinia, "/profile/u-bob");
    await vi.waitFor(() => {
      expect(wrapper.find('[data-testid="profile-display-name"]').text()).toBe(
        "Bob Builder",
      );
    });

    expect(wrapper.find('[data-testid="profile-card"]').text()).toContain(
      "@bob",
    );
    expect(wrapper.find('[data-testid="profile-level-chip"]').text()).toBe(
      "Lv.44",
    );
    // Level 44 → dragon-egg band.
    expect(wrapper.find('[data-testid="profile-title-chip"]').text()).toBe(
      "龙蛋",
    );
    expect(wrapper.find('[data-testid="profile-bio"]').text()).toBe("盖房子中");

    // Read-only: no edit button, no edit form, no self-avatar button.
    expect(wrapper.find('[data-testid="profile-edit"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="profile-edit-form"]').exists()).toBe(
      false,
    );
    expect(wrapper.find('[data-testid="profile-avatar-self"]').exists()).toBe(
      false,
    );
    expect(
      wrapper
        .find('[data-testid="profile-card"] [data-testid="avatar-emoji"]')
        .text(),
    ).toBe("💎");
  });

  it("fetches the peer profile from the server on entry", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    const auth = useAuthStore();
    auth.user = { userId: "u-self", username: "alice", uid: 100007 };
    auth.accessToken = "test-token";

    const peer = makeProfile({
      user_id: "u-bob",
      username: "bob",
      uid: 200001,
      display_name: "Bob",
      bio: "",
      avatar: null,
      level: 3,
      title: "土块",
      xp: 10,
      xp_to_next: 50,
    });
    fetchMock.mockResolvedValue(jsonResponse(200, peer));

    await mountView(pinia, "/profile/u-bob");
    await vi.waitFor(() => {
      expect(useProfileStore().peers["u-bob"]?.display_name).toBe("Bob");
    });

    const [path] = fetchMock.mock.calls[0] as [string];
    expect(path).toBe("/api/users/profile/u-bob");
  });
});
