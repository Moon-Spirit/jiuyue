import { defineStore } from "pinia";
import { ref } from "vue";
import { ApiError, apiGetAuthed } from "../api/client";
import type { Presence } from "../generated/Presence";
import type { PresenceList } from "../generated/PresenceList";
import type { PresenceStatus } from "../generated/PresenceStatus";
import { useAuthStore } from "./auth";
import { useRealtimeStore } from "./realtime";

/**
 * Presence (CONTEXT.md: 在线状态) — who is reachable, and when they last were.
 *
 * One map keyed by **User** id, fed from two directions:
 *
 * - the socket, through `onPresenceEvent`, for a change as it happens. The server
 *   only sends that to Participants of a Conversation shared with the User, so a
 *   client is never told about a stranger;
 * - `GET /presence`, for the current state of the peers a view is about to render.
 *   A live event only exists *when something changes*, so the read is what a
 *   freshly opened screen uses.
 *
 * The state is scoped to the User, not to a Device: a User is online while any of
 * their Devices is, and the server owns that decision. This store never derives
 * presence from a socket of its own.
 *
 * An entry is **replaced**, never merged: an `online` event carries no last-seen
 * instant and an `offline` one carries the instant the User stopped being
 * reachable, so the newest event is the whole truth for that User.
 */
export const usePresenceStore = defineStore("presence", () => {
  /** The known presence of each User, by User id. Absent means "not known". */
  const presences = ref<Record<string, Presence>>({});
  const loading = ref(false);
  const errorMessage = ref<string | null>(null);

  /** Adopt one presence, from the socket or from a REST read. Idempotent. */
  function apply(presence: Presence): void {
    presences.value[presence.user_id] = presence;
  }

  /** The last known status of a User, or `null` when nothing is known yet. */
  function statusFor(userId: string): PresenceStatus | null {
    return presences.value[userId]?.status ?? null;
  }

  /** When the User was last reachable, or `null` while online or unknown. */
  function lastSeenFor(userId: string): number | null {
    return presences.value[userId]?.last_seen_ms ?? null;
  }

  /**
   * Load the current presence of the named Users.
   *
   * Blank ids and duplicates are dropped, and a request with nothing to ask about
   * is not sent at all — the server refuses an empty list, and "nobody to ask
   * about" is not a failure. A User the caller shares no Conversation with is
   * absent from the answer rather than reported offline; this store simply keeps
   * whatever it already knew about them.
   */
  async function load(userIds: readonly string[]): Promise<void> {
    const token = useAuthStore().accessToken;
    if (token === null) return;

    const ids = [...new Set(userIds.filter((userId) => userId !== ""))];
    if (ids.length === 0) return;

    loading.value = true;
    errorMessage.value = null;
    try {
      const query = encodeURIComponent(ids.join(","));
      const payload = await apiGetAuthed<PresenceList>(
        `/presence?user_ids=${query}`,
        token,
      );
      for (const presence of payload.presences) apply(presence);
    } catch (cause) {
      errorMessage.value =
        cause instanceof ApiError
          ? (cause.body?.error.message ?? `请求失败（HTTP ${cause.status}）`)
          : "无法加载在线状态";
    } finally {
      loading.value = false;
    }
  }

  /** Forget every entry; used when the session ends so nothing leaks across accounts. */
  function clear(): void {
    presences.value = {};
  }

  // One subscription for the store's lifetime: the realtime store forwards only
  // presence changes here, and this store is what knows what they mean.
  useRealtimeStore().onPresenceEvent(apply);

  return {
    presences,
    loading,
    errorMessage,
    apply,
    statusFor,
    lastSeenFor,
    load,
    clear,
  };
});
