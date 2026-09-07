import { defineStore } from "pinia";
import {
  login as apiLogin,
  refreshSession,
  register as apiRegister,
  requestCode as apiRequestCode,
} from "../lib/api/auth";
import type { AuthChannel, RegisterInput } from "../lib/api/auth";
import {
  secureLoadRefresh,
  secureRemoveRefresh,
  secureStoreRefresh,
} from "../lib/secure";

const USER_KEY = "jiuyue.user";

export interface AuthUser {
  userId: string;
  username: string;
  /**
   * Numeric user identifier. Required on fresh login/register responses;
   * a pre-UID persisted profile restores as 0 (unknown) until the next
   * successful auth round-trip refreshes it.
   */
  uid: number;
}

export type AuthStatus = "anon" | "authed" | "loading";

/**
 * Shared in-flight refresh promise so concurrent `ensureAccessToken` calls
 * trigger exactly one network round-trip.
 */
let refreshInFlight: Promise<string | null> | null = null;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function readPersistedUser(): AuthUser | null {
  try {
    const raw = localStorage.getItem(USER_KEY);
    if (raw === null) return null;
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed)) return null;
    const { userId, username } = parsed;
    if (
      typeof userId === "string" &&
      userId.length > 0 &&
      typeof username === "string"
    ) {
      // Tolerate profiles persisted before UIDs existed: uid degrades to 0
      // and gets backfilled on the next login/register round-trip.
      const uid = typeof parsed["uid"] === "number" ? parsed["uid"] : 0;
      return { userId, username, uid };
    }
    return null;
  } catch {
    // Corrupted/unavailable storage must degrade to "signed out", never crash boot.
    return null;
  }
}

export const useAuthStore = defineStore("auth", {
  state: () => ({
    user: null as AuthUser | null,
    /** Access token lives in memory only — never persisted. */
    accessToken: null as string | null,
    status: "anon" as AuthStatus,
  }),

  actions: {
    async requestCode(channel: AuthChannel, target: string): Promise<number> {
      const result = await apiRequestCode(channel, target);
      return result.expires_in_secs;
    },

    async register(input: RegisterInput): Promise<void> {
      this.status = "loading";
      try {
        const result = await apiRegister(input);
        await this.adoptTokens(result.access_token, result.refresh_token, {
          userId: result.user_id,
          username: result.username,
          uid: result.uid,
        });
      } catch (error) {
        this.status = "anon";
        throw error;
      }
    },

    async login(identifier: string, password: string): Promise<void> {
      this.status = "loading";
      try {
        const result = await apiLogin(identifier, password);
        await this.adoptTokens(result.access_token, result.refresh_token, {
          userId: result.user_id,
          username: result.username,
          uid: result.uid,
        });
      } catch (error) {
        this.status = "anon";
        throw error;
      }
    },

    /**
     * Returns a usable access token: cached one, or a single refresh attempt.
     * Resolves `null` when the session cannot be restored.
     */
    async ensureAccessToken(): Promise<string | null> {
      if (this.accessToken !== null) return this.accessToken;
      if (refreshInFlight === null) {
        refreshInFlight = this.performRefresh().finally(() => {
          refreshInFlight = null;
        });
      }
      return refreshInFlight;
    },

    /** Boot-time session restoration. Safe offline: no token → no request. */
    async restoreSession(): Promise<boolean> {
      const access = await this.ensureAccessToken();
      this.status = access === null ? "anon" : "authed";
      return access !== null;
    },

    async logout(): Promise<void> {
      this.accessToken = null;
      this.user = null;
      this.status = "anon";
      // No server-side revoke endpoint exists; clearing is client-side only.
      await secureRemoveRefresh();
      localStorage.removeItem(USER_KEY);
      // Drop the previous account's in-memory + persisted data so a
      // subsequent login/registration never inherits conversations, friends,
      // or (critically) the E2EE identity of the prior user.
      const { useWsStore } = await import("./ws");
      const { useFriendsStore } = await import("./friends");
      useWsStore().dispose();
      useWsStore().wipeUserData();
      useFriendsStore().reset();
    },

    async performRefresh(): Promise<string | null> {
      const stored = await secureLoadRefresh();
      if (stored === null) return null;
      try {
        const tokens = await refreshSession(stored);
        this.accessToken = tokens.access_token;
        await secureStoreRefresh(tokens.refresh_token);
        if (this.user === null) this.user = readPersistedUser();
        this.status = "authed";
        return tokens.access_token;
      } catch {
        // Dead/expired refresh token: drop local session silently.
        await this.logout();
        return null;
      }
    },

    async adoptTokens(
      accessToken: string,
      refreshToken: string,
      user: AuthUser,
    ): Promise<void> {
      this.accessToken = accessToken;
      this.user = user;
      this.status = "authed";
      await secureStoreRefresh(refreshToken);
      localStorage.setItem(USER_KEY, JSON.stringify(user));
    },
  },
});
