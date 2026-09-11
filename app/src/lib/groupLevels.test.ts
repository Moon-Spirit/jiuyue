import { describe, expect, it } from "vitest";
import { groupTierOf, groupTierTitleKey } from "./groupLevels";

describe("groupLevels — 群等级称号阶梯", () => {
  it("maps each level to the right 10-level tier", () => {
    expect(groupTierOf(1).key).toBe("wood");
    expect(groupTierOf(10).key).toBe("wood");
    expect(groupTierOf(11).key).toBe("stone");
    expect(groupTierOf(20).key).toBe("stone");
    expect(groupTierOf(21).key).toBe("ironIngot");
    expect(groupTierOf(41).key).toBe("diamond");
    expect(groupTierOf(50).key).toBe("diamond");
    expect(groupTierOf(51).key).toBe("emerald");
    expect(groupTierOf(61).key).toBe("netheriteIngot");
    expect(groupTierOf(71).key).toBe("netherStar");
    expect(groupTierOf(81).key).toBe("dragonEgg");
    expect(groupTierOf(90).key).toBe("dragonEgg");
    expect(groupTierOf(91).key).toBe("elytra");
    expect(groupTierOf(200).key).toBe("elytra");
  });

  it("exposes the localized i18n key for a member's level", () => {
    expect(groupTierTitleKey(15)).toBe("group.tiers.stone");
    expect(groupTierTitleKey(55)).toBe("group.tiers.emerald");
    expect(groupTierTitleKey(120)).toBe("group.tiers.elytra");
  });
});
