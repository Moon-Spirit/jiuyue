/**
 * Avatar vocabulary shared with the server.
 *
 * The backend only ever accepts avatars from this curated emoji set (a
 * PATCH that sends anything else → 422), so the client picker grid renders
 * exactly these candidates and nothing custom can creep into the pipeline.
 *
 * Emoji are stored as their raw UTF-16 pairs (no ZWJ/variation selectors) —
 * every entry below is a single `code point` pair so length/counting math in
 * the UI stays 1:1 with what the server validates.
 */

/** The full curated set — also the server's allow-list (mock/backend mirror). */
export const AVATAR_EMOJIS: readonly string[] = [
  "🐶",
  "🐱",
  "🐭",
  "🐹",
  "🐰",
  "🦊",
  "🐻",
  "🐼",
  "🐨",
  "🐯",
  "🦁",
  "🐮",
  "🐷",
  "🐸",
  "🐵",
  "🦄",
  "🐲",
  "🦋",
  "🐢",
  "🐙",
  "🦀",
  "🐬",
  "🐳",
  "🦉",
  "🦅",
  "🍎",
  "🍊",
  "🍋",
  "🍉",
  "🍇",
  "🍓",
  "🍑",
  "🥑",
  "🌽",
  "🍕",
  "⚽",
  "🏀",
  "🎮",
  "🎧",
  "🎯",
  "🎨",
  "🌈",
  "🔥",
  "❄️",
  "⭐",
  "🌙",
  "☀️",
] as const;

/** True when `candidate` is one of the curated emojis (client-side mirror of
 *  the server's 422 rule). */
export function isAvatarAllowed(candidate: string): boolean {
  return AVATAR_EMOJIS.includes(candidate as (typeof AVATAR_EMOJIS)[number]);
}
