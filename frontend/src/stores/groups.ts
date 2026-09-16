/**
 * The Group domain's frontend rules: which Role may do what, and how an
 * announcement is edited.
 *
 * # One mirror, not a second permission model
 *
 * `jiuyue-chat::permission` is the single authority on Group permissions; the
 * server re-checks every action. The predicates below **mirror** that decision
 * table so the info panel never offers an action that will be refused — they are
 * not the enforcement. Keeping the mirror in one module (rather than spread
 * through the panel) is what stops the two drifting into "a button that fails",
 * which is exactly what happens when a component invents its own rules.
 *
 * The mirror is deliberately structural, not incidental: {@link may} answers the
 * same table as `permission::may`, and the two refinements (`may_remove` /
 * `may_leave`) keep their own functions because a Role alone does not answer them.
 */
import { ApiError, apiPatchAuthed } from "../api/client";
import type { GroupInfo } from "../generated/GroupInfo";
import type { Role } from "../generated/Role";
import { useAuthStore } from "./auth";
import { useChatStore } from "./chat";

/**
 * Hard cap on a Group announcement, in Unicode code points.
 *
 * Mirrors `MAX_ANNOUNCEMENT_CHARS` in the Rust contract and the
 * `conversations_announcement_length` CHECK in the schema, so the limit is felt
 * before a round trip while the server remains the authority.
 */
export const MAX_ANNOUNCEMENT_CHARS = 2000;

/**
 * An action a Participant might take in a Group Conversation.
 *
 * The names mirror `jiuyue_chat::permission::Capability` so the two tables can be
 * compared line by line.
 */
export type Capability =
  | "viewMembers"
  | "postMessage"
  | "inviteMembers"
  | "removeMembers"
  | "changeRoles"
  | "transferOwnership"
  | "dissolve"
  | "editGroupInfo"
  | "leave";

/**
 * Compile-time exhaustiveness check.
 *
 * A new {@link Capability} or {@link Role} stops the branch being `never`, so the
 * build fails until the answer is written down — the same guarantee the Rust
 * `match` gives.
 */
function assertExhaustive(_value: never): false {
  return false;
}

/**
 * Whether `role` may perform `capability`, ignoring per-target refinements.
 *
 * Mirrors `permission::may` exactly:
 *
 * | Capability                              | Owner | Admin | Member |
 * | --------------------------------------- | :---: | :---: | :----: |
 * | viewMembers / postMessage / leave       |   ✓   |   ✓   |   ✓    |
 * | inviteMembers / removeMembers / editGroupInfo | ✓ | ✓ |   ✗    |
 * | changeRoles / transferOwnership / dissolve | ✓  |   ✗   |   ✗    |
 */
export function may(role: Role, capability: Capability): boolean {
  switch (capability) {
    case "viewMembers":
    case "postMessage":
    case "leave":
      return true;
    case "inviteMembers":
    case "removeMembers":
    case "editGroupInfo":
      return role === "owner" || role === "admin";
    case "changeRoles":
    case "transferOwnership":
    case "dissolve":
      return role === "owner";
    default:
      return assertExhaustive(capability);
  }
}

/**
 * Whether a Participant holding `actor` may remove one holding `target`.
 *
 * Mirrors `permission::may_remove`: the owner may remove an admin or a member but
 * not another owner; an admin may remove an ordinary member only. The caller's
 * own row is excluded by the panel, because removal is never self-removal.
 */
export function mayRemove(actor: Role, target: Role): boolean {
  switch (actor) {
    case "owner":
      return target !== "owner";
    case "admin":
      return target === "member";
    case "member":
      return false;
    default:
      return assertExhaustive(actor);
  }
}

/**
 * Whether a Participant holding `role` may leave when `othersRemaining`
 * Participants would stay behind.
 *
 * Mirrors `permission::may_leave`: an owner must transfer ownership or dissolve
 * first, because a group with no owner has nobody who can administer it.
 */
export function mayLeave(role: Role, othersRemaining: number): boolean {
  return role !== "owner" || othersRemaining <= 0;
}

/**
 * The length problem with a draft announcement, or `null` when it is acceptable.
 *
 * Length is counted in Unicode code points (`[...text].length`), matching the
 * server's `chars().count()`, so an emoji counts once and not twice.
 */
export function announcementProblem(announcement: string): string | null {
  if ([...announcement].length > MAX_ANNOUNCEMENT_CHARS) {
    return `群公告最多 ${MAX_ANNOUNCEMENT_CHARS} 个字符`;
  }
  return null;
}

/** The outcome of an announcement edit: either it landed, or it did not. */
export type AnnouncementResult =
  { readonly ok: true } | { readonly ok: false; readonly message: string };

/**
 * Replace a Group's announcement, or clear it by passing blank text.
 *
 * The server is the authority on permission and length; this mirrors the length
 * check so an over-long draft never leaves the browser, and it surfaces the
 * server's own message (a refusal or a validation failure) when one comes back.
 *
 * After a successful write the group's info is re-read, so the panel — which
 * renders the conversation the chat store holds — shows the stored text without
 * waiting for the live event. The event is still delivered (every Participant,
 * the editor included, is in the fan-out), so other Devices update too.
 */
export async function editAnnouncement(
  conversationId: string,
  announcement: string,
): Promise<AnnouncementResult> {
  const text = announcement.trim();
  const problem = announcementProblem(text);
  if (problem !== null) return { ok: false, message: problem };

  const token = useAuthStore().accessToken;
  if (token === null) {
    return { ok: false, message: "登录状态已失效，请重新登录" };
  }

  try {
    // A blank draft clears the announcement; the server stores SQL NULL for it.
    await apiPatchAuthed<GroupInfo>(
      `/conversations/${conversationId}/announcement`,
      { announcement: text === "" ? null : text },
      token,
    );
    await useChatStore().loadGroupInfo(conversationId);
    return { ok: true };
  } catch (cause) {
    if (cause instanceof ApiError) {
      return {
        ok: false,
        message:
          cause.body?.error.message ?? `请求失败（HTTP ${cause.status}）`,
      };
    }
    return { ok: false, message: "无法连接服务器，请稍后重试" };
  }
}
