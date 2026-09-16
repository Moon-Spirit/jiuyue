import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

import type {
  CallStartResult,
  CallWindowHandle,
  CallWindowRequest,
  DesktopShell,
  MediaPermissionRequest,
  MediaPermissionResult,
  PermissionState,
  ScreenShareSelection,
  ScreenShareTier,
  ShellNotification,
  UnreadState,
  Unsubscribe,
} from "./types";

/**
 * Rust command names.
 *
 * One place, so the seam and the crate cannot drift silently: a rename on either
 * side shows up as a failing call, not as a notification that never appears.
 */
const COMMANDS = {
  openCallWindow: "open_call_window",
  closeCallWindow: "close_call_window",
  setUnread: "set_unread",
} as const;

/**
 * Event the Rust side emits to the Call window's own document, carrying the
 * {@link CallWindowRequest} it was opened for.
 */
export const CALL_OPEN_EVENT = "shell://call-open";

/**
 * The plugin reports `"default"` where the interface says `"prompt"` — one
 * vocabulary for the whole application, mapped at the adapter.
 */
function toPermissionState(state: string): PermissionState {
  if (state === "granted" || state === "denied") return state;
  return "prompt";
}

/**
 * The Tauri v2 implementation (Windows / macOS — ADR-0008).
 *
 * Every capability maps onto either a Rust command (window and tray work, which
 * belongs in the native process) or an official plugin (notifications). It is
 * loaded dynamically by `installDetectedShell`, so the browser bundle never
 * carries it.
 */
export async function createTauriShell(): Promise<DesktopShell> {
  const mediaPermission: PermissionState = "prompt";
  const clickListeners = new Set<(conversationId: string) => void>();

  /**
   * The Conversation of the most recent notification.
   *
   * The desktop shells expose no reliable "notification activated" callback, so
   * the click is inferred from the window regaining focus: on Windows a toast
   * click activates the app and nothing else does. It is cleared once delivered,
   * so a later alt-tab is not mistaken for a click.
   */
  let lastNotifiedConversationId: string | null = null;

  function emitClick(conversationId: string): void {
    for (const listener of clickListeners) listener(conversationId);
  }

  async function openCallWindow(
    request: CallWindowRequest,
  ): Promise<CallWindowHandle | null> {
    try {
      const label = await invoke<string>(COMMANDS.openCallWindow, {
        request: {
          conversationId: request.conversationId,
          kind: request.kind,
        },
      });
      return { label };
    } catch {
      return null;
    }
  }

  await listen("tauri://focus", () => {
    const conversationId = lastNotifiedConversationId;
    if (conversationId === null) return;
    lastNotifiedConversationId = null;
    emitClick(conversationId);
  });

  return {
    kind: "tauri",

    async startCall(request: CallWindowRequest): Promise<CallStartResult> {
      const handle = await openCallWindow(request);
      return {
        window: handle,
        failure: handle === null ? "无法打开通话窗口" : null,
      };
    },

    openCallWindow,

    async closeCallWindow(handle: CallWindowHandle): Promise<boolean> {
      try {
        return await invoke<boolean>(COMMANDS.closeCallWindow, {
          label: handle.label,
        });
      } catch {
        return false;
      }
    },

    onCallWindowOpen(
      listener: (request: CallWindowRequest) => void,
    ): Unsubscribe {
      let unlisten: UnlistenFn | null = null;
      let cancelled = false;

      void listen<CallWindowRequest>(CALL_OPEN_EVENT, (event) => {
        listener(event.payload);
      })
        .then((detach) => {
          if (cancelled) detach();
          else unlisten = detach;
        })
        .catch(() => {
          // A failed subscription is not a crash: the Call window simply never
          // learns its Conversation, and the caller's timeout path applies.
        });

      return () => {
        cancelled = true;
        unlisten?.();
      };
    },

    async pickScreenShareSource(
      _tier: ScreenShareTier,
    ): Promise<ScreenShareSelection | null> {
      // WebView2 shows its own picker inside `getDisplayMedia`, so the shell has
      // nothing to select on the user's behalf. The capability exists for the
      // Linux shell, whose portal picker is the one the UX must target.
      return null;
    },

    async requestMediaPermission(
      _request: MediaPermissionRequest,
    ): Promise<MediaPermissionResult> {
      // Reporting only: the prompt is raised by `getUserMedia` at capture time.
      // Making the WebView2 permission callback explicit is ticket #30 (ADR-0007).
      return { microphone: mediaPermission, camera: mediaPermission };
    },

    async notificationPermission(): Promise<PermissionState> {
      return (await isPermissionGranted()) ? "granted" : "prompt";
    },

    async requestNotificationPermission(): Promise<PermissionState> {
      return toPermissionState(await requestPermission());
    },

    async notify(notification: ShellNotification): Promise<boolean> {
      if (!(await isPermissionGranted())) return false;

      sendNotification({
        title: notification.title,
        body: notification.body,
      });
      lastNotifiedConversationId = notification.conversationId;
      return true;
    },

    isWindowFocused(): boolean {
      return document.hasFocus();
    },

    onNotificationClick(
      listener: (conversationId: string) => void,
    ): Unsubscribe {
      clickListeners.add(listener);
      return () => {
        clickListeners.delete(listener);
      };
    },

    async setUnread(state: UnreadState): Promise<void> {
      try {
        await invoke(COMMANDS.setUnread, { count: state.total });
      } catch {
        // The tray is an affordance, not the data: a failed write must not
        // interrupt the caller that is merely reporting unread state.
      }
    },
  };
}
