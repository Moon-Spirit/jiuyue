import { describe, expect, it } from "vitest";
import { AVATAR_EMOJIS, isAvatarAllowed } from "./avatars";
import { avatarEmojiOf, displayNameOf } from "./identity";
import { bandOf, isMaxTitle, RANK_BANDS, xpForLevel, xpToNext } from "./levels";

describe("identity helpers", () => {
  it("prefers display_name over username and trims blank labels", () => {
    expect(displayNameOf({ username: "alice", display_name: "Alice 张" })).toBe(
      "Alice 张",
    );
    expect(displayNameOf({ username: "bob", display_name: "" })).toBe("bob");
    expect(displayNameOf({ username: "bob", display_name: null })).toBe("bob");
    expect(displayNameOf({ username: "bob" })).toBe("bob");
    expect(displayNameOf({ username: "carol", display_name: "   " })).toBe(
      "carol",
    );
  });

  it("returns the avatar emoji only when it belongs to the curated set", () => {
    expect(avatarEmojiOf("🐶")).toBe("🐶");
    expect(avatarEmojiOf("🐱")).toBe("🐱");
    // Unset / unknown values fall back to the initial circle.
    expect(avatarEmojiOf(null)).toBeNull();
    expect(avatarEmojiOf(undefined)).toBeNull();
    expect(avatarEmojiOf("")).toBeNull();
    expect(avatarEmojiOf("  ")).toBeNull();
    expect(avatarEmojiOf("🤖")).toBeNull(); // not server-curated
  });
});

describe("avatar vocabulary", () => {
  it("every picker entry passes the allow-list and stays unique", () => {
    expect(AVATAR_EMOJIS.length).toBeGreaterThanOrEqual(30);
    expect(new Set(AVATAR_EMOJIS).size).toBe(AVATAR_EMOJIS.length);
    for (const emoji of AVATAR_EMOJIS) {
      expect(isAvatarAllowed(emoji)).toBe(true);
    }
    expect(isAvatarAllowed("🤖")).toBe(false);
  });
});

describe("level curve", () => {
  it("charges 50 XP for the climb into level 2 (frozen contract)", () => {
    expect(xpToNext(1)).toBe(50);
  });

  it("scales each later step by 1.1 rounded", () => {
    expect(xpToNext(2)).toBe(Math.round(50 * 1.1 ** 1)); // 55
    expect(xpToNext(3)).toBe(Math.round(50 * 1.1 ** 2)); // 61
    expect(xpToNext(10)).toBe(Math.round(50 * 1.1 ** 9));
  });

  it("cumulative XP reaches exactly the sum of its steps", () => {
    // Level 1 costs nothing.
    expect(xpForLevel(1)).toBe(0);
    expect(xpForLevel(2)).toBe(50);
    expect(xpForLevel(3)).toBe(50 + xpToNext(2));
    expect(xpForLevel(10)).toBe(
      xpToNext(1) +
        xpToNext(2) +
        xpToNext(3) +
        xpToNext(4) +
        xpToNext(5) +
        xpToNext(6) +
        xpToNext(7) +
        xpToNext(8) +
        xpToNext(9),
    );
  });
});

describe("rank bands", () => {
  it("covers every level 1+ with the nine frozen Minecraft tiers", () => {
    expect(RANK_BANDS.map((b) => b.key)).toEqual([
      "dirt",
      "wood",
      "stone",
      "iron",
      "gold",
      "diamond",
      "netherite",
      "netherStar",
      "dragonEgg",
    ]);
    expect(RANK_BANDS[0]?.min).toBe(1);
  });

  it("maps each frozen band boundary exactly", () => {
    expect(bandOf(1).key).toBe("dirt");
    expect(bandOf(5).key).toBe("dirt");
    expect(bandOf(6).key).toBe("wood");
    expect(bandOf(10).key).toBe("wood");
    expect(bandOf(11).key).toBe("stone");
    expect(bandOf(15).key).toBe("stone");
    expect(bandOf(16).key).toBe("iron");
    expect(bandOf(20).key).toBe("iron");
    expect(bandOf(21).key).toBe("gold");
    expect(bandOf(25).key).toBe("gold");
    expect(bandOf(26).key).toBe("diamond");
    expect(bandOf(30).key).toBe("diamond");
    expect(bandOf(31).key).toBe("netherite");
    expect(bandOf(35).key).toBe("netherite");
    expect(bandOf(36).key).toBe("netherStar");
    expect(bandOf(40).key).toBe("netherStar");
    expect(bandOf(41).key).toBe("dragonEgg");
    expect(bandOf(99).key).toBe("dragonEgg");
  });

  it("flags the dragon-egg tier as the ceiling", () => {
    expect(isMaxTitle(41)).toBe(true);
    expect(isMaxTitle(40)).toBe(false);
    expect(isMaxTitle(1)).toBe(false);
  });
});
