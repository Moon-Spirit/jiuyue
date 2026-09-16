/**
 * The `desktop-shell` capability interface.
 *
 * The application talks to *this*, never to a platform API. Windows and macOS
 * ship a Tauri v2 shell, Linux a separate Electron shell (ADR-0008); this
 * interface is what keeps the same frontend bundle valid in both, and what makes
 * a plain browser (a shell-less host) a first-class implementation rather than a
 * degraded one.
 *
 * **Invariant: a component, view, store or router file must never import a
 * shell-specific package.** `shell/guard.ts` enforces it mechanically and
 * `shell/guard.spec.ts` runs that guard against the real source tree.
 *
 * Contract of the seam:
 *
 * - Every method is capability-shaped, not platform-shaped. There is no
 *   `invokeTauri`, no `getCurrentWindow`, no `WebviewWindow` in the surface.
 * - Calls that need the OS window manager return a {@link CallWindowHandle}, not
 *   a platform window object; the handle is opaque to the caller.
 * - A capability a host cannot provide resolves to an explicit result (a refused
 *   notification, a `null` window, an `"unsupported"` permission) — never a
 *   silent no-op the caller cannot distinguish from success.
 */

/** Which shell implementation is installed. */
export type ShellKind = "web" | "tauri";

/** Detaches a listener registered through `on*`. Idempotent. */
export type Unsubscribe = () => void;

/** The two Call shapes the product supports (CONTEXT.md: 通话). */
export type CallKind = "audio" | "video";

/**
 * Which Call to open a window for.
 *
 * `conversationId` is the ULID of the Conversation and is what the call surface
 * uses to join; signalling itself is ticket #30, not this seam.
 */
export interface CallWindowRequest {
  readonly conversationId: string;
  readonly kind: CallKind;
}

/** An opaque reference to an open Call window. */
export interface CallWindowHandle {
  /** Stable host-side label; also the key for {@link DesktopShell.closeCallWindow}. */
  readonly label: string;
}

/**
 * The result of asking to begin a Call.
 *
 * A `null` window is a real failure with a reason, so the caller can tell the
 * user instead of waiting for a window that will never appear.
 */
export interface CallStartResult {
  readonly window: CallWindowHandle | null;
  /** Human-readable failure, or `null` when the window opened. */
  readonly failure: string | null;
}

/** Screen-share resolution tiers the product offers (AGENTS.md). */
export type ScreenShareResolution = "720p" | "1080p" | "1440p" | "2160p";

/** Screen-share frame-rate tiers the product offers (AGENTS.md). */
export type ScreenShareFrameRate = 60 | 120 | 144 | 165 | 180 | 240 | 360;

/** The requested capture tier. */
export interface ScreenShareTier {
  readonly resolution: ScreenShareResolution;
  readonly frameRate: ScreenShareFrameRate;
}

/**
 * A capture source the user picked through the shell's own picker.
 *
 * `tier` is what was actually granted. A grant below what was asked arrives with
 * a non-null {@link ScreenShareSelection.downgradeReason}: silent downgrade is
 * forbidden (AGENTS.md), so the caller must be able to surface it.
 */
export interface ScreenShareSelection {
  readonly sourceId: string;
  readonly label: string;
  readonly thumbnailDataUrl: string | null;
  readonly tier: ScreenShareTier;
  readonly downgradeReason: string | null;
}

/** A permission as the host reports it. */
export type PermissionState = "granted" | "denied" | "prompt" | "unsupported";

/** Microphone / camera, the two permissions a Call needs. */
export type MediaPermissionKind = "microphone" | "camera";

/** What a call surface asks the host about before joining. */
export interface MediaPermissionRequest {
  readonly conversationId: string;
  readonly kinds: readonly MediaPermissionKind[];
}

/** The host's report for each requested media permission kind. */
export interface MediaPermissionResult {
  readonly microphone: PermissionState;
  readonly camera: PermissionState;
}

/** A native notification for one incoming Message. */
export interface ShellNotification {
  readonly conversationId: string;
  /** Usually the sender's display name. */
  readonly title: string;
  readonly body: string;
}

/** The account's unread state, as the tray renders it. */
export interface UnreadState {
  /** Total across every Conversation — the number on the tray. */
  readonly total: number;
}

/**
 * Every desktop capability the product has, as one interface.
 *
 * The web implementation ({@link ./web.ts}) is the default the browser gets; the
 * Tauri implementation ({@link ./tauri.ts}) is one shell's adapter. A future
 * Electron shell implements the same surface without a single component change.
 */
export interface DesktopShell {
  readonly kind: ShellKind;

  // ---- Calls (media itself is #30; this is the window/permission seam) ----

  /**
   * Begin a Call: open the dedicated OS window, focusing it if it already exists.
   *
   * A Call **must** run in a window of its own, never as an in-app overlay: that
   * is a hard product requirement, so the seam only ever returns a window.
   */
  startCall(request: CallWindowRequest): Promise<CallStartResult>;

  /**
   * Open (or focus) the Call window for a Conversation without signalling.
   *
   * Returns `null` when the host refused to open a window.
   */
  openCallWindow(request: CallWindowRequest): Promise<CallWindowHandle | null>;

  /** Close a Call window previously opened through this shell. */
  closeCallWindow(handle: CallWindowHandle): Promise<boolean>;

  /**
   * Subscribe to "this Call window was opened for X", as received by the Call
   * surface itself.
   *
   * The window is a separate document, so it learns its Conversation from the
   * shell rather than from a shared store instance.
   */
  onCallWindowOpen(listener: (request: CallWindowRequest) => void): Unsubscribe;

  // ---- Screen sharing ----

  /**
   * Ask the host to choose a screen-share source at the requested tier.
   *
   * `null` means the **host has its own picker and the caller should proceed**:
   * browsers and WebView2 consume the choice inside `getDisplayMedia`, and on
   * Linux the choice is made by the system portal (ADR-0008), which the
   * application must not replace with an in-app grid.
   */
  pickScreenShareSource(
    tier: ScreenShareTier,
  ): Promise<ScreenShareSelection | null>;

  // ---- Media permissions ----

  /**
   * Report the microphone/camera permission state for a Call.
   *
   * Reporting only. The prompt itself is raised by the capture API at
   * `getUserMedia` time; the desktop shells own the window-level permission
   * callback that makes that prompt appear (ADR-0007).
   */
  requestMediaPermission(
    request: MediaPermissionRequest,
  ): Promise<MediaPermissionResult>;

  // ---- Notifications ----

  /** The current notification permission, without prompting. */
  notificationPermission(): Promise<PermissionState>;

  /** Prompt for notification permission if not yet decided. */
  requestNotificationPermission(): Promise<PermissionState>;

  /**
   * Show a native notification.
   *
   * Returns whether it was shown; a refusal (permission denied, unsupported
   * host) is `false`, never an error the caller has to guess at.
   */
  notify(notification: ShellNotification): Promise<boolean>;

  /** Whether the application window currently has the user's focus. */
  isWindowFocused(): boolean;

  /** Subscribe to a notification being activated, by Conversation id. */
  onNotificationClick(listener: (conversationId: string) => void): Unsubscribe;

  // ---- Tray / badge ----

  /** Reflect the account's unread total on the tray (and badge, where present). */
  setUnread(state: UnreadState): Promise<void>;
}
