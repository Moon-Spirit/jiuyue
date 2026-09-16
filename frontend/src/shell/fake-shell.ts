import { vi } from "vitest";
import type {
  CallStartResult,
  CallWindowHandle,
  CallWindowRequest,
  DesktopShell,
  ScreenShareSelection,
} from "./types";

function base(): DesktopShell {
  const handle: CallWindowHandle = { label: "call-c1-audio" };
  const started: CallStartResult = { window: handle, failure: null };
  const selection: ScreenShareSelection | null = null;

  return {
    kind: "web",
    startCall: vi.fn(async (_request: CallWindowRequest) => started),
    openCallWindow: vi.fn(async (_request: CallWindowRequest) => handle),
    closeCallWindow: vi.fn(async (_handle: CallWindowHandle) => true),
    onCallWindowOpen: vi.fn(
      (_listener: (request: CallWindowRequest) => void) => () => {},
    ),
    pickScreenShareSource: vi.fn(async () => selection),
    requestMediaPermission: vi.fn(async () => ({
      microphone: "prompt" as const,
      camera: "prompt" as const,
    })),
    notificationPermission: vi.fn(async () => "granted" as const),
    requestNotificationPermission: vi.fn(async () => "granted" as const),
    notify: vi.fn(async () => true),
    isWindowFocused: vi.fn(() => true),
    onNotificationClick: vi.fn(
      (_listener: (conversationId: string) => void) => () => {},
    ),
    setUnread: vi.fn(async () => {}),
  };
}

/**
 * A `DesktopShell` whose every method is a spy.
 *
 * Tests that exercise the notification policy or the tray reporter assert on
 * *what the shell was asked to do*, which is the whole contract of the seam.
 */
export function createFakeShell(
  overrides: Partial<DesktopShell> = {},
): DesktopShell {
  return Object.assign(base(), overrides);
}
