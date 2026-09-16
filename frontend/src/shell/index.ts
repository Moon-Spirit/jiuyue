import type { DesktopShell } from "./types";
import { createWebShell } from "./web";

/**
 * The installed shell.
 *
 * Defaulted to the browser implementation so the module has a valid answer from
 * the first import: no capability call ever has to null-check "is a shell
 * installed yet".
 */
let current: DesktopShell = createWebShell();

/**
 * The shell the application should talk to.
 *
 * This is the single entry point every component, store and view uses. No caller
 * may import a platform package (`@tauri-apps/*`, `electron`, ...) directly —
 * `shell/guard.ts` fails the test suite if one does.
 */
export function desktopShell(): DesktopShell {
  return current;
}

/**
 * Register a shell implementation.
 *
 * The composition root calls this once (through {@link installDetectedShell}, or
 * directly when a host needs an explicit implementation); tests call it to
 * substitute a fake.
 */
export function installDesktopShell(shell: DesktopShell): void {
  current = shell;
}

/** Restore the browser default. For tests. */
export function resetDesktopShell(): void {
  current = createWebShell();
}

/**
 * Whether this document is inside a Tauri webview.
 *
 * Deliberately a property check rather than an import: importing a Tauri package
 * here would pull shell-specific code into the browser bundle, which is the very
 * coupling this seam exists to prevent.
 */
function isTauriHost(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * Detect the host and install the matching shell.
 *
 * The wiring the composition root adds is this one call:
 *
 * ```ts
 * import { installDetectedShell } from "./shell";
 * void installDetectedShell();
 * ```
 *
 * The Tauri implementation is imported dynamically, so the browser bundle only
 * ever ships the web implementation; a second shell (the Linux Electron one,
 * ADR-0008) installs itself by calling {@link installDesktopShell} directly
 * before mounting, without this function changing.
 */
export async function installDetectedShell(): Promise<DesktopShell> {
  if (!isTauriHost()) {
    current = createWebShell();
    return current;
  }

  const { createTauriShell } = await import("./tauri");
  current = await createTauriShell();
  return current;
}

export type {
  CallKind,
  CallStartResult,
  CallWindowHandle,
  CallWindowRequest,
  DesktopShell,
  MediaPermissionKind,
  MediaPermissionRequest,
  MediaPermissionResult,
  PermissionState,
  ScreenShareFrameRate,
  ScreenShareResolution,
  ScreenShareSelection,
  ScreenShareTier,
  ShellKind,
  ShellNotification,
  UnreadState,
  Unsubscribe,
} from "./types";
export { createWebShell } from "./web";
export { CALL_WINDOW_QUERY } from "./web";
export {
  createMessageNotifier,
  presentMessageNotification,
  shouldNotify,
} from "./notifications";
export type {
  MessageNotificationInput,
  MessageNotifierDeps,
  NotificationContext,
} from "./notifications";
export { bindUnreadToTray, totalUnread } from "./unread";
