import { apiRequest, ApiError } from "./client";
import { apiErrorMessage } from "./messages";

type Translate = (key: string) => string;

/** A member's role inside a group conversation. */
export type GroupRole = "owner" | "admin" | "member";

/** One member row from `GET /api/groups/{id}`. */
export interface GroupMember {
  user_id: string;
  username: string;
  /** Curated label; additive field (older servers omit/empty it). */
  display_name?: string;
  /** Curated emoji avatar; additive field (older servers omit it). */
  avatar?: string | null;
  role: GroupRole;
  /** RFC 3339 timestamp, passed through verbatim. */
  joined_at: string;
}

/** POST /api/groups → 201 { conversation_id, name, member_count, invited } */
export interface CreateGroupResult {
  conversation_id: number;
  name: string;
  member_count: number;
  invited: string[];
}

/** GET /api/groups/{id} → authoritative group info + member roster. */
export interface GroupInfo {
  conversation_id: number;
  name: string;
  my_role: GroupRole;
  member_count: number;
  members: GroupMember[];
}

/** The inviter reference carried by a pending group invite. */
export interface GroupInviteFrom {
  user_id: string;
  username: string;
  /** Curated label; additive field (older servers omit it). */
  display_name?: string;
}

/** GET /api/groups/invites → pending invite addressed to me. */
export interface GroupInvite {
  invite_id: string;
  conversation_id: number;
  group_name: string;
  from: GroupInviteFrom;
}

/** POST /api/groups/invites/{id}/accept → { conversation_id } */
export interface AcceptGroupInviteResult {
  conversation_id: number;
}

/**
 * POST /api/groups { name, invite_usernames? } (Bearer access).
 * `invite_usernames` is omitted when empty so a bare create stays clean.
 */
export function createGroup(
  accessToken: string,
  name: string,
  inviteUsernames: string[] = [],
): Promise<CreateGroupResult> {
  return apiRequest<CreateGroupResult>("/api/groups", {
    method: "POST",
    body:
      inviteUsernames.length > 0
        ? { name, invite_usernames: inviteUsernames }
        : { name },
    accessToken,
  });
}

/** GET /api/groups/{id} (Bearer access) — group info + members. */
export function getGroup(
  accessToken: string,
  conversationId: number,
): Promise<GroupInfo> {
  return apiRequest<GroupInfo>(
    `/api/groups/${encodeURIComponent(String(conversationId))}`,
    { method: "GET", accessToken },
  );
}

/** POST /api/groups/{id}/invites { username } (owner/admin) → 201. */
export function inviteToGroup(
  accessToken: string,
  conversationId: number,
  username: string,
): Promise<{ invite_id: string }> {
  return apiRequest<{ invite_id: string }>(
    `/api/groups/${encodeURIComponent(String(conversationId))}/invites`,
    { method: "POST", body: { username }, accessToken },
  );
}

/** GET /api/groups/invites (Bearer access) — my pending group invites. */
export function listGroupInvites(accessToken: string): Promise<GroupInvite[]> {
  return apiRequest<GroupInvite[]>("/api/groups/invites", {
    method: "GET",
    accessToken,
  });
}

/** POST /api/groups/invites/{id}/accept → 200 { conversation_id }. */
export function acceptGroupInvite(
  accessToken: string,
  inviteId: string,
): Promise<AcceptGroupInviteResult> {
  return apiRequest<AcceptGroupInviteResult>(
    `/api/groups/invites/${encodeURIComponent(inviteId)}/accept`,
    { method: "POST", accessToken },
  );
}

/** POST /api/groups/invites/{id}/decline → 204. */
export function declineGroupInvite(
  accessToken: string,
  inviteId: string,
): Promise<void> {
  return apiRequest<void>(
    `/api/groups/invites/${encodeURIComponent(inviteId)}/decline`,
    { method: "POST", accessToken },
  );
}

/** POST /api/groups/{id}/members/{userId}/kick (owner/admin) → 204. */
export function kickGroupMember(
  accessToken: string,
  conversationId: number,
  userId: string,
): Promise<void> {
  return apiRequest<void>(
    `/api/groups/${encodeURIComponent(String(conversationId))}/members/${encodeURIComponent(userId)}/kick`,
    { method: "POST", accessToken },
  );
}

/** POST /api/groups/{id}/members/{userId}/role { role } (owner) → 200. */
export function setGroupMemberRole(
  accessToken: string,
  conversationId: number,
  userId: string,
  role: Exclude<GroupRole, "owner">,
): Promise<void> {
  return apiRequest<void>(
    `/api/groups/${encodeURIComponent(String(conversationId))}/members/${encodeURIComponent(userId)}/role`,
    { method: "POST", body: { role }, accessToken },
  );
}

/** POST /api/groups/{id}/transfer { user_id } (owner) → 200. */
export function transferGroupOwnership(
  accessToken: string,
  conversationId: number,
  userId: string,
): Promise<void> {
  return apiRequest<void>(
    `/api/groups/${encodeURIComponent(String(conversationId))}/transfer`,
    { method: "POST", body: { user_id: userId }, accessToken },
  );
}

/** POST /api/groups/{id}/leave → 204 (owner is refused server-side). */
export function leaveGroup(
  accessToken: string,
  conversationId: number,
): Promise<void> {
  return apiRequest<void>(
    `/api/groups/${encodeURIComponent(String(conversationId))}/leave`,
    { method: "POST", accessToken },
  );
}

/**
 * Maps a group failure to a localized human message using the backend's
 * machine error code; falls back to the shared mapper.
 */
export function groupApiErrorMessage(
  error: unknown,
  translate: Translate,
): string {
  if (!(error instanceof ApiError)) return translate("errors.unknown");
  switch (error.machine) {
    case "owner_cannot_leave":
      return translate("errors.owner_cannot_leave");
    case "not_group_member":
      return translate("errors.not_group_member");
    case "forbidden":
      return translate("errors.forbidden");
    default:
      return apiErrorMessage(error, translate);
  }
}
