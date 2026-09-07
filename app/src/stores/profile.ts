import { defineStore } from "pinia";
import { getProfile, patchProfile } from "../lib/api/profile";
import type { ProfilePatch, UserProfile } from "../lib/api/profile";
import { ApiError } from "../lib/api/client";
import { useAuthStore } from "./auth";

/**
 * Profile store: one-view-many-observers cache for the CURRENT user's
 * profile plus an in-memory peer cache keyed by user UUID.
 *
 * Rules honoured here (spec, client side):
 * - Profile fetch NEVER blocks auth: `ensureLoaded` is a background call the
 *   chat/contacts views fire on mount; while it is in flight the app renders
 *   from the persisted AuthUser fields ("" / null are safe defaults).
 * - Peer lookups fall back gracefully: unknown UUID → the transient shape
 *   seeded from whatever the caller had (e.g. a conversation row), which the
 *   profile page then refreshes from the server.
 */

interface ProfileState {
  /** Current user's full profile; null until the first GET resolves. */
  me: UserProfile | null;
  /** True while the self-profile GET is in flight. */
  loading: boolean;
  /** Last failed self-profile GET error (cleared on success). */
  error: string | null;
  /** Last PATCH error message for the edit form. */
  patchError: string | null;
  /** In-memory peer profiles keyed by user UUID (never persisted). */
  peers: Record<string, UserProfile>;
}

function fromAuth(): UserProfile {
  const auth = useAuthStore();
  const user = auth.user;
  return {
    user_id: user?.userId ?? "",
    username: user?.username ?? "",
    uid: user?.uid ?? 0,
    display_name: user?.displayName ?? "",
    bio: user?.bio ?? "",
    avatar: user?.avatar ?? null,
    level: 1,
    title: "",
    xp: 0,
    xp_to_next: 0,
  };
}

export const useProfileStore = defineStore("profile", {
  state: (): ProfileState => ({
    me: null,
    loading: false,
    error: null,
    patchError: null,
    peers: {},
  }),

  getters: {
    /** Best-effort self profile; falls back to a transient AuthUser seed. */
    selfProfile(state): UserProfile {
      return state.me ?? fromAuth();
    },
    /** Peer profile by user UUID, falling back to a transient seed. */
    peerProfile: (state) => {
      return (
        userId: string,
        seed?: {
          username?: string;
          uid?: number;
          displayName?: string;
          avatar?: string | null;
        },
      ): UserProfile => {
        const cached = state.peers[userId];
        if (cached !== undefined) return cached;
        return {
          user_id: userId,
          username: seed?.username ?? "",
          uid: seed?.uid ?? 0,
          display_name: seed?.displayName ?? "",
          bio: "",
          avatar: seed?.avatar ?? null,
          level: 1,
          title: "",
          xp: 0,
          xp_to_next: 0,
        };
      };
    },
  },

  actions: {
    /**
     * One-shot self profile GET. Fire-and-forget from view mounts — never
     * awaited by auth flows. The returned promise lets callers await when
     * they actually need the data (the profile page).
     */
    async fetchMe(): Promise<UserProfile | null> {
      if (this.loading && this.me !== null) return this.me;
      const auth = useAuthStore();
      const token = await auth.ensureAccessToken();
      if (token === null) return null;
      this.loading = true;
      this.error = null;
      try {
        const profile = await getProfile(token, auth.user?.userId ?? "");
        this.applyMe(profile);
        this.loading = false;
        return profile;
      } catch (error) {
        this.error =
          error instanceof ApiError ? error.message : "network_error";
        this.loading = false;
        return null;
      }
    },

    /** Fires the self-profile GET without blocking the caller. */
    ensureLoaded(): void {
      const auth = useAuthStore();
      if (auth.user === null) return;
      if (this.me !== null || this.loading) return;
      void this.fetchMe();
    },

    /**
     * PATCHes the self profile and, on success, mirrors the editable fields
     * into the auth user (nav/other surfaces update immediately) and the
     * local cache. Returns the updated profile or throws.
     */
    async saveMe(patch: ProfilePatch): Promise<UserProfile> {
      const auth = useAuthStore();
      const token = await auth.ensureAccessToken();
      if (token === null) {
        throw new ApiError(0, "network_error", "not signed in");
      }
      this.patchError = null;
      const updated = await patchProfile(token, patch);
      this.applyMe(updated);
      return updated;
    },

    applyMe(profile: UserProfile): void {
      this.me = profile;
      const auth = useAuthStore();
      if (auth.user === null) return;
      // Keep auth's persisted user in sync with the editable profile fields.
      auth.user = {
        ...auth.user,
        displayName: profile.display_name ?? "",
        avatar: profile.avatar ?? null,
        bio: profile.bio ?? "",
      };
      try {
        localStorage.setItem("jiuyue.user", JSON.stringify(auth.user));
      } catch {
        // Persistence is best-effort (private mode / quota).
      }
    },

    /**
     * Refreshes the self profile from the server — the profile page calls
     * this on entry so returning from a session/contact always re-reads.
     */
    async refreshMe(): Promise<UserProfile | null> {
      const auth = useAuthStore();
      if (auth.user === null) return null;
      this.loading = true;
      this.error = null;
      try {
        const token = await auth.ensureAccessToken();
        if (token === null) {
          this.loading = false;
          return null;
        }
        const profile = await getProfile(token, auth.user.userId);
        this.applyMe(profile);
        this.loading = false;
        return profile;
      } catch (error) {
        this.error =
          error instanceof ApiError ? error.message : "network_error";
        this.loading = false;
        return null;
      }
    },

    async fetchPeer(userId: string): Promise<UserProfile | null> {
      const auth = useAuthStore();
      const token = await auth.ensureAccessToken();
      if (token === null) return null;
      try {
        const profile = await getProfile(token, userId);
        this.peers[userId] = profile;
        return profile;
      } catch {
        // Peer lookups are best-effort: the view falls back to its seed.
        return null;
      }
    },

    /** Seeds a transient peer from wire-level row data; server refresh later
     *  replaces it. Does nothing when the peer is already cached. */
    seedPeer(
      userId: string,
      data: {
        username?: string;
        uid?: number;
        displayName?: string;
        avatar?: string | null;
      },
    ): void {
      if (this.peers[userId] !== undefined) return;
      this.peers[userId] = {
        user_id: userId,
        username: data.username ?? "",
        uid: data.uid ?? 0,
        display_name: data.displayName ?? "",
        bio: "",
        avatar: data.avatar ?? null,
        level: 1,
        title: "",
        xp: 0,
        xp_to_next: 0,
      };
    },

    /** Clears caches on logout so a new account never sees the old one's. */
    reset(): void {
      this.me = null;
      this.peers = {};
      this.loading = false;
      this.error = null;
      this.patchError = null;
    },
  },
});
