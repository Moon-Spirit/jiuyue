/**
 * XP / level / rank helpers — pure display logic shared with tests.
 *
 * The server is the single source of truth for awarding XP; these functions
 * only DERIVE display state (level bars, "distance to next level", the
 * Minecraft-block title band) from the profile numbers a GET returns.
 *
 * Level curve (frozen contract):
 *   req(1→2) = 50
 *   req(N)   = round(50 × 1.1^(N-1))   for the climb from level N to N+1
 *
 * Title bands (level ranges → Minecraft blocks, 9 tiers):
 *   1-5 土块 · 6-10 木头 · 11-15 石头 · 16-20 铁块 · 21-25 金块 ·
 *   26-30 钻石块 · 31-35 下界合金块 · 36-40 下界之星 · 41+ 龙蛋
 */

/** XP required to climb FROM `level` TO `level + 1` (level ≥ 1). */
export function xpToNext(level: number): number {
  const safe = Math.max(1, Math.floor(level));
  return Math.round(50 * 1.1 ** (safe - 1));
}

/** Cumulative XP needed to REACH `level` (level 1 costs nothing). */
export function xpForLevel(level: number): number {
  let total = 0;
  for (let lv = 1; lv < Math.max(1, Math.floor(level)); lv += 1) {
    total += xpToNext(lv);
  }
  return total;
}

/**
 * XP earned inside the CURRENT level: total XP minus the cumulative cost of
 * every level below it. The level bar renders `progressInLevel / xpToNext(level)`.
 */
export function progressInLevel(totalXp: number, level: number): number {
  return Math.max(0, Math.floor(totalXp) - xpForLevel(level));
}

/** i18n key suffix of the title band holding `level` (see rankTitles in the
 *  locale files: profile.rankTitles.dirt … profile.rankTitles.dragonEgg). */
export type RankKey =
  | "dirt"
  | "wood"
  | "stone"
  | "iron"
  | "gold"
  | "diamond"
  | "netherite"
  | "netherStar"
  | "dragonEgg";

export interface RankBand {
  key: RankKey;
  /** Smallest level in the band (inclusive). */
  min: number;
  /** Color dot shown next to the localized title chip (hex). */
  color: string;
}

/** Ordered lowest → highest; `titleForLevel` picks the last band whose
 *  `min` the level reaches. */
export const RANK_BANDS: readonly RankBand[] = [
  { key: "dirt", min: 1, color: "#a2703e" },
  { key: "wood", min: 6, color: "#8b5a2b" },
  { key: "stone", min: 11, color: "#9e9e9e" },
  { key: "iron", min: 16, color: "#d8dbe2" },
  { key: "gold", min: 21, color: "#f6c945" },
  { key: "diamond", min: 26, color: "#4aedd9" },
  { key: "netherite", min: 31, color: "#4a3728" },
  { key: "netherStar", min: 36, color: "#cfcfe8" },
  { key: "dragonEgg", min: 41, color: "#191523" },
] as const;

/** Title band key for a level (levels ≥ 41 → dragonEgg). */
export function bandOf(level: number): RankBand {
  let band = RANK_BANDS[0]!;
  for (const candidate of RANK_BANDS) {
    if (level >= candidate.min) band = candidate;
  }
  return band;
}

/** True when the level has already reached the final (dragon egg) tier. */
export function isMaxTitle(level: number): boolean {
  return level >= 41;
}
