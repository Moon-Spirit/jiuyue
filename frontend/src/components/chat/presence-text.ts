import type { PresenceStatus } from "../../generated/PresenceStatus";

/**
 * Compile-time exhaustiveness check, matching the reducer stores.
 *
 * Runtime no-op: a status a newer server adds is rendered as nothing rather than
 * thrown, which is what keeps the contract additive (ADR-0003). The build still
 * fails, because the new variant stops being `never` here.
 */
function assertExhaustive(_variant: never): void {}

/** Zero-pad a date part so the absolute form is stable across locales. */
function pad(value: number): string {
  return String(value).padStart(2, "0");
}

/**
 * Render the instant a User was last reachable as a short relative time.
 *
 * Pure and clock-injected, so the boundaries are unit-tested rather than inferred
 * from the current time. Anything under a week is relative; beyond that an
 * absolute date is more useful than "23 天前", and a clock that has drifted (a
 * `lastSeen` in the future) is clamped to "刚刚" rather than rendering a negative
 * age.
 */
export function formatLastSeen(lastSeenMs: number, nowMs: number): string {
  const elapsedMs = Math.max(0, nowMs - lastSeenMs);
  const minutes = Math.floor(elapsedMs / 60_000);
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;

  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;

  const days = Math.floor(hours / 24);
  if (days < 7) return `${days} 天前`;

  const at = new Date(lastSeenMs);
  return `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())}`;
}

/**
 * The one line a presence badge shows, or `null` when there is nothing to say.
 *
 * `online` needs no instant (the present is not a past), `offline` shows when the
 * User was last reachable when that is known, and an unknown User — or a status a
 * newer server adds — shows nothing at all.
 */
export function presenceLabel(
  status: PresenceStatus | null,
  lastSeenMs: number | null,
  nowMs: number,
): string | null {
  switch (status) {
    case "online":
      return "在线";
    case "offline":
      return lastSeenMs === null
        ? "离线"
        : `最后在线 ${formatLastSeen(lastSeenMs, nowMs)}`;
    case null:
      return null;
    default:
      assertExhaustive(status);
      return null;
  }
}
