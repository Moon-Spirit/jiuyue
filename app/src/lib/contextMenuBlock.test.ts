import { afterEach, describe, expect, it } from "vitest";
import { installContextMenuBlock } from "./contextMenuBlock";

describe("右键拦截 — context menu interception", () => {
  let cleanup: (() => void) | null = null;

  afterEach(() => {
    cleanup?.();
    cleanup = null;
  });

  it("suppresses the native context menu app-wide", () => {
    cleanup = installContextMenuBlock();

    const event = new MouseEvent("contextmenu", {
      bubbles: true,
      cancelable: true,
    });
    document.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
  });

  it("restores the default behavior after cleanup", () => {
    const dispose = installContextMenuBlock();
    dispose();

    const event = new MouseEvent("contextmenu", {
      bubbles: true,
      cancelable: true,
    });
    document.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(false);
  });

  it("blocks right-click on nested elements, not just the document", () => {
    cleanup = installContextMenuBlock();

    const child = document.createElement("div");
    document.body.appendChild(child);
    try {
      const event = new MouseEvent("contextmenu", {
        bubbles: true,
        cancelable: true,
      });
      child.dispatchEvent(event);
      expect(event.defaultPrevented).toBe(true);
    } finally {
      child.remove();
    }
  });
});
