import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ConversationSummary } from "../generated/ConversationSummary";
import type { GroupInfo } from "../generated/GroupInfo";
import type { MemberView } from "../generated/MemberView";
import type { MembershipChange } from "../generated/MembershipChange";
import type { Role } from "../generated/Role";
import { FakeWebSocket } from "../testing/fake-websocket";
import { useAuthStore } from "./auth";
import { useChatStore } from "./chat";
import { useRealtimeStore } from "./realtime";

const ACCESS_KEY = "jiuyue.auth.access_token";
const GROUP_ID = "01JABC1234567890ABCDEFGHJ2";
const SELF_ID = "01JABC1234567890ABCDEFGHJ1";
const BOB_ID = "01JABC1234567890ABCDEFGHJ3";
const CAROL_ID = "01JABC1234567890ABCDEFGHJ4";

/** A Group Conversation summary as the backend returns it. */
function groupSummary(myRole: Role, memberCount: number): ConversationSummary {
  return {
    id: GROUP_ID,
    kind: "group",
    peer: null,
    group: { title: "九月小组", member_count: memberCount, my_role: myRole },
    unread_count: 0,
    created_at_ms: 1_700_000_000_000,
  };
}

/** One member as the backend returns them. */
function member(
  user_id: string,
  username: string,
  role: Role,
  display_name = username,
): MemberView {
  return {
    user_id,
    username,
    display_name,
    avatar_url: null,
    role,
    joined_at_ms: 1_700_000_000_000,
  };
}

/** The three-member group the store is seeded with. */
function threeMembers(): MemberView[] {
  return [
    member(SELF_ID, "alice", "owner", "Alice"),
    member(BOB_ID, "bob", "member", "Bob"),
    member(CAROL_ID, "carol", "member", "Carol"),
  ];
}

function groupInfo(myRole: Role, members: MemberView[]): GroupInfo {
  return {
    conversation: groupSummary(myRole, members.length),
    members,
  };
}

function envelope(sequence: number, event: unknown): string {
  return JSON.stringify({ v: 1, s: sequence, ts: 1_700_000_000_000, e: event });
}

function membershipEnvelope(
  sequence: number,
  change: MembershipChange,
): string {
  return envelope(sequence, {
    t: "MembershipChanged",
    d: { conversation_id: GROUP_ID, actor_id: SELF_ID, change },
  });
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

type Responder = () => Response;

/** Substitute `fetch` with a queue of responders per URL (the HTTP boundary). */
function stubFetch(
  routes: Record<string, readonly Responder[]>,
): ReturnType<typeof vi.fn<typeof fetch>> {
  const queues: Record<string, Responder[]> = {};
  for (const [url, responders] of Object.entries(routes)) {
    queues[url] = [...responders];
  }

  const mock = vi.fn<typeof fetch>(async (input) => {
    const url = String(input);
    const responder = queues[url]?.shift();
    if (responder === undefined) {
      throw new Error(`unexpected request: ${url}`);
    }
    return responder();
  });

  vi.stubGlobal("fetch", mock);
  return mock;
}

function signIn(): void {
  useAuthStore().user = {
    id: SELF_ID,
    username: "alice",
    email: "alice@example.com",
    display_name: "Alice",
    avatar_url: null,
    email_verified: false,
    created_at_ms: 1_700_000_000_000,
  };
}

/** Connect the realtime store and return the fake socket, already open. */
function openSocket(): FakeWebSocket {
  const realtime = useRealtimeStore();
  realtime.connect();

  const socket = FakeWebSocket.latest();
  socket.emitOpen();
  return socket;
}

/** Bring the store to "a three-member group is open", through the real API path. */
async function openedGroup() {
  stubFetch({
    "/api/conversations/group": [() => jsonResponse(groupSummary("owner", 3))],
    [`/api/conversations/${GROUP_ID}`]: [
      () => jsonResponse(groupInfo("owner", threeMembers())),
    ],
  });

  const store = useChatStore();
  await store.createGroup("九月小组", ["bob", "carol"]);
  return store;
}

describe("useChatStore group membership", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    FakeWebSocket.reset();
    vi.stubGlobal("WebSocket", FakeWebSocket);
    window.localStorage.setItem(ACCESS_KEY, "test-access");
    signIn();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    window.localStorage.clear();
  });

  it("creates a group and seeds the info panel from the server", async () => {
    const store = await openedGroup();

    expect(store.conversations).toHaveLength(1);
    expect(store.conversations[0]?.group?.title).toBe("九月小组");
    expect(store.activeConversationId).toBe(GROUP_ID);
    expect(store.groupMembers[GROUP_ID]?.map((entry) => entry.role)).toEqual([
      "owner",
      "member",
      "member",
    ]);
  });

  it("adds a member and bumps the count when someone joins", async () => {
    const store = await openedGroup();
    const socket = openSocket();

    socket.emitMessage(
      membershipEnvelope(2, {
        type: "joined",
        member: member("01JABC1234567890ABCDEFGHJ5", "dave", "member", "Dave"),
      }),
    );

    expect(store.groupMembers[GROUP_ID]).toHaveLength(4);
    expect(store.conversations[0]?.group?.member_count).toBe(4);
  });

  it("drops the conversation when the current user is removed", async () => {
    const store = await openedGroup();
    const socket = openSocket();

    socket.emitMessage(
      membershipEnvelope(2, { type: "removed", user_id: SELF_ID }),
    );

    expect(store.conversations).toHaveLength(0);
    expect(store.activeConversationId).toBeNull();
    expect(store.groupMembers[GROUP_ID]).toBeUndefined();
  });

  it("updates both the member list and my_role when my role changes", async () => {
    const store = await openedGroup();
    const socket = openSocket();

    socket.emitMessage(
      membershipEnvelope(2, {
        type: "role_changed",
        user_id: SELF_ID,
        role: "admin",
      }),
    );

    const self = store.groupMembers[GROUP_ID]?.find(
      (entry) => entry.user_id === SELF_ID,
    );
    expect(self?.role).toBe("admin");
    expect(store.conversations[0]?.group?.my_role).toBe("admin");
  });

  it("moves owner to admin and the target to owner on a transfer", async () => {
    const store = await openedGroup();
    const socket = openSocket();

    socket.emitMessage(
      membershipEnvelope(2, {
        type: "ownership_transferred",
        from_user_id: SELF_ID,
        to_user_id: BOB_ID,
      }),
    );

    const members = store.groupMembers[GROUP_ID] ?? [];
    expect(members.find((entry) => entry.user_id === SELF_ID)?.role).toBe(
      "admin",
    );
    expect(members.find((entry) => entry.user_id === BOB_ID)?.role).toBe(
      "owner",
    );
    expect(store.conversations[0]?.group?.my_role).toBe("admin");
  });

  it("drops the conversation for everyone when it is dissolved", async () => {
    const store = await openedGroup();
    const socket = openSocket();

    socket.emitMessage(membershipEnvelope(2, { type: "dissolved" }));

    expect(store.conversations).toHaveLength(0);
    expect(store.activeConversationId).toBeNull();
  });

  it("removes the leaving member from the panel and the count", async () => {
    const store = await openedGroup();
    const socket = openSocket();

    socket.emitMessage(
      membershipEnvelope(2, { type: "left", user_id: CAROL_ID }),
    );

    expect(store.groupMembers[GROUP_ID]).toHaveLength(2);
    expect(
      store.groupMembers[GROUP_ID]?.some((entry) => entry.user_id === CAROL_ID),
    ).toBe(false);
    expect(store.conversations[0]?.group?.member_count).toBe(2);
  });

  it("adopts the refreshed panel after an invite", async () => {
    const store = await openedGroup();
    const dave = member("01JABC1234567890ABCDEFGHJ5", "dave", "member", "Dave");

    stubFetch({
      [`/api/conversations/${GROUP_ID}/members`]: [
        () => jsonResponse(groupInfo("owner", [...threeMembers(), dave])),
      ],
    });

    const invited = await store.inviteMembers(GROUP_ID, ["dave"]);

    expect(invited).toBe(true);
    expect(store.groupMembers[GROUP_ID]).toHaveLength(4);
    expect(store.conversations[0]?.group?.member_count).toBe(4);
  });

  it("drops the conversation when the current user leaves", async () => {
    const store = await openedGroup();
    stubFetch({
      [`/api/conversations/${GROUP_ID}/leave`]: [
        () => new Response(null, { status: 204 }),
      ],
    });

    const left = await store.leaveGroup(GROUP_ID);

    expect(left).toBe(true);
    expect(store.conversations).toHaveLength(0);
    expect(store.activeConversationId).toBeNull();
  });

  it("loads a group's panel on open, and only once", async () => {
    const store = useChatStore();
    stubFetch({
      "/api/conversations": [
        () => jsonResponse({ conversations: [groupSummary("owner", 3)] }),
      ],
      [`/api/conversations/${GROUP_ID}`]: [
        () => jsonResponse(groupInfo("owner", threeMembers())),
      ],
      [`/api/conversations/${GROUP_ID}/messages`]: [
        () => jsonResponse({ messages: [] }),
      ],
    });

    await store.loadConversations();
    await store.openConversation(GROUP_ID);

    expect(store.groupMembers[GROUP_ID]).toHaveLength(3);

    // The second open must not refetch: the queue for the info route is empty,
    // so an extra request would make `stubFetch` throw.
    await store.openConversation(GROUP_ID);
    expect(store.groupMembers[GROUP_ID]).toHaveLength(3);
  });
});
