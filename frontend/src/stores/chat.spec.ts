import { createPinia, setActivePinia } from "pinia";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeWebSocket } from "../testing/fake-websocket";
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
  e: { t: string; d: { client_msg_id?: string } };
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
});
