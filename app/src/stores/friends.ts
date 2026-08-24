import { defineStore } from "pinia";
import {
  acceptFriendRequest,
  cancelFriendRequest,
  declineFriendRequest,
  listFriendRequests,
  listFriends,
  sendFriendRequest,
  unfriend as apiUnfriend,
} from "../lib/api/friends";
import type {
  Friend,
  IncomingFriendRequest,
  OutgoingFriendRequest,
} from "../lib/api/friends";
import type { FriendAccepted, FriendRequested } from "../lib/protocol/frames";
import { ApiError } from "../lib/api/client";
import { useAuthStore } from "./auth";

/**
 * Friends store: the local cache behind the contacts panel (通讯录).
 *
 * State mirrors three server lists (friends / incoming / outgoing requests)
 * plus a `loaded` flag so views can distinguish "not fetched yet" from
 * "genuinely empty". WS frames (`friend.requested` / `friend.accepted`)
 * mutate the same lists through `onFriendRequested` / `onFriendAccepted`,
 * which the ws store delegates to — keeping realtime updates and manual
 * refreshes on one code path.
 */
export const useFriendsStore = defineStore("friends", {
  state: () => ({
    friends: [] as Friend[],
    incoming: [] as IncomingFriendRequest[],
    outgoing: [] as OutgoingFriendRequest[],
    loaded: false,
  }),

  getters: {
    /** Badge count for the nav 通讯录 entry. */
    pendingCount: (state): number => state.incoming.length,
  },

  actions: {
    /** Fetches both lists; used on view mount and after reconnects. */
    async loadAll(): Promise<void> {
      await Promise.all([this.fetchFriends(), this.fetchRequests()]);
      this.loaded = true;
    },

    async fetchFriends(): Promise<void> {
      this.friends = await listFriends(await this.token());
    },

    async fetchRequests(): Promise<void> {
      const lists = await listFriendRequests(await this.token());
      this.incoming = lists.incoming;
      this.outgoing = lists.outgoing;
    },

    /** Sends a request and optimistically appends it to `outgoing`. */
    async sendRequest(username: string): Promise<void> {
      const result = await sendFriendRequest(await this.token(), username);
      if (!this.outgoing.some((r) => r.request_id === result.request_id)) {
        this.outgoing.push({
          request_id: result.request_id,
          to: result.to,
          created_at: new Date().toISOString(),
        });
      }
    },

    /** Accepts an incoming request: friend added, request removed. */
    async accept(requestId: string): Promise<void> {
      const result = await acceptFriendRequest(await this.token(), requestId);
      this.incoming = this.incoming.filter((r) => r.request_id !== requestId);
      if (!this.friends.some((f) => f.user_id === result.friend.user_id)) {
        this.friends.push({
          user_id: result.friend.user_id,
          username: result.friend.username,
          since: new Date().toISOString(),
        });
      }
    },

    /** Declines an incoming request. */
    async decline(requestId: string): Promise<void> {
      await declineFriendRequest(await this.token(), requestId);
      this.incoming = this.incoming.filter((r) => r.request_id !== requestId);
    },

    /** Cancels one of MY outgoing requests. */
    async cancel(requestId: string): Promise<void> {
      await cancelFriendRequest(await this.token(), requestId);
      this.outgoing = this.outgoing.filter((r) => r.request_id !== requestId);
    },

    /** Removes an established friendship. */
    async unfriend(userId: string): Promise<void> {
      await apiUnfriend(await this.token(), userId);
      this.friends = this.friends.filter((f) => f.user_id !== userId);
    },

    /**
     * WS `friend.requested`: push onto `incoming` (deduped by request_id).
     * Called by the ws store's frame handler.
     */
    onFriendRequested(payload: FriendRequested): void {
      if (this.incoming.some((r) => r.request_id === payload.request_id)) {
        return;
      }
      this.incoming.push({
        request_id: payload.request_id,
        from: payload.from,
        created_at: new Date().toISOString(),
      });
    },

    /**
     * WS `friend.accepted`: my outgoing request became a friendship —
     * add the friend (deduped by user_id) and drop the pending entry.
     */
    onFriendAccepted(payload: FriendAccepted): void {
      if (!this.friends.some((f) => f.user_id === payload.friend.user_id)) {
        this.friends.push({
          user_id: payload.friend.user_id,
          username: payload.friend.username,
          since: new Date().toISOString(),
        });
      }
      this.outgoing = this.outgoing.filter(
        (r) => r.to.user_id !== payload.friend.user_id,
      );
    },

    /** Bearer token or a typed failure, mirroring the ws store pattern. */
    async token(): Promise<string> {
      const auth = useAuthStore();
      const accessToken = await auth.ensureAccessToken();
      if (accessToken === null) {
        throw new ApiError(0, "network_error", "not signed in");
      }
      return accessToken;
    },
  },
});
