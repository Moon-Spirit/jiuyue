import { afterEach, describe, expect, it } from "vitest";
import {
  desktopShell,
  installDesktopShell,
  installDetectedShell,
  resetDesktopShell,
} from "./index";
import { createFakeShell } from "./fake-shell";

afterEach(() => {
  resetDesktopShell();
});

describe("the desktop-shell seam", () => {
  it("answers with the browser shell before anything is installed", () => {
    expect(desktopShell().kind).toBe("web");
  });

  it("replaces the installed shell when one is registered", () => {
    const fake = createFakeShell({ kind: "tauri" });

    installDesktopShell(fake);

    expect(desktopShell()).toBe(fake);
  });

  it("installs the browser shell when no Tauri webview is present", async () => {
    const installed = await installDetectedShell();

    expect(installed.kind).toBe("web");
    expect(desktopShell()).toBe(installed);
  });
});
