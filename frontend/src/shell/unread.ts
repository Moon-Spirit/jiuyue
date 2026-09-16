import { toValue, watchEffect, type WatchSource } from "vue";
import type { DesktopShell, Unsubscribe } from "./types";

/**
 * The account's total unread count, which is what the tray shows.
 *
 * Per-Conversation counts live in the chat store; the tray only ever needs their
 * sum, so the seam takes the sum rather than the map.
 */
export function totalUnread(counts: Readonly<Record<string, number>>): number {
  let total = 0;
  for (const count of Object.values(counts)) total += count;
  return total;
}

/**
 * Keep the tray's unread state equal to `source`.
 *
 * Reactive on purpose: the tray is a projection of the store, not a copy that a
 * caller has to remember to refresh at each of the places unread changes.
 * Returns the unsubscribe.
 */
export function bindUnreadToTray(
  shell: DesktopShell,
  source: WatchSource<number>,
): Unsubscribe {
  return watchEffect(() => {
    // The promise is deliberately not awaited: the tray is a projection of UI
    // state, and a failed write must not stall the reactive effect.
    void shell.setUnread({ total: toValue(source) });
  });
}
