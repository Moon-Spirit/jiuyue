import { AVATAR_EMOJIS } from "./avatars";

/**
 * Identity display helpers shared across every surface that shows a user:
 * conversation rows, contacts, search hits, thread headers, profiles.
 *
 * Server responses carry BOTH `username` (stable, unique, lowercase) and the
 * optional `display_name` (a user-chosen label, ≤24 chars, may be empty).
 * Wherever we show someone we prefer the display name and fall back to the
 * username — `displayNameOf` encodes that single rule so views can't drift.
 */

/** Any server/user-shaped object that carries the display fields. */
export interface DisplayUser {
  username: string;
  display_name?: string | null;
  avatar?: string | null;
}

/** display_name when present and non-empty, else the username. */
export function displayNameOf(
  user: Pick<DisplayUser, "username"> & {
    display_name?: string | null;
  },
): string {
  const display = user.display_name?.trim() ?? "";
  return display.length > 0 ? display : user.username;
}

/** The avatar emoji when set (and a recognized picker entry), else null —
 *  callers render the initial-circle fallback. */
export function avatarEmojiOf(
  avatar: string | null | undefined,
  emojis: readonly string[] = AVATAR_EMOJIS,
): string | null {
  const candidate = avatar?.trim() ?? "";
  if (candidate.length === 0) return null;
  if (!emojis.includes(candidate as (typeof emojis)[number])) return null;
  return candidate;
}
