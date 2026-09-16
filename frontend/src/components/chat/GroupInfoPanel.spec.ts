import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ConversationSummary } from "../../generated/ConversationSummary";
import type { GroupInfo } from "../../generated/GroupInfo";
import type { MemberView } from "../../generated/MemberView";
import type { Role } from "../../generated/Role";
import {
  MAX_ANNOUNCEMENT_CHARS,
  may,
  mayLeave,
  mayRemove,
} from "../../stores/groups";
import GroupInfoPanel from "./GroupInfoPanel.vue";

const SELF_ID = "01JABC1234567890ABCDEFGHJ1";
const BOB_ID = "01JABC1234567890ABCDEFGHJ3";
const CAROL_ID = "01JABC1234567890ABCDEFGHJ4";
const GROUP_ID = "01JABC1234567890ABCDEFGHJ2";
const ACCESS_KEY = "jiuyue.auth.access_token";

function conversation(
  myRole: Role,
  memberCount: number,
  announcement: string | null = null,
): ConversationSummary {
  return {
    id: GROUP_ID,
    kind: "group",
    peer: null,
    group: {
      title: "九月小组",
      member_count: memberCount,
      my_role: myRole,
      announcement,
    },
    unread_count: 0,
    created_at_ms: 1_700_000_000_000,
  };
}

function member(
  user_id: string,
  username: string,
  role: Role,
  display_name = username,
): MemberView {
  return {
    user_id,
    username,
    display_name,
    avatar_url: null,
    role,
    joined_at_ms: 1_700_000_000_000,
  };
}

function owners(): MemberView[] {
  return [
    member(SELF_ID, "alice", "owner", "Alice"),
    member(BOB_ID, "bob", "admin", "Bob"),
    member(CAROL_ID, "carol", "member", "Carol"),
  ];
}

function groupInfo(
  myRole: Role,
  members: MemberView[],
  announcement: string | null,
): GroupInfo {
  return {
    conversation: conversation(myRole, members.length, announcement),
    members,
  };
}

function mountPanel(
  myRole: Role,
  members: MemberView[],
  currentUserId = SELF_ID,
  announcement: string | null = null,
) {
  return mount(GroupInfoPanel, {
    props: {
      conversation: conversation(myRole, members.length, announcement),
      members,
      currentUserId,
      loading: false,
    },
  });
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

/** One recorded request, so a test can assert what actually left the browser. */
type RecordedCall = {
  readonly url: string;
  readonly method: string;
  readonly body: string | null;
};

/**
 * Substitute `fetch` with a responder per URL, recording every call.
 *
 * The HTTP boundary is the single seam here; the store, the request shape and the
 * panel are all real.
 */
function stubFetch(
  routes: Record<string, () => Response>,
): readonly RecordedCall[] {
  const calls: RecordedCall[] = [];

  vi.stubGlobal(
    "fetch",
    vi.fn<typeof fetch>(async (input, init) => {
      const url = String(input);
      calls.push({
        url,
        method: init?.method ?? "GET",
        body: typeof init?.body === "string" ? init.body : null,
      });

      const responder = routes[url];
      if (responder === undefined)
        throw new Error(`unexpected request: ${url}`);
      return responder();
    }),
  );

  return calls;
}

function signIn(): void {
  window.localStorage.setItem(ACCESS_KEY, "test-access");
}

describe("GroupInfoPanel", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    signIn();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    window.localStorage.clear();
  });

  it("renders every member with their role label", () => {
    const wrapper = mountPanel("owner", owners());

    expect(wrapper.findAll('[data-test="member-row"]')).toHaveLength(3);
    expect(
      wrapper.findAll('[data-test="role-badge"]').map((badge) => badge.text()),
    ).toEqual(["群主", "管理员", "成员"]);
  });

  it("offers the owner every governed action", () => {
    const wrapper = mountPanel("owner", owners());

    expect(wrapper.find('[data-test="invite-form"]').exists()).toBe(true);
    expect(wrapper.find('[data-test="dissolve-group"]').exists()).toBe(true);
    // Alice may govern Bob (admin) and Carol (member): two toggle buttons.
    expect(wrapper.findAll('[data-test="toggle-admin"]')).toHaveLength(2);
    expect(wrapper.findAll('[data-test="remove-member"]')).toHaveLength(2);
    expect(wrapper.findAll('[data-test="transfer-owner"]')).toHaveLength(2);
  });

  it("tells an owner with members present to transfer or dissolve instead of leaving", () => {
    const wrapper = mountPanel("owner", owners());

    expect(wrapper.find('[data-test="leave-group"]').exists()).toBe(false);
    expect(wrapper.find('[data-test="owner-leave-hint"]').exists()).toBe(true);
  });

  it("offers a member only the actions a member has", () => {
    const wrapper = mountPanel("member", owners(), CAROL_ID);

    expect(wrapper.find('[data-test="invite-form"]').exists()).toBe(false);
    expect(wrapper.find('[data-test="dissolve-group"]').exists()).toBe(false);
    expect(wrapper.find('[data-test="toggle-admin"]').exists()).toBe(false);
    expect(wrapper.find('[data-test="remove-member"]').exists()).toBe(false);
    expect(wrapper.find('[data-test="transfer-owner"]').exists()).toBe(false);
    expect(wrapper.find('[data-test="leave-group"]').exists()).toBe(true);
  });

  it("lets an admin remove an ordinary member but not the owner or a peer admin", () => {
    // The caller is Bob, an admin; Alice owns, Carol is an ordinary member.
    const wrapper = mountPanel("admin", owners(), BOB_ID);

    // Bob may not remove himself, the owner, or a peer admin: only Carol.
    expect(wrapper.findAll('[data-test="remove-member"]')).toHaveLength(1);
    expect(wrapper.findAll('[data-test="transfer-owner"]')).toHaveLength(0);
    expect(wrapper.find('[data-test="invite-form"]').exists()).toBe(true);
  });

  it("emits remove with the member's id", async () => {
    const wrapper = mountPanel("owner", owners());

    // Both removable members are Bob (admin) and Carol (member).
    await wrapper.findAll('[data-test="remove-member"]')[1]?.trigger("click");

    expect(wrapper.emitted("remove")?.[0]).toEqual([CAROL_ID]);
  });

  it("emits the role toggle for a member", async () => {
    const wrapper = mountPanel("owner", owners());

    // Bob is currently an admin, so the button demotes him.
    await wrapper.findAll('[data-test="toggle-admin"]')[0]?.trigger("click");

    expect(wrapper.emitted("setRole")?.[0]).toEqual([BOB_ID, "member"]);
  });

  it("emits transfer with the target id", async () => {
    const wrapper = mountPanel("owner", owners());

    await wrapper.findAll('[data-test="transfer-owner"]')[0]?.trigger("click");

    expect(wrapper.emitted("transfer")?.[0]).toEqual([BOB_ID]);
  });

  it("emits an invite with the split usernames", async () => {
    const wrapper = mountPanel("owner", owners());

    await wrapper.get('[data-test="invite-input"]').setValue("dave, erin");
    await wrapper.get('[data-test="invite-form"]').trigger("submit");

    expect(wrapper.emitted("invite")?.[0]).toEqual([["dave", "erin"]]);
  });

  it("emits leave for a member", async () => {
    const wrapper = mountPanel("member", owners(), CAROL_ID);

    await wrapper.get('[data-test="leave-group"]').trigger("click");

    expect(wrapper.emitted("leave")).toHaveLength(1);
  });

  it("mirrors the server's permission table for every capability", () => {
    // Owner: every capability.
    for (const capability of [
      "viewMembers",
      "postMessage",
      "inviteMembers",
      "removeMembers",
      "changeRoles",
      "transferOwnership",
      "dissolve",
      "editGroupInfo",
      "leave",
    ] as const) {
      expect(may("owner", capability)).toBe(true);
    }

    // Admin: may invite and edit the group's profile, but not govern it.
    expect(may("admin", "inviteMembers")).toBe(true);
    expect(may("admin", "editGroupInfo")).toBe(true);
    expect(may("admin", "changeRoles")).toBe(false);
    expect(may("admin", "transferOwnership")).toBe(false);
    expect(may("admin", "dissolve")).toBe(false);

    // Member: may view, post and leave — and nothing else.
    expect(may("member", "viewMembers")).toBe(true);
    expect(may("member", "postMessage")).toBe(true);
    expect(may("member", "leave")).toBe(true);
    expect(may("member", "inviteMembers")).toBe(false);
    expect(may("member", "editGroupInfo")).toBe(false);

    // The two refinements a Role alone cannot answer.
    expect(mayRemove("admin", "member")).toBe(true);
    expect(mayRemove("admin", "admin")).toBe(false);
    expect(mayRemove("admin", "owner")).toBe(false);
    expect(mayRemove("owner", "admin")).toBe(true);
    expect(mayRemove("owner", "owner")).toBe(false);
    expect(mayLeave("owner", 2)).toBe(false);
    expect(mayLeave("owner", 0)).toBe(true);
    expect(mayLeave("member", 3)).toBe(true);
  });

  it("shows the announcement, or a placeholder when there is none", () => {
    const withAnnouncement = mountPanel(
      "owner",
      owners(),
      SELF_ID,
      "周六下午三点线上会议",
    );
    expect(withAnnouncement.get('[data-test="announcement-text"]').text()).toBe(
      "周六下午三点线上会议",
    );
    expect(
      withAnnouncement.find('[data-test="announcement-empty"]').exists(),
    ).toBe(false);

    const without = mountPanel("member", owners(), CAROL_ID);
    expect(without.get('[data-test="announcement-empty"]').text()).toContain(
      "暂无群公告",
    );
    expect(without.find('[data-test="announcement-text"]').exists()).toBe(
      false,
    );
  });

  it("offers the announcement editor only to an owner or admin", () => {
    expect(
      mountPanel("owner", owners())
        .find('[data-test="announcement-edit"]')
        .exists(),
    ).toBe(true);
    expect(
      mountPanel("admin", owners(), BOB_ID)
        .find('[data-test="announcement-edit"]')
        .exists(),
    ).toBe(true);
    expect(
      mountPanel("member", owners(), CAROL_ID)
        .find('[data-test="announcement-edit"]')
        .exists(),
    ).toBe(false);
  });

  it("saves the announcement, refreshes the group, and closes the editor", async () => {
    const updated = groupInfo("owner", owners(), "周六下午三点线上会议");
    const calls = stubFetch({
      [`/api/conversations/${GROUP_ID}/announcement`]: () =>
        jsonResponse(updated),
      [`/api/conversations/${GROUP_ID}`]: () => jsonResponse(updated),
    });

    const wrapper = mountPanel("owner", owners());
    await wrapper.get('[data-test="announcement-edit"]').trigger("click");

    const input = wrapper.get('[data-test="announcement-input"]');
    await input.setValue("周六下午三点线上会议");
    expect(wrapper.get('[data-test="announcement-count"]').text()).toContain(
      `10 / ${MAX_ANNOUNCEMENT_CHARS}`,
    );

    await wrapper.get('[data-test="announcement-save"]').trigger("click");
    await flushPromises();

    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.url).toBe(`/api/conversations/${GROUP_ID}/announcement`);
    expect(JSON.parse(patch?.body ?? "")).toEqual({
      announcement: "周六下午三点线上会议",
    });
    // The group is re-read so the panel shows the stored text, not the draft.
    expect(calls.some((call) => call.method === "GET")).toBe(true);
    // The editor is closed on success.
    expect(wrapper.find('[data-test="announcement-input"]').exists()).toBe(
      false,
    );
  });

  it("clears the announcement by saving a blank draft", async () => {
    const cleared = groupInfo("owner", owners(), null);
    const calls = stubFetch({
      [`/api/conversations/${GROUP_ID}/announcement`]: () =>
        jsonResponse(cleared),
      [`/api/conversations/${GROUP_ID}`]: () => jsonResponse(cleared),
    });

    const wrapper = mountPanel("owner", owners(), SELF_ID, "旧公告");
    await wrapper.get('[data-test="announcement-edit"]').trigger("click");
    await wrapper.get('[data-test="announcement-input"]').setValue("");
    await wrapper.get('[data-test="announcement-save"]').trigger("click");
    await flushPromises();

    const patch = calls.find((call) => call.method === "PATCH");
    expect(JSON.parse(patch?.body ?? "")).toEqual({ announcement: null });
  });

  it("refuses an over-length announcement without calling the server", async () => {
    const calls = stubFetch({});

    const wrapper = mountPanel("owner", owners());
    await wrapper.get('[data-test="announcement-edit"]').trigger("click");
    await wrapper
      .get('[data-test="announcement-input"]')
      .setValue("九".repeat(MAX_ANNOUNCEMENT_CHARS + 1));

    expect(wrapper.get('[data-test="announcement-error"]').text()).toContain(
      String(MAX_ANNOUNCEMENT_CHARS),
    );
    expect(
      wrapper.get('[data-test="announcement-save"]').attributes("disabled"),
    ).toBeDefined();
    expect(calls).toHaveLength(0);
  });

  it("shows the server's message when an edit is refused", async () => {
    const refusal = {
      error: { code: "FORBIDDEN", message: "你的权限不允许此操作", fields: [] },
    };
    stubFetch({
      [`/api/conversations/${GROUP_ID}/announcement`]: () =>
        jsonResponse(refusal, 403),
    });

    const wrapper = mountPanel("owner", owners());
    await wrapper.get('[data-test="announcement-edit"]').trigger("click");
    await wrapper
      .get('[data-test="announcement-input"]')
      .setValue("不该被接受");
    await wrapper.get('[data-test="announcement-save"]').trigger("click");
    await flushPromises();

    expect(wrapper.get('[data-test="announcement-error"]').text()).toBe(
      "你的权限不允许此操作",
    );
    // A failed save keeps the editor open so the draft is not lost.
    expect(wrapper.find('[data-test="announcement-input"]').exists()).toBe(
      true,
    );
  });
});
