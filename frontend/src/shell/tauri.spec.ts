import type { Event } from "@tauri-apps/api/event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: vi.fn(),
  requestPermission: vi.fn(),
  sendNotification: vi.fn(),
}));

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { desktopShell, installDetectedShell, resetDesktopShell } from "./index";
import { CALL_OPEN_EVENT, createTauriShell } from "./tauri";
import type { CallWindowRequest } from "./types";

beforeEach(() => {
  vi.mocked(listen).mockResolvedValue(() => {});
  vi.mocked(isPermissionGranted).mockResolvedValue(true);
  vi.mocked(requestPermission).mockResolvedValue("granted");
});

afterEach(() => {
  vi.clearAllMocks();
  vi.unstubAllGlobals();
  resetDesktopShell();
});

describe("the Tauri shell", () => {
  it("reports itself as the Tauri shell and watches window focus", async () => {
    const shell = await createTauriShell();

    expect(shell.kind).toBe("tauri");
    expect(listen).toHaveBeenCalledWith("tauri://focus", expect.any(Function));
  });

  it("maps openCallWindow onto the Rust command as a camelCase request", async () => {
    vi.mocked(invoke).mockResolvedValue("call-c1-video");
    const shell = await createTauriShell();

    const handle = await shell.openCallWindow({
      conversationId: "c1",
      kind: "video",
    });

    expect(invoke).toHaveBeenCalledWith("open_call_window", {
      request: { conversationId: "c1", kind: "video" },
    });
    expect(handle).toEqual({ label: "call-c1-video" });
  });

  it("returns no window when the Rust command refuses", async () => {
    vi.mocked(invoke).mockRejectedValue(new Error("window creation failed"));
    const shell = await createTauriShell();

    expect(
      await shell.openCallWindow({ conversationId: "c1", kind: "audio" }),
    ).toBeNull();
  });

  it("closes a Call window by label", async () => {
    vi.mocked(invoke).mockResolvedValue(true);
    const shell = await createTauriShell();

    expect(await shell.closeCallWindow({ label: "call-c1-audio" })).toBe(true);
    expect(invoke).toHaveBeenCalledWith("close_call_window", {
      label: "call-c1-audio",
    });
  });

  it("reports the unread total to the tray", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    const shell = await createTauriShell();

    await shell.setUnread({ total: 5 });

    expect(invoke).toHaveBeenCalledWith("set_unread", { count: 5 });
  });

  it("shows a native notification and routes its activation to the Conversation", async () => {
    const focusHandlers: ((event: Event<unknown>) => void)[] = [];
    vi.mocked(listen).mockImplementation(async (event, handler) => {
      if (event === "tauri://focus") focusHandlers.push(handler);
      return () => {};
    });

    const shell = await createTauriShell();
    const clicks: string[] = [];
    shell.onNotificationClick((conversationId) => clicks.push(conversationId));

    const shown = await shell.notify({
      conversationId: "c7",
      title: "小明",
      body: "在吗",
    });

    expect(shown).toBe(true);
    expect(sendNotification).toHaveBeenCalledWith({
      title: "小明",
      body: "在吗",
    });

    for (const handler of focusHandlers) {
      handler({ event: "tauri://focus", id: 1, payload: null });
    }
    expect(clicks).toEqual(["c7"]);

    // The click is consumed: a later refocus is not a second activation.
    for (const handler of focusHandlers) {
      handler({ event: "tauri://focus", id: 1, payload: null });
    }
    expect(clicks).toEqual(["c7"]);
  });

  it("refuses to notify without permission", async () => {
    vi.mocked(isPermissionGranted).mockResolvedValue(false);
    const shell = await createTauriShell();

    const shown = await shell.notify({
      conversationId: "c7",
      title: "小明",
      body: "在吗",
    });

    expect(shown).toBe(false);
    expect(sendNotification).not.toHaveBeenCalled();
  });

  it("reports permission through the plugin", async () => {
    const shell = await createTauriShell();

    expect(await shell.notificationPermission()).toBe("granted");

    vi.mocked(isPermissionGranted).mockResolvedValue(false);
    vi.mocked(requestPermission).mockResolvedValue("denied");

    expect(await shell.notificationPermission()).toBe("prompt");
    expect(await shell.requestNotificationPermission()).toBe("denied");
  });

  it("subscribes the Call surface to the call-open event", async () => {
    const handlers = new Map<
      string,
      (event: Event<CallWindowRequest>) => void
    >();
    vi.mocked(listen).mockImplementation(async (event, handler) => {
      handlers.set(event, handler);
      return () => {};
    });

    const shell = await createTauriShell();
    const seen: CallWindowRequest[] = [];
    shell.onCallWindowOpen((request) => seen.push(request));

    const deliver = handlers.get(CALL_OPEN_EVENT);
    expect(deliver).toBeDefined();
    deliver?.({
      event: CALL_OPEN_EVENT,
      id: 1,
      payload: { conversationId: "c1", kind: "video" },
    });

    expect(seen).toEqual([{ conversationId: "c1", kind: "video" }]);
  });

  it("leaves screen-share selection to the webview's own picker", async () => {
    const shell = await createTauriShell();

    expect(
      await shell.pickScreenShareSource({
        resolution: "1440p",
        frameRate: 120,
      }),
    ).toBeNull();
  });

  it("is installed by detection when the webview global is present", async () => {
    vi.stubGlobal("__TAURI_INTERNALS__", {});

    const installed = await installDetectedShell();

    expect(installed.kind).toBe("tauri");
    expect(desktopShell().kind).toBe("tauri");
  });
});
