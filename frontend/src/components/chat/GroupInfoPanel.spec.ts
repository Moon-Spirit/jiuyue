import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import type { ConversationSummary } from "../../generated/ConversationSummary";
import type { MemberView } from "../../generated/MemberView";
import type { Role } from "../../generated/Role";
import GroupInfoPanel from "./GroupInfoPanel.vue";

const SELF_ID = "01JABC1234567890ABCDEFGHJ1";
const BOB_ID = "01JABC1234567890ABCDEFGHJ3";
const CAROL_ID = "01JABC1234567890ABCDEFGHJ4";
const GROUP_ID = "01JABC1234567890ABCDEFGHJ2";

function conversation(myRole: Role, memberCount: number): ConversationSummary {
  return {
    id: GROUP_ID,
    kind: "group",
    peer: null,
    group: { title: "九月小组", member_count: memberCount, my_role: myRole },
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

function mountPanel(
  myRole: Role,
  members: MemberView[],
  currentUserId = SELF_ID,
) {
  return mount(GroupInfoPanel, {
    props: {
      conversation: conversation(myRole, members.length),
      members,
      currentUserId,
      loading: false,
    },
  });
}

describe("GroupInfoPanel", () => {
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
});
