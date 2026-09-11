import { describe, expect, it } from "vitest";
import type { GroupMember } from "./api/groups";
import {
  canChangeRole,
  canInviteMembers,
  canKickMember,
  canLeaveGroup,
  canManageGroup,
  canTransferOwnership,
  myMembership,
  roleLabelKey,
} from "./groupRoles";

function member(
  user_id: string,
  role: GroupMember["role"],
  username = user_id,
): GroupMember {
  return {
    user_id,
    username,
    role,
    joined_at: "2026-01-01T00:00:00Z",
  };
}

const roster: GroupMember[] = [
  member("me-owner", "owner"),
  member("u-admin", "admin"),
  member("u-member", "member"),
];

describe("groupRoles — 角色权限矩阵", () => {
  it("owner 可踢除管理员与成员，但不能踢自己", () => {
    expect(canKickMember(roster, "me-owner", "u-admin")).toBe(true);
    expect(canKickMember(roster, "me-owner", "u-member")).toBe(true);
    expect(canKickMember(roster, "me-owner", "me-owner")).toBe(false);
  });

  it("admin 只能踢普通成员，不能踢其他管理员或群主", () => {
    expect(canKickMember(roster, "u-admin", "u-member")).toBe(true);
    expect(canKickMember(roster, "u-admin", "u-admin")).toBe(false);
    expect(canKickMember(roster, "u-admin", "me-owner")).toBe(false);
  });

  it("普通成员没有任何管理权限", () => {
    expect(canKickMember(roster, "u-member", "u-admin")).toBe(false);
    expect(canKickMember(roster, "u-member", "me-owner")).toBe(false);
    expect(canManageGroup(roster, "u-member")).toBe(false);
    expect(canInviteMembers(roster, "u-member")).toBe(false);
  });

  it("仅群主可任命/罢免管理员与转让群主", () => {
    expect(canChangeRole(roster, "me-owner", "u-member")).toBe(true);
    expect(canChangeRole(roster, "me-owner", "u-admin")).toBe(true);
    expect(canChangeRole(roster, "u-admin", "u-member")).toBe(false);
    expect(canTransferOwnership(roster, "me-owner", "u-member")).toBe(true);
    expect(canTransferOwnership(roster, "u-admin", "u-member")).toBe(false);
    // Owner cannot transfer to self.
    expect(canTransferOwnership(roster, "me-owner", "me-owner")).toBe(false);
  });

  it("群主不能退出，其他成员可以", () => {
    expect(canLeaveGroup(roster, "me-owner")).toBe(false);
    expect(canLeaveGroup(roster, "u-admin")).toBe(true);
    expect(canLeaveGroup(roster, "u-member")).toBe(true);
  });

  it("非成员拿不到任何权限", () => {
    const strangers = [member("owner", "owner"), member("m", "member")];
    expect(myMembership(strangers, "nobody")).toBeNull();
    expect(canKickMember(strangers, "nobody", "m")).toBe(false);
    expect(canManageGroup(strangers, "nobody")).toBe(false);
    expect(canLeaveGroup(strangers, "nobody")).toBe(false);
  });

  it("角色标签映射到本地化 key", () => {
    expect(roleLabelKey("owner")).toBe("group.roleOwner");
    expect(roleLabelKey("admin")).toBe("group.roleAdmin");
    expect(roleLabelKey("member")).toBe("group.roleMember");
  });
});
