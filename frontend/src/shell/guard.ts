/**
 * The mechanical enforcement of the seam.
 *
 * `AGENTS.md` says a component must never call a shell-specific API directly, and
 * ADR-0008 says that rule is what keeps the Linux Electron shell able to reuse
 * this frontend. A convention does not survive a deadline, so the rule is a pure
 * function over source text, and a test runs it against the real tree.
 *
 * This module scans text, not the AST, which is deliberate: it also catches a
 * dynamic `import()`, a `require`, a side-effect import, and a platform global,
 * all of which an import-restriction lint rule can miss.
 */

/** A file that reaches around the `desktop-shell` interface. */
export interface Violation {
  /** Repository-relative path, with forward slashes. */
  readonly file: string;
  /** What was found: an import specifier, or the platform global's name. */
  readonly specifier: string;
  /** Why it is a violation, phrased for the failure message. */
  readonly reason: string;
}

/**
 * Package specifiers that only a shell implementation may import.
 *
 * Matched as a *prefix*, so `@tauri-apps/api/core`,
 * `@tauri-apps/plugin-notification` and any future plugin are all covered by one
 * entry — a new plugin must not require remembering to extend a list.
 */
export const RESTRICTED_SPECIFIERS: readonly string[] = [
  "@tauri-apps/",
  "tauri-plugin-",
  "electron",
  "@electron/",
];

/** Globals a Tauri (or Electron) webview injects, which only the shell may read. */
export const RESTRICTED_GLOBALS: readonly string[] = [
  "__TAURI__",
  "__TAURI_INTERNALS__",
];

/** Directory (as a path segment) whose files are the implementation of the seam. */
export const SHELL_SEGMENT = "/shell/";

/**
 * `from "x"`, `import("x")`, `require("x")` and the side-effect `import "x"`.
 *
 * The leading alternation requires a quote to follow, so unrelated uses of the
 * word `import` (e.g. `import.meta.glob`) do not match.
 */
const SPECIFIER_PATTERN =
  /(?:\bfrom\s*|\bimport\s*\(?\s*|\brequire\s*\(\s*)["']([^"']+)["']/g;

export interface GuardOptions {
  /**
   * Path segments whose files are exempt because they *are* the interface.
   *
   * Defaults to {@link SHELL_SEGMENT}; the value stays a parameter so the guard
   * is testable against a synthetic tree without a special case.
   */
  readonly exemptSegments?: readonly string[];
}

function isRestrictedSpecifier(specifier: string): boolean {
  return RESTRICTED_SPECIFIERS.some(
    (restricted) =>
      specifier === restricted || specifier.startsWith(restricted),
  );
}

function scanFile(file: string, source: string): Violation[] {
  const violations: Violation[] = [];

  for (const match of source.matchAll(SPECIFIER_PATTERN)) {
    const specifier = match[1];
    if (specifier !== undefined && isRestrictedSpecifier(specifier)) {
      violations.push({
        file,
        specifier,
        reason:
          "imports a shell-specific package; every desktop capability must go through the desktop-shell interface",
      });
    }
  }

  for (const global of RESTRICTED_GLOBALS) {
    if (source.includes(global)) {
      violations.push({
        file,
        specifier: global,
        reason:
          "reads a shell-injected global; use desktopShell() from frontend/src/shell instead",
      });
    }
  }

  return violations;
}

/**
 * Find every reach-around of the `desktop-shell` interface.
 *
 * `files` maps a path to its source text, so the caller decides where the tree
 * comes from (the real `src` tree in the test, a fixture in a unit test).
 */
export function findDirectShellImports(
  files: Readonly<Record<string, string>>,
  options: GuardOptions = {},
): Violation[] {
  const exempt = options.exemptSegments ?? [SHELL_SEGMENT];
  const violations: Violation[] = [];

  for (const [rawPath, source] of Object.entries(files)) {
    const file = rawPath.replaceAll("\\", "/");
    if (exempt.some((segment) => file.includes(segment))) continue;
    violations.push(...scanFile(file, source));
  }

  return violations;
}
