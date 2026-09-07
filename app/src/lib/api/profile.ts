import { apiRequest } from "./client";
import { AVATAR_EMOJIS } from "../avatars";

/**
 * User-profile HTTP surface (Bearer access). The parallel server build owns
 * this contract; these wrappers are the only client touch-point.
 *
 *   GET  /api/users/profile/{user_id} → 200 Profile
 *   PATCH /api/users/profile           → 200 Profile   (self only)
 *
 * All numbers in the profile come straight from the server's XP ledger; the
 * client only renders them through lib/levels.ts.
 */

export interface UserProfile {
  user_id: string;
  username: string;
  /** Numeric user identifier (QQ-style). */
  uid: number;
  /** Curated label; absent when never set. ≤24 chars server-side. */
  display_name: string;
  /** Free text; absent when never set. ≤200 chars server-side. */
  bio: string;
  /** One emoji from the curated set; absent when never set. */
  avatar: string | null;
  level: number;
  title: string;
  xp: number;
  /** XP required to climb from `level` to `level + 1`. */
  xp_to_next: number;
}

/** Editable subset of the profile (every field optional, empty clears). */
export interface ProfilePatch {
  display_name?: string;
  bio?: string;
  avatar?: string;
}

/** Body fields the PATCH endpoint validates against (mirrors the 422 rules). */
export const PROFILE_LIMITS = {
  displayNameMax: 24,
  bioMax: 200,
} as const;

/** Allowed avatar symbols — the server-curated emoji set (see lib/avatars). */
export const PROFILE_AVATAR_SET: readonly string[] = AVATAR_EMOJIS;

/** Server contract for the frozen profile payloads. */
export const PROFILE_FIELDS = {
  display_name: {
    max: PROFILE_LIMITS.displayNameMax,
    clearByEmpty: true,
    pattern: null as string | null,
  },
  bio: {
    max: PROFILE_LIMITS.bioMax,
    clearByEmpty: true,
    pattern: null as string | null,
  },
  avatar: {
    max: PROFILE_LIMITS.bioMax,
    clearByEmpty: true,
    /** Server-curated emoji set — enforced verbatim. */
    pattern: null as string | null,
  },
} as const;

/** One-shot profile fetch — never blocks boot (callers fire and forget). */
export function getProfile(
  accessToken: string,
  userId: string,
): Promise<UserProfile> {
  return apiRequest<UserProfile>(
    `/api/users/profile/${encodeURIComponent(userId)}`,
    { method: "GET", accessToken },
  );
}

/** PATCH the SELF profile (server rejects editing anyone else's). */
export function patchProfile(
  accessToken: string,
  patch: ProfilePatch,
): Promise<UserProfile> {
  return apiRequest<UserProfile>("/api/users/profile", {
    method: "PATCH",
    body: patch,
    accessToken,
  });
}
