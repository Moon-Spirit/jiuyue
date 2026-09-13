/**
 * Pure call-status → label mapping (M14).
 *
 * Kept out of the component so every possible {@link CallStatus} value can be
 * unit-tested for a NON-EMPTY label — a blank status line (seen on the callee
 * path) is a regression the UI must never reintroduce.
 */

/** Mirrors the call store's state machine. Keep in sync with stores/call.ts. */
export type CallStatus =
  "idle" | "outgoing" | "incoming" | "connecting" | "active" | "ended";

/** Every status the store can hold, for exhaustive tests. */
export const CALL_STATUSES: readonly CallStatus[] = [
  "idle",
  "outgoing",
  "incoming",
  "connecting",
  "active",
  "ended",
] as const;

/** Minimal translator shape (compatible with vue-i18n's `t`). */
export type TranslateFn = (
  key: string,
  params?: Record<string, unknown>,
) => string;

export interface CallStatusContext {
  /** Group call vs 1:1 (only affects the `active` label). */
  isGroup?: boolean;
  /** Roster size for the group `active` label. */
  participantCount?: number;
}

/**
 * Human label for a call status. Never returns an empty string: every branch —
 * including unrecognised future values — falls back to a localized label.
 */
export function callStatusText(
  status: CallStatus,
  t: TranslateFn,
  context: CallStatusContext = {},
): string {
  switch (status) {
    case "outgoing":
      return t("call.ringing");
    case "incoming":
      return t("call.incoming");
    case "connecting":
      return t("call.connecting");
    case "active":
      return context.isGroup === true
        ? t("call.participants", { n: context.participantCount ?? 1 })
        : t("call.inCall");
    case "ended":
      return t("call.toastEnded");
    case "idle":
      return t("call.inCall");
    default: {
      // Exhaustiveness guard: a value added to CallStatus without a branch
      // fails type-check here, and an unexpected runtime value still renders.
      const exhausted: never = status;
      void exhausted;
      return t("call.inCall");
    }
  }
}
