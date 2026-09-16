import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "../testing/fake-websocket";
import { usePresenceStore } from "./presence";
import { useRealtimeStore } from "./realtime";

const ACCESS_KEY = "jiuyue.auth.access_token";
const PEER_ID = "01JABC1234567890ABCDEFGHJ3";

/** One presence envelope as the server pushes it, produced from the contract. */
function presenceEnvelope(
  status: "online" | "offline",
  lastSeenMs: number | null,
  sequence: number,
): string {
  return JSON.stringify({
    v: 1,
    s: sequence,
    ts: 1_750_000_000_000,
    e: {
      t: "Presence",
      d: { user_id: PEER_ID, status, last_seen_ms: lastSeenMs },
    },
  });
}

/** A JSON response body, as `fetch` would return it. */
function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("usePresenceStore", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    FakeWebSocket.reset();
    vi.stubGlobal("WebSocket", FakeWebSocket);
    window.localStorage.setItem(ACCESS_KEY, "test-access");
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  it("knows nothing about anyone before anything arrives", () => {
    const presence = usePresenceStore();

    expect(presence.statusFor(PEER_ID)).toBeNull();
    expect(presence.lastSeenFor(PEER_ID)).toBeNull();
  });

  it("records a reachable User, who has no last-seen instant", () => {
    usePresenceStore().apply({
      user_id: PEER_ID,
      status: "online",
      last_seen_ms: null,
    });

    expect(usePresenceStore().statusFor(PEER_ID)).toBe("online");
    expect(usePresenceStore().lastSeenFor(PEER_ID)).toBeNull();
  });

  it("replaces the entry when the User goes offline, keeping the instant", () => {
    const presence = usePresenceStore();
    presence.apply({
      user_id: PEER_ID,
      status: "online",
      last_seen_ms: null,
    });

    presence.apply({
      user_id: PEER_ID,
      status: "offline",
      last_seen_ms: 1_750_000_000_000,
    });

    // Replacement, not a merge: the offline event is the whole truth, and the
    // instant it carries is what a peer renders.
    expect(presence.statusFor(PEER_ID)).toBe("offline");
    expect(presence.lastSeenFor(PEER_ID)).toBe(1_750_000_000_000);
  });

  it("adopts a presence change pushed over the socket", () => {
    const presence = usePresenceStore();
    useRealtimeStore().connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    socket.emitMessage(presenceEnvelope("online", null, 1));

    expect(presence.statusFor(PEER_ID)).toBe("online");
  });

  it("tracks a peer going offline and then online again, live", () => {
    const presence = usePresenceStore();
    useRealtimeStore().connect();
    const socket = FakeWebSocket.latest();
    socket.emitOpen();

    socket.emitMessage(presenceEnvelope("online", null, 1));
    socket.emitMessage(presenceEnvelope("offline", 1_750_000_000_000, 2));
    expect(presence.statusFor(PEER_ID)).toBe("offline");
    expect(presence.lastSeenFor(PEER_ID)).toBe(1_750_000_000_000);

    socket.emitMessage(presenceEnvelope("online", null, 3));
    expect(presence.statusFor(PEER_ID)).toBe("online");
    expect(presence.lastSeenFor(PEER_ID)).toBeNull();
  });

  it("loads the current presence of the named peers, de-duplicated", async () => {
    const fetchMock = vi.fn<typeof fetch>(async () =>
      jsonResponse({
        presences: [
          {
            user_id: PEER_ID,
            status: "offline",
            last_seen_ms: 1_750_000_000_000,
          },
        ],
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const presence = usePresenceStore();
    await presence.load([PEER_ID, PEER_ID, ""]);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const requested = String(fetchMock.mock.calls[0]?.[0]);
    expect(requested).toContain(
      `/presence?user_ids=${encodeURIComponent(PEER_ID)}`,
    );
    expect(presence.statusFor(PEER_ID)).toBe("offline");
    expect(presence.lastSeenFor(PEER_ID)).toBe(1_750_000_000_000);
  });

  it("does not ask the server when there is nobody to ask about", async () => {
    const fetchMock = vi.fn<typeof fetch>(async () =>
      jsonResponse({ presences: [] }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await usePresenceStore().load(["", ""]);

    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("keeps what it knew about a User the answer omits", async () => {
    const fetchMock = vi.fn<typeof fetch>(async () =>
      jsonResponse({ presences: [] }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const presence = usePresenceStore();
    presence.apply({
      user_id: PEER_ID,
      status: "offline",
      last_seen_ms: 1_750_000_000_000,
    });

    // A User outside the caller's audience is omitted rather than reported
    // offline, so an empty answer must not erase a live event already applied.
    await presence.load([PEER_ID]);

    expect(presence.statusFor(PEER_ID)).toBe("offline");
    expect(presence.lastSeenFor(PEER_ID)).toBe(1_750_000_000_000);
  });

  it("forgets everyone when the session ends", () => {
    const presence = usePresenceStore();
    presence.apply({
      user_id: PEER_ID,
      status: "online",
      last_seen_ms: null,
    });

    presence.clear();

    expect(presence.statusFor(PEER_ID)).toBeNull();
    expect(Object.keys(presence.presences)).toHaveLength(0);
  });
});
