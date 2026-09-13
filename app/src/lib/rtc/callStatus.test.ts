import { describe, expect, it } from "vitest";

import { CALL_STATUSES, callStatusText } from "./callStatus";
import type { TranslateFn } from "./callStatus";

/** Fake translator: returns the key (+ params) so assertions stay locale-free. */
const t: TranslateFn = (key, params) =>
  params === undefined ? key : `${key}:${JSON.stringify(params)}`;

describe("callStatusText", () => {
  it("returns a NON-EMPTY label for every status the store can hold", () => {
    for (const status of CALL_STATUSES) {
      const label = callStatusText(status, t);
      expect(label.length).toBeGreaterThan(0);
      // The callee path in particular must never blank the status line.
      expect(label.trim().length).toBeGreaterThan(0);
    }
  });

  it("maps each status to its localized key", () => {
    expect(callStatusText("outgoing", t)).toBe("call.ringing");
    expect(callStatusText("incoming", t)).toBe("call.incoming");
    expect(callStatusText("connecting", t)).toBe("call.connecting");
    expect(callStatusText("active", t)).toBe("call.inCall");
    expect(callStatusText("ended", t)).toBe("call.toastEnded");
    expect(callStatusText("idle", t)).toBe("call.inCall");
  });

  it("uses the participant count for an active GROUP call", () => {
    const label = callStatusText("active", t, {
      isGroup: true,
      participantCount: 3,
    });
    expect(label).toContain("call.participants");
    expect(label).toContain("3");
  });

  it("defaults the group count and stays non-empty without context", () => {
    const label = callStatusText("active", t, { isGroup: true });
    expect(label).toContain("call.participants");
    expect(label).toContain("1");
  });

  it("enumerates the full store union (exhaustiveness guard)", () => {
    expect([...CALL_STATUSES].sort()).toEqual(
      ["active", "connecting", "ended", "idle", "incoming", "outgoing"].sort(),
    );
  });
});
