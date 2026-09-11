import { describe, expect, it } from "vitest";
import { EMOJIS } from "./emoji";

describe("emoji catalog", () => {
  it("exposes a non-trivial, duplicate-free grid of single tokens", () => {
    expect(EMOJIS.length).toBeGreaterThanOrEqual(60);
    // Duplicates would render twice in the picker and skew the grid.
    expect(new Set(EMOJIS).size).toBe(EMOJIS.length);
    for (const emoji of EMOJIS) {
      expect(typeof emoji).toBe("string");
      expect(emoji.length).toBeGreaterThan(0);
      // Whitespace inside an entry would break the tap-to-insert contract.
      expect(emoji).not.toMatch(/\s/);
    }
  });

  it("mixes several categories (faces, hands, hearts, objects)", () => {
    expect(EMOJIS).toContain("😀"); // face
    expect(EMOJIS).toContain("👍"); // hand
    expect(EMOJIS).toContain("❤️"); // heart
    expect(EMOJIS).toContain("🎉"); // object
  });
});
