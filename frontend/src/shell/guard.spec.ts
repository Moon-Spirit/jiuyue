import { describe, expect, it } from "vitest";
import { findDirectShellImports } from "./guard";

/**
 * Every source file under `src/`, as text.
 *
 * Vite resolves the glob at transform time, so this needs no `node:fs` — the app
 * tsconfig deliberately keeps Node types out of the browser scope. Glob keys
 * arrive relative to *this* file (`src/shell/`); {@link asSrcPath} turns them back
 * into paths the guard can reason about.
 */
const rawSources: Record<string, string> = import.meta.glob("../**/*", {
  query: "?raw",
  import: "default",
  eager: true,
});

function asSrcPath(key: string): string {
  if (key.startsWith("./")) return `src/shell/${key.slice(2)}`;
  if (key.startsWith("../")) return `src/${key.slice(3)}`;
  return key;
}

const sources: Record<string, string> = Object.fromEntries(
  Object.entries(rawSources).map(([key, text]) => [asSrcPath(key), text]),
);

function report(
  violations: readonly { file: string; specifier: string }[],
): string {
  return violations
    .map((violation) => `${violation.file} → ${violation.specifier}`)
    .join("\n");
}

describe("the no-direct-shell-API guard", () => {
  it("actually loaded the source tree", () => {
    expect(Object.keys(sources).length).toBeGreaterThan(10);
    // Both sides of the exemption must be present, or the scan below would pass
    // vacuously by never seeing a non-shell file.
    expect(Object.keys(sources)).toContain("src/main.ts");
    expect(Object.keys(sources)).toContain("src/shell/guard.ts");
  });

  it("finds no shell-specific import or shell-injected global outside frontend/src/shell", () => {
    const violations = findDirectShellImports(sources);
    expect(violations, report(violations)).toEqual([]);
  });

  it("flags a component importing a Tauri API", () => {
    const violations = findDirectShellImports({
      "src/components/chat/MessageList.vue": [
        '<script setup lang="ts">',
        'import { invoke } from "@tauri-apps/api/core";',
        "</script>",
      ].join("\n"),
    });

    expect(violations).toHaveLength(1);
    expect(violations[0]?.file).toBe("src/components/chat/MessageList.vue");
    expect(violations[0]?.specifier).toBe("@tauri-apps/api/core");
  });

  it("flags a dynamically imported plugin", () => {
    const violations = findDirectShellImports({
      "src/stores/chat.ts":
        'const notification = await import("@tauri-apps/plugin-notification");',
    });

    expect(violations.map((violation) => violation.specifier)).toEqual([
      "@tauri-apps/plugin-notification",
    ]);
  });

  it("flags an Electron import, because the same rule covers the second shell", () => {
    const violations = findDirectShellImports({
      "src/views/ChatView.vue": 'import { ipcRenderer } from "electron";',
    });

    expect(violations.map((violation) => violation.specifier)).toEqual([
      "electron",
    ]);
  });

  it("flags reading a shell-injected global", () => {
    const violations = findDirectShellImports({
      "src/main.ts": "const tauri = window.__TAURI_INTERNALS__;",
    });

    expect(violations.map((violation) => violation.specifier)).toEqual([
      "__TAURI_INTERNALS__",
    ]);
  });

  it("exempts the shell implementation itself", () => {
    const violations = findDirectShellImports({
      "src/shell/tauri.ts": 'import { invoke } from "@tauri-apps/api/core";',
    });

    expect(violations).toEqual([]);
  });

  it("does not confuse import.meta.glob for an import specifier", () => {
    const violations = findDirectShellImports({
      "src/App.vue": 'const mods = import.meta.glob("./views/*.vue");',
    });

    expect(violations).toEqual([]);
  });
});
