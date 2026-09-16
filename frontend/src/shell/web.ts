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
 * Query-parameter names the browser call window is opened with.
 *
 * A popup is a separate document, so the request travels in the URL instead of
 * an event. The Tauri shell delivers the same request through
 * {@link DesktopShell.onCallWindowOpen} instead; either way the call surface
 * learns its Conversation through the interface.
 */
export const CALL_WINDOW_QUERY = {
  conversationId: "shell_call",
  kind: "call_kind",
} as const;

const CALL_WINDOW_FEATURES = "popup=yes,width=960,height=640";

/**
 * The Badging API is not in every engine, and not in every `lib.dom`; feature
 * detection through a structural type keeps the call honest without a cast.
 */
interface BadgeHost {
  setAppBadge?: (contents?: number) => Promise<void>;
  clearAppBadge?: () => Promise<void>;
}

/** The Permissions API surface used here, narrow enough to stay cast-free. */
interface PermissionHost {
  query(descriptor: { name: string }): Promise<{ state: string }>;
}

function labelFor(request: CallWindowRequest): string {
  return `call-${request.conversationId}-${request.kind}`;
}

function callUrl(request: CallWindowRequest): string {
  const base = import.meta.env.BASE_URL || "/";
  const url = new URL(base, window.location.origin);
  url.searchParams.set(
    CALL_WINDOW_QUERY.conversationId,
    request.conversationId,
  );
  url.searchParams.set(CALL_WINDOW_QUERY.kind, request.kind);
  return url.toString();
}

function notificationState(): PermissionState {
  if (typeof Notification === "undefined") return "unsupported";
  switch (Notification.permission) {
    case "granted":
      return "granted";
    case "denied":
      return "denied";
    default:
      return "prompt";
  }
}

function mediaHost(): PermissionHost | undefined {
  if (typeof navigator === "undefined") return undefined;
  return navigator.permissions;
}

async function queryMediaPermission(kind: string): Promise<PermissionState> {
  const host = mediaHost();
  if (host === undefined) return "prompt";
  try {
    const status = await host.query({ name: kind });
    switch (status.state) {
      case "granted":
        return "granted";
      case "denied":
        return "denied";
      default:
        return "prompt";
    }
  } catch {
    // An engine that does not know this permission name is not a denial: the
    // prompt is still ahead of us at capture time.
    return "prompt";
  }
}

/**
 * The plain-browser implementation.
 *
 * It is not a stub: the browser is a real host with real capabilities. What it
 * cannot do — an OS-level tray, an OS-level picker for capture sources — is
 * reported as exactly that, so a caller never mistakes a browser for a shell.
 */
export function createWebShell(): DesktopShell {
  const windows = new Map<string, Window>();
  const clickListeners = new Set<(conversationId: string) => void>();

  function emitClick(conversationId: string): void {
    for (const listener of clickListeners) listener(conversationId);
  }

  async function openCallWindow(
    request: CallWindowRequest,
  ): Promise<CallWindowHandle | null> {
    const label = labelFor(request);
    const existing = windows.get(label);
    if (existing !== undefined && !existing.closed) {
      existing.focus();
      return { label };
    }

    const opened = window.open(callUrl(request), label, CALL_WINDOW_FEATURES);
    if (opened === null) return null;
    windows.set(label, opened);
    return { label };
  }

  return {
    kind: "web",

    async startCall(request: CallWindowRequest): Promise<CallStartResult> {
      const handle = await openCallWindow(request);
      return {
        window: handle,
        failure:
          handle === null ? "浏览器拦截了通话窗口，请允许弹出窗口" : null,
      };
    },

    openCallWindow,

    async closeCallWindow(handle: CallWindowHandle): Promise<boolean> {
      const opened = windows.get(handle.label);
      if (opened === undefined) return false;
      opened.close();
      windows.delete(handle.label);
      return true;
    },

    onCallWindowOpen(
      listener: (request: CallWindowRequest) => void,
    ): Unsubscribe {
      // A popup navigates to its own URL, so the request is already in the query
      // string when the listener attaches; deliver it without waiting for an
      // event that will never come on this host.
      const params = new URLSearchParams(window.location.search);
      const conversationId = params.get(CALL_WINDOW_QUERY.conversationId);
      if (conversationId !== null) {
        const kind =
          params.get(CALL_WINDOW_QUERY.kind) === "video" ? "video" : "audio";
        queueMicrotask(() => listener({ conversationId, kind }));
      }
      return () => {};
    },

    async pickScreenShareSource(
      _tier: ScreenShareTier,
    ): Promise<ScreenShareSelection | null> {
      // The browser owns the picker: the user's choice arrives through
      // `getDisplayMedia`, so the shell has nothing to select on their behalf.
      return null;
    },

    async requestMediaPermission(
      request: MediaPermissionRequest,
    ): Promise<MediaPermissionResult> {
      return {
        microphone: request.kinds.includes("microphone")
          ? await queryMediaPermission("microphone")
          : "unsupported",
        camera: request.kinds.includes("camera")
          ? await queryMediaPermission("camera")
          : "unsupported",
      };
    },

    async notificationPermission(): Promise<PermissionState> {
      return notificationState();
    },

    async requestNotificationPermission(): Promise<PermissionState> {
      if (typeof Notification === "undefined") return "unsupported";
      const result = await Notification.requestPermission();
      return result === "default" ? "prompt" : result;
    },

    async notify(notification: ShellNotification): Promise<boolean> {
      if (notificationState() !== "granted") return false;
      const shown = new Notification(notification.title, {
        body: notification.body,
      });
      shown.onclick = () => {
        window.focus();
        emitClick(notification.conversationId);
      };
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
      const host: BadgeHost | undefined =
        typeof navigator === "undefined" ? undefined : navigator;
      if (host === undefined) return;

      if (state.total > 0) {
        await host.setAppBadge?.(state.total);
        return;
      }
      await host.clearAppBadge?.();
    },
  };
}
