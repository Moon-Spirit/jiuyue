import { describe, expect, it } from "vitest";
import { formatLastSeen, presenceLabel } from "./presence-text";

/** A fixed "now" so every boundary below is deterministic. */
const NOW = 1_750_000_000_000;

describe("formatLastSeen", () => {
  it("reads as just now under a minute", () => {
    expect(formatLastSeen(NOW - 5_000, NOW)).toBe("刚刚");
  });

  it("reads in minutes under an hour", () => {
    expect(formatLastSeen(NOW - 5 * 60_000, NOW)).toBe("5 分钟前");
  });

  it("reads in hours under a day", () => {
    expect(formatLastSeen(NOW - 3 * 3_600_000, NOW)).toBe("3 小时前");
  });

  it("reads in days under a week", () => {
    expect(formatLastSeen(NOW - 2 * 86_400_000, NOW)).toBe("2 天前");
  });

  it("falls back to an absolute date beyond a week", () => {
    // The exact date depends on the runner's zone; the shape must not.
    expect(formatLastSeen(NOW - 30 * 86_400_000, NOW)).toMatch(
      /^\d{4}-\d{2}-\d{2}$/,
    );
  });

  it("clamps a clock that has drifted ahead to just now", () => {
    expect(formatLastSeen(NOW + 60_000, NOW)).toBe("刚刚");
  });
});

describe("presenceLabel", () => {
  it("says nothing about a User whose presence is unknown", () => {
    expect(presenceLabel(null, null, NOW)).toBeNull();
  });

  it("says 在线 for a reachable User, with no instant", () => {
    expect(presenceLabel("online", null, NOW)).toBe("在线");
  });

  it("shows the last-seen time for an offline User", () => {
    expect(presenceLabel("offline", NOW - 5 * 60_000, NOW)).toBe(
      "最后在线 5 分钟前",
    );
  });

  it("says 离线 when an offline User has never been seen", () => {
    expect(presenceLabel("offline", null, NOW)).toBe("离线");
  });
});
