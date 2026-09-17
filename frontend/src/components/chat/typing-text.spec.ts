import { describe, expect, it } from "vitest";
import { typingLabel } from "./typing-text";

describe("typingLabel", () => {
  it("has nothing to say about an empty list", () => {
    expect(typingLabel([])).toBeNull();
    expect(typingLabel(["", ""])).toBeNull();
  });

  it("names a single Participant", () => {
    expect(typingLabel(["Alice"])).toBe("Alice 正在输入…");
  });

  it("names up to two Participants", () => {
    expect(typingLabel(["Alice", "Bob"])).toBe("Alice、Bob 正在输入…");
  });

  it("summarises more than two by count", () => {
    expect(typingLabel(["Alice", "Bob", "Carol"])).toBe("3 人正在输入…");
  });

  it("de-duplicates a repeated name", () => {
    expect(typingLabel(["Alice", "Alice"])).toBe("Alice 正在输入…");
  });
});
