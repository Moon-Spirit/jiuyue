import { defineStore } from "pinia";
import {
  acceptGroupInvite,
  declineGroupInvite,
  getGroup,
  inviteToGroup,
  kickGroupMember,
  leaveGroup as apiLeaveGroup,
  listGroupInvites,
  setGroupMemberRole,
  transferGroupOwnership,
} from "../lib/api/groups";
import type {
  GroupInfo,
  GroupInvite,
  GroupMember,
  GroupRole,
} from "../lib/api/groups";
import { ApiError } from "../lib/api/client";
import type { GroupInvited, GroupUpdated } from "../lib/protocol/frames";
import { useAuthStore } from "./auth";

/** Lookup label for a member (display_name when set, else username). */
function memberLabel(member: {
  username: string;
  display_name?: string;
}): string {
  const display = member.display_name?.trim() ?? "";
  return display.length > 0 ? display : member.username;
}

function memberAvatar(member: { avatar?: string | null }): string | null {
  const avatar = member.avatar?.trim() ?? "";
  return avatar.length > 0 ? avatar : null;
}

/**
 * Groups store: caches authoritative group info (roster + my role), the
 * pending-invite list behind the contacts badge, and the per-conversation
 * member-name/avatar maps that group bubbles use for sender identity.
 *
 * The ws store delegates `group.invited` / `group.updated` frames here, so
 * realtime mutations and manual refreshes share one code path.
 */
export const useGroupsStore = defineStore("groups", {
  state: () => ({
    /** conversation_id → latest fetched GroupInfo. */
    infos: {} as Record<number, GroupInfo>,
    /** Pending invites addressed to me (newest first on live arrival). */
    invites: [] as GroupInvite[],
    /** conversation_id → user_id → sender label for group bubbles. */
    memberNames: {} as Record<number, Record<string, string>>,
    /** conversation_id → user_id → avatar emoji (or null). */
    memberAvatars: {} as Record<number, Record<string, string | null>>,
    /** Whether the invite list was fetched at least once. */
    invitesLoaded: false,
  }),

  getters: {
    /** Badge count for the nav 通讯录 entry. */
    pendingInviteCount: (state): number => state.invites.length,

    /** Cached group info for a conversation (null when never fetched). */
    infoFor:
      (state) =>
      (conversationId: number | null): GroupInfo | null => {
        if (conversationId === null) return null;
        return state.infos[conversationId] ?? null;
      },

    /** Sender label for a group bubble (falls back to the raw id). */
    memberNameFor:
      (state) =>
      (conversationId: number, userId: string): string =>
        state.memberNames[conversationId]?.[userId] ?? userId,
  },

  actions: {
    async token(): Promise<string> {
      const auth = useAuthStore();
      const accessToken = await auth.ensureAccessToken();
      if (accessToken === null) {
        throw new ApiError(0, "network_error", "not signed in");
      }
      return accessToken;
    },

    /** Rebuilds the sender-identity maps from an authoritative roster. */
    applyGroupInfo(info: GroupInfo): void {
      this.infos[info.conversation_id] = info;
      const names: Record<string, string> = {};
      const avatars: Record<string, string | null> = {};
      for (const member of info.members) {
        names[member.user_id] = memberLabel(member);
        avatars[member.user_id] = memberAvatar(member);
      }
      this.memberNames[info.conversation_id] = names;
      this.memberAvatars[info.conversation_id] = avatars;
    },

    async fetchInfo(conversationId: number): Promise<GroupInfo | null> {
      try {
        const info = await getGroup(await this.token(), conversationId);
        this.applyGroupInfo(info);
        return info;
      } catch {
        // Info is cosmetic/permission metadata; callers keep their last copy.
        return this.infos[conversationId] ?? null;
      }
    },

    async loadInvites(): Promise<void> {
      this.invites = await listGroupInvites(await this.token());
      this.invitesLoaded = true;
    },

    /** WS `group.invited`: append the invite immediately (deduped). */
    onInvited(payload: GroupInvited): void {
      if (this.invites.some((i) => i.invite_id === payload.invite_id)) return;
      this.invites = [
        {
          invite_id: payload.invite_id,
          conversation_id: payload.conversation_id,
          group_name: payload.group_name,
          from: payload.from,
        },
        ...this.invites,
      ];
      this.invitesLoaded = true;
    },

    /**
     * WS `group.updated`: refresh the affected info when we hold it (or the
     * conversation is open) and always refresh the sessions listing so group
     * names/roster sizes stay current.
     */
    async onUpdated(payload: GroupUpdated): Promise<void> {
      const { useWsStore } = await import("./ws");
      const ws = useWsStore();
      const known =
        this.infos[payload.conversation_id] !== undefined ||
        ws.activeConversationId === payload.conversation_id;
      if (known) void this.fetchInfo(payload.conversation_id);
      void ws.refreshListing();
    },

    /** Accepts an invite; returns the joined conversation id (or null). */
    async acceptInvite(inviteId: string): Promise<number | null> {
      const result = await acceptGroupInvite(await this.token(), inviteId);
      this.invites = this.invites.filter((i) => i.invite_id !== inviteId);
      const { useWsStore } = await import("./ws");
      const ws = useWsStore();
      try {
        await ws.refreshListing();
      } catch {
        // Listing refresh is best-effort; the conversation may still open.
      }
      ws.openConversation(result.conversation_id);
      return result.conversation_id;
    },

    async declineInvite(inviteId: string): Promise<void> {
      await declineGroupInvite(await this.token(), inviteId);
      this.invites = this.invites.filter((i) => i.invite_id !== inviteId);
    },

    /** Owner/admin invites a username, then refetches the roster. */
    async inviteMember(
      conversationId: number,
      username: string,
    ): Promise<void> {
      await inviteToGroup(await this.token(), conversationId, username);
      await this.fetchInfo(conversationId);
    },

    async kickMember(conversationId: number, userId: string): Promise<void> {
      await kickGroupMember(await this.token(), conversationId, userId);
      await this.fetchInfo(conversationId);
    },

    async changeMemberRole(
      conversationId: number,
      userId: string,
      role: Exclude<GroupRole, "owner">,
    ): Promise<void> {
      await setGroupMemberRole(
        await this.token(),
        conversationId,
        userId,
        role,
      );
      await this.fetchInfo(conversationId);
    },

    async transferOwnership(
      conversationId: number,
      userId: string,
    ): Promise<void> {
      await transferGroupOwnership(await this.token(), conversationId, userId);
      await this.fetchInfo(conversationId);
    },

    /** Leaves a group: drops local cache and forgets the conversation. */
    async leave(conversationId: number): Promise<void> {
      await apiLeaveGroup(await this.token(), conversationId);
      delete this.infos[conversationId];
      delete this.memberNames[conversationId];
      delete this.memberAvatars[conversationId];
      const { useWsStore } = await import("./ws");
      useWsStore().forgetConversation(conversationId);
    },

    /** Member list accessor used by the info panel. */
    membersOf(conversationId: number): GroupMember[] {
      return this.infos[conversationId]?.members ?? [];
    },

    /** Clears cached state; called on logout so a new account starts clean. */
    reset(): void {
      this.infos = {};
      this.invites = [];
      this.memberNames = {};
      this.memberAvatars = {};
      this.invitesLoaded = false;
    },
  },
});
