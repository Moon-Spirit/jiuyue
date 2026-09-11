/**
 * Group "level title" tiers — the Minecraft-themed badge a group member earns
 * from the XP they have contributed to the group. Mirrors the server contract
 * (every 10 levels, 10 tiers) so the client can resolve a localized title even
 * when the server payload omits the pre-resolved `title`.
 *
 *   Lv.1–10 木头 · 11–20 石头 · 21–30 铁锭 · 31–40 金锭 · 41–50 钻石 ·
 *   51–60 绿宝石 · 61–70 下界合金锭 · 71–80 下界之星 · 81–90 龙蛋 · 91+ 鞘翅
 */

/** i18n key suffix of a group tier (see `group.tiers.*` in the locales). */
export type GroupTierKey =
  | "wood"
  | "stone"
  | "ironIngot"
  | "goldIngot"
  | "diamond"
  | "emerald"
  | "netheriteIngot"
  | "netherStar"
  | "dragonEgg"
  | "elytra";

export interface GroupTierBand {
  key: GroupTierKey;
  /** Smallest level in the band (inclusive). */
  min: number;
}

/** Ordered lowest → highest; `groupTierOf` picks the last band reached. */
export const GROUP_TIER_BANDS: readonly GroupTierBand[] = [
  { key: "wood", min: 1 },
  { key: "stone", min: 11 },
  { key: "ironIngot", min: 21 },
  { key: "goldIngot", min: 31 },
  { key: "diamond", min: 41 },
  { key: "emerald", min: 51 },
  { key: "netheriteIngot", min: 61 },
  { key: "netherStar", min: 71 },
  { key: "dragonEgg", min: 81 },
  { key: "elytra", min: 91 },
] as const;

/** Tier band holding `level` (levels ≤ 1 fall back to the first tier). */
export function groupTierOf(level: number): GroupTierBand {
  let band = GROUP_TIER_BANDS[0]!;
  for (const candidate of GROUP_TIER_BANDS) {
    if (level >= candidate.min) band = candidate;
  }
  return band;
}

/** Localized-title i18n key for a member's group level. */
export function groupTierTitleKey(level: number): string {
  return `group.tiers.${groupTierOf(level).key}`;
}
