import { afterEach, describe, expect, it, vi } from "vitest";
import { createWebShell } from "./web";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  window.history.replaceState({}, "", "/");
});

describe("the browser shell", () => {
  it("reports itself as the web shell", () => {
    expect(createWebShell().kind).toBe("web");
  });

  it("opens a Call in a popup window and returns an opaque handle", async () => {
    const popup = { closed: false, focus: vi.fn(), close: vi.fn() };
    let requestedUrl = "";
    const open = vi.fn((url?: string | URL) => {
      requestedUrl = String(url);
      return popup;
    });
    vi.stubGlobal("open", open);

    const handle = await createWebShell().openCallWindow({
      conversationId: "c1",
      kind: "video",
    });

    expect(handle).toEqual({ label: "call-c1-video" });
    expect(open).toHaveBeenCalledTimes(1);
    expect(requestedUrl).toContain("shell_call=c1");
  });

  it("refuses a Call when the browser blocks the popup", async () => {
    vi.stubGlobal(
      "open",
      vi.fn(() => null),
    );

    const shell = createWebShell();
    const result = await shell.startCall({
      conversationId: "c1",
      kind: "audio",
    });

    expect(result.window).toBeNull();
    expect(result.failure).not.toBeNull();
  });

  it("delivers the Call request to the popup that opened with it", async () => {
    window.history.replaceState({}, "", "/?shell_call=c9&call_kind=video");

    const seen: unknown[] = [];
    createWebShell().onCallWindowOpen((request) => {
      seen.push(request);
    });

    await vi.waitFor(() => {
      expect(seen).toEqual([{ conversationId: "c9", kind: "video" }]);
    });
  });

  it("leaves screen-share selection to the browser's own picker", async () => {
    const selection = await createWebShell().pickScreenShareSource({
      resolution: "1080p",
      frameRate: 60,
    });

    expect(selection).toBeNull();
  });

  it("reports a notification refusal instead of throwing when unsupported", async () => {
    vi.stubGlobal("Notification", undefined);

    const shell = createWebShell();

    expect(await shell.notificationPermission()).toBe("unsupported");
    expect(await shell.requestNotificationPermission()).toBe("unsupported");
    expect(
      await shell.notify({ conversationId: "c1", title: "t", body: "b" }),
    ).toBe(false);
  });

  it("shows a notification only once permission is granted", async () => {
    vi.stubGlobal("focus", vi.fn());

    class FakeNotification {
      static instances: FakeNotification[] = [];
      static permission = "granted";
      static requestPermission = vi.fn(async () => "granted" as const);

      readonly title: string;
      readonly options: { body?: string } | undefined;
      onclick: (() => void) | null = null;

      constructor(title: string, options?: { body?: string }) {
        this.title = title;
        this.options = options;
        FakeNotification.instances.push(this);
      }

      close(): void {}
    }
    vi.stubGlobal("Notification", FakeNotification);

    const shell = createWebShell();
    const clicks: string[] = [];
    shell.onNotificationClick((conversationId) => clicks.push(conversationId));

    expect(
      await shell.notify({ conversationId: "c7", title: "小明", body: "在吗" }),
    ).toBe(true);
    expect(FakeNotification.instances).toHaveLength(1);
    expect(FakeNotification.instances[0]?.title).toBe("小明");

    FakeNotification.instances[0]?.onclick?.();
    expect(clicks).toEqual(["c7"]);
  });

  it("writes the unread total to the app badge when the engine has one", async () => {
    const setAppBadge = vi.fn(async () => {});
    const clearAppBadge = vi.fn(async () => {});
    vi.stubGlobal("navigator", { setAppBadge, clearAppBadge });

    const shell = createWebShell();
    await shell.setUnread({ total: 4 });
    await shell.setUnread({ total: 0 });

    expect(setAppBadge).toHaveBeenCalledWith(4);
    expect(clearAppBadge).toHaveBeenCalledTimes(1);
  });
});
