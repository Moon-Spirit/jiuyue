import { defineStore } from "pinia";
import {
  login as apiLogin,
  refreshSession,
  register as apiRegister,
  requestCode as apiRequestCode,
} from "../lib/api/auth";
import type { AuthChannel, RegisterInput } from "../lib/api/auth";

const REFRESH_TOKEN_KEY = "jiuyue.refresh";
const USER_KEY = "jiuyue.user";

export interface AuthUser {
  userId: number;
  username: string;
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
    if (typeof userId === "number" && typeof username === "string") {
      return { userId, username };
    }
    return null;
  } catch {
    // Corrupted/unavailable storage must degrade to "signed out", never crash boot.
    return null;
  }
}

/**
 * The login endpoint returns tokens only (no profile), so derive a display
 * name from the identifier. Replaced once a profile endpoint exists.
 */
function displayNameFromIdentifier(identifier: string): string {
  const [name] = identifier.split("@");
  if (name === undefined || name.length === 0) return identifier;
  return name;
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
        this.adoptTokens(result.access_token, result.refresh_token, {
          userId: result.user_id,
          username: result.username,
        });
      } catch (error) {
        this.status = "anon";
        throw error;
      }
    },

    async login(identifier: string, password: string): Promise<void> {
      this.status = "loading";
      try {
        const tokens = await apiLogin(identifier, password);
        this.adoptTokens(tokens.access_token, tokens.refresh_token, {
          userId: 0,
          username: displayNameFromIdentifier(identifier),
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

    logout(): void {
      this.accessToken = null;
      this.user = null;
      this.status = "anon";
      // No server-side revoke endpoint exists; clearing is client-side only.
      localStorage.removeItem(REFRESH_TOKEN_KEY);
      localStorage.removeItem(USER_KEY);
    },

    async performRefresh(): Promise<string | null> {
      const stored = localStorage.getItem(REFRESH_TOKEN_KEY);
      if (stored === null) return null;
      try {
        const tokens = await refreshSession(stored);
        this.accessToken = tokens.access_token;
        localStorage.setItem(REFRESH_TOKEN_KEY, tokens.refresh_token);
        if (this.user === null) this.user = readPersistedUser();
        this.status = "authed";
        return tokens.access_token;
      } catch {
        // Dead/expired refresh token: drop local session silently.
        this.logout();
        return null;
      }
    },

    adoptTokens(
      accessToken: string,
      refreshToken: string,
      user: AuthUser,
    ): void {
      this.accessToken = accessToken;
      this.user = user;
      this.status = "authed";
      localStorage.setItem(REFRESH_TOKEN_KEY, refreshToken);
      localStorage.setItem(USER_KEY, JSON.stringify(user));
    },
  },
});
