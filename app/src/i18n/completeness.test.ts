import { describe, expect, it } from "vitest";
import en from "./locales/en";
import zhCN from "./locales/zh-CN";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function collectKeys(node: unknown, prefix = ""): string[] {
  if (!isRecord(node)) return [prefix];
  return Object.entries(node).flatMap(([key, value]) =>
    collectKeys(value, prefix.length === 0 ? key : `${prefix}.${key}`),
  );
}

function collectLeaves(node: unknown): string[] {
  if (!isRecord(node)) return typeof node === "string" ? [node] : [];
  return Object.values(node).flatMap(collectLeaves);
}

describe("i18n locale completeness", () => {
  it("zh-CN and en expose identical key sets", () => {
    expect(collectKeys(en).sort()).toEqual(collectKeys(zhCN).sort());
  });

  it("every message leaf is a non-empty string in both locales", () => {
    for (const locale of [zhCN, en]) {
      const leaves = collectLeaves(locale);
      expect(leaves.length).toBeGreaterThan(0);
      for (const leaf of leaves) {
        expect(typeof leaf).toBe("string");
        expect(leaf.length).toBeGreaterThan(0);
      }
    }
  });
});
