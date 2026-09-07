/**
 * Avatar vocabulary shared with the server.
 *
 * The backend only ever accepts avatars from this curated emoji set (a
 * PATCH that sends anything else → 422), so the client picker grid renders
 * exactly these candidates and nothing custom can creep into the pipeline.
 *
 * This list MUST mirror the server's `AVATARS` constant in
 * crates/server/src/profile.rs (44 Minecraft-themed entries). If one side
 * changes, the other must follow or every avatar save 422s.
 */

/** The full curated set — must equal the server's allow-list exactly. */
export const AVATAR_EMOJIS: readonly string[] = [
  "🟫",
  "🪨",
  "🪵",
  "🧱",
  "⛏️",
  "🪓",
  "🏹",
  "🗡️",
  "🛡️",
  "🪖",
  "💎",
  "🪙",
  "⭐",
  "🌟",
  "🔥",
  "💧",
  "🌱",
  "🌳",
  "🍄",
  "🐷",
  "🐮",
  "🐑",
  "🐔",
  "🐺",
  "🐱",
  "🐲",
  "🥚",
  "⚗️",
  "🧪",
  "🪄",
  "📦",
  "🚪",
  "🗺️",
  "🧭",
  "🏔️",
  "🌋",
  "🌙",
  "☀️",
  "👑",
  "💀",
  "👾",
  "⚡",
  "🌀",
  "🌸",
] as const;

/** True when `candidate` is one of the curated emojis (client-side mirror of
 *  the server's 422 rule). */
export function isAvatarAllowed(candidate: string): boolean {
  return AVATAR_EMOJIS.includes(candidate as (typeof AVATAR_EMOJIS)[number]);
}
