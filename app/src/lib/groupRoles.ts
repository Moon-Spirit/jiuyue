import type { GroupMember, GroupRole } from "./api/groups";

/**
 * Pure role-gating rules for the group info panel. Keeping these out of the
 * view makes the permission matrix unit-testable and impossible to drift:
 *
 * - owner: kick anyone but self, appoint/demote admins, transfer ownership;
 * - admin: kick plain members only (never peers or the owner);
 * - member: no management actions at all;
 * - owner: cannot leave (must transfer first).
 */

export function myMembership(
  members: GroupMember[],
  myId: string,
): GroupMember | null {
  return members.find((m) => m.user_id === myId) ?? null;
}

/** Broadest management gate: owner or admin (invites). */
export function canManageGroup(members: GroupMember[], myId: string): boolean {
  const me = myMembership(members, myId);
  return me !== null && (me.role === "owner" || me.role === "admin");
}

/**
 * owner → any member except self/owner; admin → plain members only; anyone
 * else → never.
 */
export function canKickMember(
  members: GroupMember[],
  myId: string,
  targetUserId: string,
): boolean {
  if (targetUserId === myId) return false;
  const me = myMembership(members, myId);
  const target = members.find((m) => m.user_id === targetUserId);
  if (me === null || target === undefined) return false;
  if (me.role === "owner") return target.role !== "owner";
  if (me.role === "admin") return target.role === "member";
  return false;
}

/** Owner-only: appoint an admin or demote an admin back to member. */
export function canChangeRole(
  members: GroupMember[],
  myId: string,
  targetUserId: string,
): boolean {
  if (targetUserId === myId) return false;
  const me = myMembership(members, myId);
  const target = members.find((m) => m.user_id === targetUserId);
  if (me === null || target === undefined) return false;
  return me.role === "owner" && target.role !== "owner";
}

/** Owner-only: hand the crown to another member. */
export function canTransferOwnership(
  members: GroupMember[],
  myId: string,
  targetUserId: string,
): boolean {
  if (targetUserId === myId) return false;
  const me = myMembership(members, myId);
  const target = members.find((m) => m.user_id === targetUserId);
  if (me === null || target === undefined) return false;
  return me.role === "owner";
}

/**
 * Owner-only: assign a custom title to another member. The owner's own title
 * is fixed (群主) and can never be overwritten, so self is excluded.
 */
export function canSetMemberTitle(
  members: GroupMember[],
  myId: string,
  targetUserId: string,
): boolean {
  if (targetUserId === myId) return false;
  const me = myMembership(members, myId);
  const target = members.find((m) => m.user_id === targetUserId);
  if (me === null || target === undefined) return false;
  return me.role === "owner" && target.role !== "owner";
}

/** Owner and admins can invite new members. */
export function canInviteMembers(
  members: GroupMember[],
  myId: string,
): boolean {
  return canManageGroup(members, myId);
}

/** Everyone but the owner may leave; the owner must transfer first. */
export function canLeaveGroup(members: GroupMember[], myId: string): boolean {
  const me = myMembership(members, myId);
  return me !== null && me.role !== "owner";
}

/** Localized label key for a role (resolved by callers through i18n). */
export function roleLabelKey(role: GroupRole): string {
  if (role === "owner") return "group.roleOwner";
  if (role === "admin") return "group.roleAdmin";
  return "group.roleMember";
}
