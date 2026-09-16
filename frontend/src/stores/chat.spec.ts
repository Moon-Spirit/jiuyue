import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "../testing/fake-websocket";
import { useAuthStore } from "./auth";
import { useChatStore } from "./chat";
import { useRealtimeStore } from "./realtime";

const ACCESS_KEY = "jiuyue.auth.access_token";
const CONVERSATION_ID = "01JABC1234567890ABCDEFGHJ2";
const PEER_ID = "01JABC1234567890ABCDEFGHJ3";
const SELF_ID = "01JABC1234567890ABCDEFGHJ1";

/** A `ConversationSummary` as the backend returns it. */
function conversationPayload(): Record<string, unknown> {
  return {
    id: CONVERSATION_ID,
    kind: "direct",
    peer: {
      id: PEER_ID,
      username: "bob",
      display_name: "Bob",
      avatar_url: null,
    },
    unread_count: 0,
    created_at_ms: 1_700_000_000_000,
  };
}

/** A `MessageView` as the backend returns it. */
function messagePayload(
  overrides: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    id: "01JABC1234567890ABCDEFGHJ4",
    conversation_id: CONVERSATION_ID,
    seq: 1,
    sender_id: SELF_ID,
    client_msg_id: "default-key",
    body: "hello",
    created_at_ms: 1_700_000_000_000,
    ...overrides,
  };
}

function envelope(sequence: number, event: unknown): string {
  return JSON.stringify({ v: 1, s: sequence, ts: 1_700_000_000_000, e: event });
}

function ackEnvelope(
  clientMsgId: string,
  message: Record<string, unknown>,
): string {
  return envelope(2, {
    t: "MessageAck",
    d: { client_msg_id: clientMsgId, message },
  });
}

function newMessageEnvelope(message: Record<string, unknown>): string {
  return envelope(3, { t: "NewMessage", d: { message } });
}

function rejectionEnvelope(
  clientMsgId: string,
  code: string,
  message: string,
): string {
  return envelope(4, {
    t: "MessageRejected",
    d: { client_msg_id: clientMsgId, code, message },
  });
}

function conversationCreatedEnvelope(
  conversation: Record<string, unknown>,
): string {
  return envelope(5, { t: "ConversationCreated", d: { conversation } });
}

/** One outgoing frame, parsed to the shape the assertions need. */
interface ClientFrame {
  v: number;
  e: {
    t: string;
    d: {
      client_msg_id?: string;
      conversation_id?: string;
      last_seq?: number;
      last_read_seq?: number;
    };
  };
}

/** The signed-in profile, so the store can tell the user's own Messages apart. */
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

function parseFrame(text: string): ClientFrame {
  return JSON.parse(text) as ClientFrame;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

type Responder = () => Response;

/**
 * Substitute `fetch` with a queue of responders per URL.
 *
 * The boundary mocked is the HTTP one — no store function is stubbed — so the
 * request shape, the error mapping and the state transitions are really exercised.
 */
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

/** Connect the realtime store and return the fake socket, already open. */
function openSocket(): FakeWebSocket {
  const realtime = useRealtimeStore();
  realtime.connect();

  const socket = FakeWebSocket.latest();
  socket.emitOpen();
  return socket;
}

/** The history route for the conversation under test. */
function messagesRoute(): string {
  return `/api/conversations/${CONVERSATION_ID}/messages`;
}

/** Bring the store to "one direct conversation open" through the real API path. */
async function openedStore() {
  stubFetch({
    "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
    [messagesRoute()]: [() => jsonResponse({ messages: [] })],
  });

  const store = useChatStore();
  await store.startDirect("bob");
  return store;
}

describe("useChatStore", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    FakeWebSocket.reset();
    vi.stubGlobal("WebSocket", FakeWebSocket);
    window.localStorage.setItem(ACCESS_KEY, "test-access");
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    window.localStorage.clear();
  });

  it("shows the bubble immediately and reconciles it when the ack arrives", async () => {
    const store = await openedStore();
    const socket = openSocket();

    expect(store.sendMessage("你好")).toBe(true);

    // Optimistic: rendered before the server has seen it.
    expect(store.messages).toHaveLength(1);
    const optimistic = store.messages[0];
    expect(optimistic?.state).toBe("sending");
    expect(optimistic?.seq).toBeNull();

    const clientMsgId = optimistic?.clientMsgId ?? "";
    const frame = parseFrame(socket.sent[0] ?? "{}");
    expect(frame.e.t).toBe("SendMessage");
    expect(frame.e.d.client_msg_id).toBe(clientMsgId);

    socket.emitMessage(
      ackEnvelope(
        clientMsgId,
        messagePayload({
          client_msg_id: clientMsgId,
          id: "SERVER-MESSAGE",
          seq: 1,
        }),
      ),
    );

    // Reconciled in place: one bubble, now carrying the server's identity.
    expect(store.messages).toHaveLength(1);
    expect(store.messages[0]?.state).toBe("sent");
    expect(store.messages[0]?.id).toBe("SERVER-MESSAGE");
    expect(store.messages[0]?.seq).toBe(1);
  });

  it("does not duplicate the bubble when the sender's own echo arrives", async () => {
    const store = await openedStore();
    const socket = openSocket();

    store.sendMessage("你好");
    const clientMsgId = store.messages[0]?.clientMsgId ?? "";

    // The server fans NewMessage out to every Participant, the sender included.
    socket.emitMessage(
      newMessageEnvelope(
        messagePayload({
          client_msg_id: clientMsgId,
          id: "SERVER-MESSAGE",
          seq: 1,
        }),
      ),
    );

    expect(store.messages).toHaveLength(1);
    expect(store.messages[0]?.state).toBe("sent");
    expect(store.messages[0]?.id).toBe("SERVER-MESSAGE");
  });

  it("appends a message the peer sends without a reload", async () => {
    const store = await openedStore();
    const socket = openSocket();

    socket.emitMessage(
      newMessageEnvelope(
        messagePayload({
          id: "PEER-MESSAGE",
          sender_id: PEER_ID,
          client_msg_id: "peer-key",
          body: "来自对方",
        }),
      ),
    );

    expect(store.messages).toHaveLength(1);
    expect(store.messages[0]?.body).toBe("来自对方");
    expect(store.messages[0]?.state).toBe("sent");
    expect(store.messages[0]?.id).toBe("PEER-MESSAGE");
  });

  it("retries with the same idempotency key and never duplicates the bubble", async () => {
    const store = await openedStore();
    const socket = openSocket();

    store.sendMessage("重试我");
    const clientMsgId = store.messages[0]?.clientMsgId ?? "";

    // The server refuses the first attempt.
    socket.emitMessage(
      rejectionEnvelope(clientMsgId, "INTERNAL", "服务器内部错误"),
    );
    expect(store.messages[0]?.state).toBe("failed");

    // The retry reuses the key rather than minting a new one.
    expect(store.retry(clientMsgId)).toBe(true);
    const frames = socket.sent.map((text) => parseFrame(text));
    expect(frames).toHaveLength(2);
    expect(frames[1]?.e.d.client_msg_id).toBe(clientMsgId);

    socket.emitMessage(
      ackEnvelope(
        clientMsgId,
        messagePayload({
          client_msg_id: clientMsgId,
          id: "SERVER-MESSAGE",
          seq: 1,
        }),
      ),
    );

    expect(store.messages).toHaveLength(1);
    expect(store.messages[0]?.state).toBe("sent");
    expect(store.messages[0]?.id).toBe("SERVER-MESSAGE");
  });

  it("marks a send failed when the socket is not open, then recovers on retry", async () => {
    const store = await openedStore();

    // Connected but not open: the frame never leaves.
    useRealtimeStore().connect();

    expect(store.sendMessage("离线消息")).toBe(false);
    expect(store.messages[0]?.state).toBe("failed");
    expect(store.messages[0]?.failure).toBe("连接已断开");

    const clientMsgId = store.messages[0]?.clientMsgId ?? "";
    FakeWebSocket.latest().emitOpen();
    expect(store.retry(clientMsgId)).toBe(true);
    expect(store.messages).toHaveLength(1);
    expect(store.messages[0]?.state).toBe("sending");
  });

  it("adds a conversation pushed by the server", () => {
    const store = useChatStore();
    const socket = openSocket();

    socket.emitMessage(conversationCreatedEnvelope(conversationPayload()));

    expect(store.conversations).toHaveLength(1);
    expect(store.conversations[0]?.id).toBe(CONVERSATION_ID);
  });

  it("loads history when a conversation is opened", async () => {
    stubFetch({
      "/api/conversations": [
        () => jsonResponse({ conversations: [conversationPayload()] }),
      ],
      [messagesRoute()]: [
        () =>
          jsonResponse({
            messages: [messagePayload({ id: "HISTORY-1", body: "旧消息" })],
          }),
      ],
    });

    const store = useChatStore();
    await store.loadConversations();
    await store.openConversation(CONVERSATION_ID);

    expect(store.messages).toHaveLength(1);
    expect(store.messages[0]?.body).toBe("旧消息");
    expect(store.messages[0]?.state).toBe("sent");
  });

  it("prepends an older page and keeps the boundary message exactly once", async () => {
    const route = messagesRoute();
    stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      [route]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M8", client_msg_id: "k8", seq: 8 }),
              messagePayload({ id: "M9", client_msg_id: "k9", seq: 9 }),
              messagePayload({ id: "M10", client_msg_id: "k10", seq: 10 }),
            ],
            next_before: 8,
            has_more: true,
          }),
      ],
      [`${route}?before=8`]: [
        () =>
          jsonResponse({
            // seq 8 deliberately overlaps the already-loaded page: the boundary
            // must deduplicate rather than duplicate the bubble.
            messages: [
              messagePayload({ id: "M6", client_msg_id: "k6", seq: 6 }),
              messagePayload({ id: "M7", client_msg_id: "k7", seq: 7 }),
              messagePayload({ id: "M8", client_msg_id: "k8", seq: 8 }),
            ],
            next_before: 6,
            has_more: true,
          }),
      ],
    });

    const store = useChatStore();
    await store.startDirect("bob");

    expect(store.messages.map((entry) => entry.seq)).toEqual([8, 9, 10]);
    expect(store.hasMoreHistory).toBe(true);

    const loaded = await store.loadOlder();

    expect(loaded).toBe(true);
    expect(store.messages.map((entry) => entry.seq)).toEqual([6, 7, 8, 9, 10]);
    expect(store.messages).toHaveLength(5);
    expect(store.hasMoreHistory).toBe(true);
  });

  it("stops paging once the beginning of the conversation is reached", async () => {
    const route = messagesRoute();
    const fetchMock = stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      [route]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M1", client_msg_id: "k1", seq: 1 }),
            ],
            next_before: null,
            has_more: false,
          }),
      ],
    });

    const store = useChatStore();
    await store.startDirect("bob");

    expect(store.hasMoreHistory).toBe(false);
    expect(await store.loadOlder()).toBe(false);
    // Only the create and the newest page were requested; no older fetch happened.
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("keeps a pending optimistic bubble when an older page is prepended", async () => {
    const route = messagesRoute();
    stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      [route]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M5", client_msg_id: "k5", seq: 5 }),
            ],
            next_before: 5,
            has_more: true,
          }),
      ],
      [`${route}?before=5`]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M4", client_msg_id: "k4", seq: 4 }),
            ],
            next_before: 4,
            has_more: true,
          }),
      ],
    });

    const store = useChatStore();
    await store.startDirect("bob");

    // No socket is open, so the send is marked failed — but its bubble stays.
    expect(store.sendMessage("待发送")).toBe(false);
    const pending = store.messages[1];
    expect(pending?.seq).toBeNull();

    await store.loadOlder();

    expect(store.messages.map((entry) => entry.seq)).toEqual([4, 5, null]);
    expect(store.messages[2]?.clientMsgId).toBe(pending?.clientMsgId);
  });

  it("starts a direct conversation through the API and focuses it", async () => {
    const fetchMock = stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      [messagesRoute()]: [() => jsonResponse({ messages: [] })],
    });

    const store = useChatStore();
    const opened = await store.startDirect("  Bob  ");

    expect(opened).toBe(true);
    expect(store.activeConversationId).toBe(CONVERSATION_ID);
    expect(store.activeConversation?.peer?.username).toBe("bob");

    const body = fetchMock.mock.calls[0]?.[1]?.body;
    expect(typeof body === "string" ? JSON.parse(body) : null).toEqual({
      peer_username: "Bob",
    });
  });

  it("repairs forward from its cursor when the server asks for a resync", async () => {
    const route = messagesRoute();
    stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      [route]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M1", client_msg_id: "k1", seq: 1 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
      "/api/conversations": [
        () => jsonResponse({ conversations: [conversationPayload()] }),
      ],
      [`${route}?after=1&limit=100`]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M2", client_msg_id: "k2", seq: 2 }),
              messagePayload({ id: "M3", client_msg_id: "k3", seq: 3 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
    });

    const store = useChatStore();
    await store.startDirect("bob");
    expect(store.messages.map((message) => message.seq)).toEqual([1]);

    const socket = openSocket();
    // The server says the position is unavailable: repair each Conversation.
    socket.emitMessage(
      envelope(1, { t: "Resync", d: { reason: "unavailable", replayed: 0 } }),
    );

    await vi.waitFor(() => {
      expect(store.messages.map((message) => message.seq)).toEqual([1, 2, 3]);
    });
    expect(store.messages.every((message) => message.state === "sent")).toBe(
      true,
    );
  });

  it("applies a duplicate delivery by Message ID without duplicating the bubble", async () => {
    const store = await openedStore();
    const socket = openSocket();

    const payload = messagePayload({ id: "M2", client_msg_id: "k2", seq: 2 });
    // At-least-once delivery: the same Message may arrive twice.
    socket.emitMessage(newMessageEnvelope(payload));
    socket.emitMessage(newMessageEnvelope(payload));

    expect(store.messages.map((message) => message.seq)).toEqual([2]);
    expect(store.messages).toHaveLength(1);
  });

  it("detects a conversation-level gap and repairs it to an exact set", async () => {
    const route = messagesRoute();
    stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      [route]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M1", client_msg_id: "k1", seq: 1 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
      "/api/conversations": [
        () => jsonResponse({ conversations: [conversationPayload()] }),
      ],
      [`${route}?after=1&limit=100`]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M2", client_msg_id: "k2", seq: 2 }),
              messagePayload({ id: "M3", client_msg_id: "k3", seq: 3 }),
              messagePayload({ id: "M4", client_msg_id: "k4", seq: 4 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
    });

    const store = useChatStore();
    await store.startDirect("bob");
    const socket = openSocket();

    // seq 4 arrives while 2 and 3 are missing: the store must pull them forward.
    socket.emitMessage(
      envelope(2, {
        t: "NewMessage",
        d: {
          message: messagePayload({ id: "M4", client_msg_id: "k4", seq: 4 }),
        },
      }),
    );

    await vi.waitFor(() => {
      expect(store.messages.map((message) => message.seq)).toEqual([
        1, 2, 3, 4,
      ]);
    });
    expect(store.messages).toHaveLength(4);
  });

  it("adopts the Device's stored cursor from SyncState and repairs what was missed", async () => {
    const route = messagesRoute();
    stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      // Opening the Conversation loads an empty page: this is a reloaded client,
      // so nothing is held locally and only the server remembers the position.
      [route]: [
        () =>
          jsonResponse({
            messages: [],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
      "/api/conversations": [
        () => jsonResponse({ conversations: [conversationPayload()] }),
      ],
      [`${route}?after=5&limit=100`]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M6", client_msg_id: "k6", seq: 6 }),
              messagePayload({ id: "M7", client_msg_id: "k7", seq: 7 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
    });

    const store = useChatStore();
    await store.startDirect("bob");
    const socket = openSocket();

    // The server hands this Device back the position it had persisted.
    socket.emitMessage(
      envelope(2, {
        t: "SyncState",
        d: {
          cursors: [{ conversation_id: CONVERSATION_ID, last_seq: 5 }],
        },
      }),
    );

    await vi.waitFor(() => {
      expect(store.messages.map((message) => message.seq)).toEqual([6, 7]);
    });
    expect(store.syncCursors[CONVERSATION_ID]).toBe(5);
    // The cursor-seeded walk leaves only the missed tail, so older history must
    // still be reachable through the backwards cursor.
    expect(store.hasMoreHistory).toBe(true);
  });

  it("reports its consumed position to the server as a per-Device cursor", async () => {
    await openedStore();
    const socket = openSocket();

    socket.emitMessage(
      newMessageEnvelope(
        messagePayload({ id: "M1", client_msg_id: "k1", seq: 1 }),
      ),
    );

    await vi.waitFor(() => {
      const reported = socket.sent
        .map((text) => parseFrame(text))
        .some(
          (frame) =>
            frame.e.t === "SyncCursor" &&
            frame.e.d.conversation_id === CONVERSATION_ID &&
            frame.e.d.last_seq === 1,
        );
      expect(reported).toBe(true);
    });
  });

  it("tolerates a duplicate re-delivered by a cursor repair", async () => {
    const route = messagesRoute();
    stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      [route]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M1", client_msg_id: "k1", seq: 1 }),
              messagePayload({ id: "M2", client_msg_id: "k2", seq: 2 }),
            ],
            next_before: 1,
            next_after: null,
            has_more: false,
          }),
      ],
      "/api/conversations": [
        () => jsonResponse({ conversations: [conversationPayload()] }),
      ],
      [`${route}?after=2&limit=100`]: [
        () =>
          jsonResponse({
            // At-least-once: the walk re-delivers seq 2, which is already held.
            messages: [
              messagePayload({ id: "M2", client_msg_id: "k2", seq: 2 }),
              messagePayload({ id: "M3", client_msg_id: "k3", seq: 3 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
    });

    const store = useChatStore();
    await store.startDirect("bob");
    const socket = openSocket();

    socket.emitMessage(
      envelope(2, {
        t: "SyncState",
        d: {
          cursors: [{ conversation_id: CONVERSATION_ID, last_seq: 2 }],
        },
      }),
    );

    await vi.waitFor(() => {
      expect(store.messages.map((message) => message.seq)).toEqual([1, 2, 3]);
    });
    expect(store.messages).toHaveLength(3);
  });

  it("never reports a cursor past an unfilled gap", async () => {
    const route = messagesRoute();
    stubFetch({
      "/api/conversations/direct": [() => jsonResponse(conversationPayload())],
      [route]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M1", client_msg_id: "k1", seq: 1 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
      "/api/conversations": [
        () => jsonResponse({ conversations: [conversationPayload()] }),
      ],
      // The repair returns nothing, so the hole under seq 4 stays open.
      [`${route}?after=1&limit=100`]: [
        () =>
          jsonResponse({
            messages: [],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
    });

    const store = useChatStore();
    await store.startDirect("bob");
    const socket = openSocket();

    // seq 4 arrives while 2 and 3 are missing: the Device may only promise seq 1.
    socket.emitMessage(
      newMessageEnvelope(
        messagePayload({ id: "M4", client_msg_id: "k4", seq: 4 }),
      ),
    );

    await vi.waitFor(() => {
      const reports = socket.sent
        .map((text) => parseFrame(text))
        .filter((frame) => frame.e.t === "SyncCursor");
      expect(reports.length).toBeGreaterThan(0);
      // A cursor of 4 would make the next resume skip 2 and 3 forever.
      expect(reports.every((frame) => frame.e.d.last_seq === 1)).toBe(true);
    });
    expect(store.messages.map((message) => message.seq)).toEqual([1, 4]);
  });

  it("raises the unread badge for a peer's Message and clears it when read", async () => {
    const store = await openedStore();
    signIn();
    const socket = openSocket();

    socket.emitMessage(
      newMessageEnvelope(
        messagePayload({
          id: "PEER-1",
          sender_id: PEER_ID,
          client_msg_id: "peer-1",
          seq: 1,
          body: "未读",
        }),
      ),
    );

    expect(store.unreadCounts[CONVERSATION_ID]).toBe(1);

    // Reading the focused Conversation reports the marker and clears the badge.
    expect(store.markActiveRead()).toBe(true);
    expect(store.unreadCounts[CONVERSATION_ID]).toBe(0);

    const marker = socket.sent
      .map((text) => parseFrame(text))
      .find((frame) => frame.e.t === "MarkRead");
    expect(marker?.e.d.conversation_id).toBe(CONVERSATION_ID);
    expect(marker?.e.d.last_read_seq).toBe(1);
  });

  it("counts a re-delivered peer Message once and never the user's own", async () => {
    const store = await openedStore();
    signIn();
    const socket = openSocket();

    const peer = messagePayload({
      id: "PEER-1",
      sender_id: PEER_ID,
      client_msg_id: "peer-1",
      seq: 1,
    });
    // At-least-once delivery: the same Message may arrive twice, but the badge
    // must count it once.
    socket.emitMessage(newMessageEnvelope(peer));
    socket.emitMessage(newMessageEnvelope(peer));
    expect(store.unreadCounts[CONVERSATION_ID]).toBe(1);

    // The user's own Message from another Device (so not a local optimistic
    // bubble) is never unread.
    socket.emitMessage(
      envelope(4, {
        t: "NewMessage",
        d: {
          message: messagePayload({
            id: "MINE",
            sender_id: SELF_ID,
            client_msg_id: "mine-1",
            seq: 2,
          }),
        },
      }),
    );
    expect(store.unreadCounts[CONVERSATION_ID]).toBe(1);
  });

  it("clears the badge when a ReadMarker arrives for the account's other Device", async () => {
    const store = await openedStore();
    signIn();
    const socket = openSocket();

    socket.emitMessage(
      newMessageEnvelope(
        messagePayload({
          id: "PEER-1",
          sender_id: PEER_ID,
          client_msg_id: "peer-1",
          seq: 1,
        }),
      ),
    );
    expect(store.unreadCounts[CONVERSATION_ID]).toBe(1);

    // The account's other Device read the Conversation, so the server echoed the
    // private Read Marker to this Device; the shared badge clears here too.
    socket.emitMessage(
      envelope(4, {
        t: "ReadMarker",
        d: {
          conversation_id: CONVERSATION_ID,
          last_read_seq: 1,
          unread_count: 0,
        },
      }),
    );
    expect(store.unreadCounts[CONVERSATION_ID]).toBe(0);
  });

  it("reports a read when a Conversation with history is opened", async () => {
    stubFetch({
      "/api/conversations": [
        () => jsonResponse({ conversations: [conversationPayload()] }),
      ],
      [messagesRoute()]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M1", client_msg_id: "k1", seq: 1 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
          }),
      ],
    });

    const store = useChatStore();
    signIn();
    await store.loadConversations();
    const socket = openSocket();
    await store.openConversation(CONVERSATION_ID);

    const marker = socket.sent
      .map((text) => parseFrame(text))
      .find((frame) => frame.e.t === "MarkRead");
    expect(marker?.e.d.conversation_id).toBe(CONVERSATION_ID);
    expect(marker?.e.d.last_read_seq).toBe(1);
  });

  it("tracks the peer's public receipt separately from the unread badge", async () => {
    stubFetch({
      "/api/conversations": [
        () => jsonResponse({ conversations: [conversationPayload()] }),
      ],
      [messagesRoute()]: [
        () =>
          jsonResponse({
            messages: [
              messagePayload({ id: "M1", client_msg_id: "k1", seq: 1 }),
            ],
            next_before: null,
            next_after: null,
            has_more: false,
            read_receipts: [
              {
                conversation_id: CONVERSATION_ID,
                reader_id: PEER_ID,
                last_read_seq: 1,
              },
            ],
          }),
      ],
    });

    const store = useChatStore();
    signIn();
    await store.loadConversations();
    const socket = openSocket();
    await store.openConversation(CONVERSATION_ID);

    // The history page carries the peer's public receipt, and it is kept apart
    // from the user's own private Unread Count.
    expect(store.peerReceipts[CONVERSATION_ID]).toBe(1);
    expect(store.unreadCounts[CONVERSATION_ID] ?? 0).toBe(0);

    // A live receipt advances it.
    socket.emitMessage(
      envelope(4, {
        t: "ReadReceipt",
        d: {
          conversation_id: CONVERSATION_ID,
          reader_id: PEER_ID,
          last_read_seq: 5,
        },
      }),
    );
    expect(store.peerReceipts[CONVERSATION_ID]).toBe(5);
  });
});
