import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { useAuthStore } from "./auth";
import { BACKOFF_CAP_MS, computeBackoffDelay, useWsStore } from "./ws";
import type { ChatMessage, Conversation } from "./ws";
import type { MsgNew } from "../lib/protocol/frames";

/**
 * Controllable WebSocket double: the store drives it through serverOpen() /
 * serverFrame() / close(), and outbound frames are recorded in `sent`.
 */
class MockWebSocket {
  static instances: MockWebSocket[] = [];

  url: string;
  readyState = 0;
  sent: string[] = [];
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;

  constructor(url: string | URL) {
    this.url = String(url);
    MockWebSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.onclose?.({});
  }

  /** Test-side helpers simulating the server side. */
  serverOpen(): void {
    this.readyState = 1;
    this.onopen?.({});
  }

  serverFrame(frame: unknown): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
}

interface RawFrame {
  v: number;
  t: string;
  d: Record<string, unknown>;
}

const fetchMock = vi.fn();

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function lastSocket(): MockWebSocket {
  const sock = MockWebSocket.instances.at(-1);
  if (sock === undefined) throw new Error("no MockWebSocket instance");
  return sock;
}

function sentFrames(sock: MockWebSocket): RawFrame[] {
  return sock.sent.map((raw) => JSON.parse(raw) as RawFrame);
}

function seedConversation(
  store: ReturnType<typeof useWsStore>,
  conversationId: number,
  overrides: Partial<Conversation> = {},
): Conversation {
  const conversation: Conversation = {
    conversationId,
    peerUserId: 100 + conversationId,
    peerUsername: `peer${conversationId}`,
    lastMessagePreview: null,
    lastActivityAt: "",
    unread: 0,
    lastSeenSeq: 0,
    maxSeq: 0,
    ...overrides,
  };
  store.conversations.push(conversation);
  return conversation;
}

function msgNew(overrides: Partial<MsgNew>): MsgNew {
  return {
    message_id: "m-x",
    conversation_id: 1,
    seq: 1,
    sender_id: "peer-1",
    body: "hello",
    sent_at: new Date("2026-08-24T10:00:00Z").toISOString(),
    ...overrides,
  };
}

function seededMessage(overrides: Partial<ChatMessage>): ChatMessage {
  return {
    clientMsgId: "",
    messageId: "m-seed",
    conversationId: 1,
    seq: 1,
    senderId: "peer-1",
    body: "seeded",
    sentAt: new Date("2026-08-24T09:00:00Z").toISOString(),
    mine: false,
    status: "delivered",
    ...overrides,
  };
}

/** Connects with a stubbed ticket fetch and opens the socket. */
async function connectAndOpen(): Promise<MockWebSocket> {
  const auth = useAuthStore();
  auth.accessToken = "tok";
  auth.user = { userId: 7, username: "me" };
  // Fresh Response per call: a Response body can only be consumed once and
  // reconnects fetch the ticket endpoint again.
  fetchMock.mockImplementation(async () => jsonResponse(200, { ticket: "t1" }));
  const store = useWsStore();
  await store.connect();
  const sock = lastSocket();
  sock.serverOpen();
  return sock;
}

beforeEach(() => {
  setActivePinia(createPinia());
  localStorage.clear();
  fetchMock.mockReset();
  MockWebSocket.instances = [];
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("WebSocket", MockWebSocket);
});

afterEach(() => {
  useWsStore().dispose();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("ws store — optimistic send and ack resolution", () => {
  it("appends an optimistic sending entry and resolves it to delivered on msg.ack", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();

    const message = store.send(1, "hello");
    expect(message).not.toBeNull();
    expect(store.messagesByConversation[1]).toHaveLength(1);
    expect(message?.status).toBe("sending");
    expect(message?.mine).toBe(true);
    expect(message?.seq).toBeNull();

    // The wire frame carries the generated idempotency key.
    const frames = sentFrames(sock);
    const sendFrame = frames.at(-1);
    expect(sendFrame?.t).toBe("msg.send");
    expect(sendFrame?.d["body"]).toBe("hello");
    expect(sendFrame?.d["client_msg_id"]).toBe(message?.clientMsgId);

    sock.serverFrame({
      v: 1,
      t: "msg.ack",
      d: {
        client_msg_id: message?.clientMsgId,
        message_id: "m-1",
        seq: 6,
        duplicate: false,
      },
    });

    expect(store.messagesByConversation[1]).toHaveLength(1);
    expect(message?.status).toBe("delivered");
    expect(message?.messageId).toBe("m-1");
    expect(message?.seq).toBe(6);
    expect(store.conversations[0]?.maxSeq).toBe(6);
    expect(store.conversations[0]?.lastMessagePreview).toBe("hello");
  });

  it("merges a duplicate ack into the existing entry instead of adding a second bubble", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();

    const message = store.send(1, "hello");
    sock.serverFrame({
      v: 1,
      t: "msg.ack",
      d: {
        client_msg_id: message?.clientMsgId,
        message_id: "m-1",
        seq: 6,
        duplicate: false,
      },
    });
    // Server re-delivers the ack for the same client_msg_id (e.g. after a
    // retry raced with the first response).
    sock.serverFrame({
      v: 1,
      t: "msg.ack",
      d: {
        client_msg_id: message?.clientMsgId,
        message_id: "m-1",
        seq: 6,
        duplicate: true,
      },
    });

    expect(store.messagesByConversation[1]).toHaveLength(1);
    expect(message?.status).toBe("delivered");
  });
});

describe("ws store — inbound messages and unread state", () => {
  it("increments unread for inactive conversations and keeps the open one clean", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    seedConversation(store, 2);
    store.openConversation(1);
    const sock = await connectAndOpen();

    // Inactive conversation: unread bumps, preview updates.
    sock.serverFrame({
      v: 1,
      t: "msg.new",
      d: msgNew({ message_id: "m-a", conversation_id: 2, seq: 3, body: "yo" }),
    });
    const conv2 = store.conversations.find((c) => c.conversationId === 2);
    expect(conv2?.unread).toBe(1);
    expect(conv2?.lastMessagePreview).toBe("yo");
    expect(conv2?.maxSeq).toBe(3);

    // Active conversation: no unread, cursor advances as seen.
    sock.serverFrame({
      v: 1,
      t: "msg.new",
      d: msgNew({ message_id: "m-b", conversation_id: 1, seq: 9 }),
    });
    const conv1 = store.conversations.find((c) => c.conversationId === 1);
    expect(conv1?.unread).toBe(0);
    expect(conv1?.lastSeenSeq).toBe(9);

    // Own echo (same sender id as me) never counts as unread.
    sock.serverFrame({
      v: 1,
      t: "msg.new",
      d: msgNew({
        message_id: "m-c",
        conversation_id: 2,
        seq: 4,
        sender_id: "7",
      }),
    });
    expect(conv2?.unread).toBe(1);
  });

  it("fills sync gaps in order and ignores idempotent re-delivery", async () => {
    const store = useWsStore();
    seedConversation(store, 1, { maxSeq: 5, lastSeenSeq: 5 });
    store.messagesByConversation[1] = Array.from({ length: 5 }, (_, i) =>
      seededMessage({
        messageId: `m-${i + 1}`,
        seq: i + 1,
        body: `old ${i + 1}`,
      }),
    );
    const sock = await connectAndOpen();

    // Cursor reflects the local max seq at open time.
    const syncReq = sentFrames(sock)[0];
    expect(syncReq?.t).toBe("sync.req");
    expect(syncReq?.d["cursors"]).toEqual([
      { conversation_id: 1, last_delivered_seq: 5 },
    ]);

    // Out-of-order batch arrives sorted; incomplete batch then completion.
    sock.serverFrame({
      v: 1,
      t: "sync.res",
      d: {
        messages: [
          msgNew({ message_id: "m-7", seq: 7, body: "gap 7" }),
          msgNew({ message_id: "m-6", seq: 6, body: "gap 6" }),
        ],
        complete: false,
      },
    });
    sock.serverFrame({
      v: 1,
      t: "sync.res",
      d: {
        messages: [
          msgNew({ message_id: "m-6", seq: 6, body: "gap 6" }), // re-delivery
          msgNew({ message_id: "m-8", seq: 8, body: "gap 8" }),
          msgNew({ message_id: "m-9", seq: 9, body: "gap 9" }),
        ],
        complete: true,
      },
    });

    const messages = store.messagesByConversation[1] ?? [];
    expect(messages.map((m) => m.seq)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9]);
    expect(messages.filter((m) => m.messageId === "m-6")).toHaveLength(1);
  });
});

describe("ws store — offline queueing and reconnect", () => {
  it("queues outbound frames while disconnected and flushes them on reopen", () => {
    const store = useWsStore();
    seedConversation(store, 1);

    // No socket at all: the frame is held in memory.
    const message = store.send(1, "offline");
    expect(message?.status).toBe("sending");
    expect(store.queuedFrames).toHaveLength(1);
  });

  it("flushes queued frames right after the cursor sync when the socket opens", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    store.send(1, "offline");

    const sock = await connectAndOpen();
    const frames = sentFrames(sock);
    expect(frames[0]?.t).toBe("sync.req");
    expect(frames.at(-1)?.t).toBe("msg.send");
    expect(frames.at(-1)?.d["body"]).toBe("offline");
    expect(store.queuedFrames).toHaveLength(0);
  });

  it("reconnects with exponential backoff and resyncs updated cursors", async () => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(0.5); // jitter factor exactly 1.0
    const store = useWsStore();
    seedConversation(store, 1, { maxSeq: 5, lastSeenSeq: 5 });
    let sock = await connectAndOpen();

    // Live traffic advances the local cursor past the seeded value.
    sock.serverFrame({
      v: 1,
      t: "msg.new",
      d: msgNew({ message_id: "m-live", seq: 6 }),
    });

    sock.close();
    expect(store.status).toBe("reconnecting");

    // First retry fires at exactly 500ms (base · 2^0 · jitter 1.0).
    await vi.advanceTimersByTimeAsync(499);
    expect(MockWebSocket.instances).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(MockWebSocket.instances).toHaveLength(2);
    expect(store.status).toBe("connecting");

    sock = lastSocket();
    sock.serverOpen();
    expect(store.status).toBe("open");

    // Reconnect resyncs from the highest locally-seen seq.
    const syncReq = sentFrames(sock)[0];
    expect(syncReq?.t).toBe("sync.req");
    expect(syncReq?.d["cursors"]).toEqual([
      { conversation_id: 1, last_delivered_seq: 6 },
    ]);

    // Backoff restarts from the base after a successful open.
    sock.close();
    await vi.advanceTimersByTimeAsync(500);
    expect(MockWebSocket.instances).toHaveLength(3);
  });

  it("computes the backoff schedule as 500ms·2^n capped at 15s with ±20% jitter", () => {
    expect(computeBackoffDelay(0, () => 0.5)).toBe(500);
    expect(computeBackoffDelay(1, () => 0.5)).toBe(1_000);
    expect(computeBackoffDelay(4, () => 0.5)).toBe(8_000);
    expect(computeBackoffDelay(5, () => 0.5)).toBe(BACKOFF_CAP_MS);
    expect(computeBackoffDelay(9, () => 0.5)).toBe(BACKOFF_CAP_MS);
    // Jitter bounds: rand=0 → ×0.8, rand→1 approaches ×1.2 (still capped).
    expect(computeBackoffDelay(0, () => 0)).toBe(400);
    expect(computeBackoffDelay(0, () => 0.999)).toBe(600);
    expect(computeBackoffDelay(20, () => 0)).toBe(12_000);
    expect(computeBackoffDelay(20, () => 0.999)).toBe(BACKOFF_CAP_MS);
  });
});

describe("ws store — error frames and retry", () => {
  it("marks in-flight sends failed on an error frame and retry reuses the same client_msg_id", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();

    const message = store.send(1, "hi");
    sock.serverFrame({
      v: 1,
      t: "error",
      d: { code: "rate_limited", message: "slow down", retryable: true },
    });

    expect(message?.status).toBe("failed");
    expect(store.lastError?.code).toBe("rate_limited");

    store.retry(message?.clientMsgId ?? "");
    expect(message?.status).toBe("sending");
    const retryFrame = sentFrames(sock).at(-1);
    expect(retryFrame?.t).toBe("msg.send");
    expect(retryFrame?.d["client_msg_id"]).toBe(message?.clientMsgId);

    // Server had already persisted the first attempt → duplicate ack merges.
    sock.serverFrame({
      v: 1,
      t: "msg.ack",
      d: {
        client_msg_id: message?.clientMsgId,
        message_id: "m-r",
        seq: 7,
        duplicate: true,
      },
    });
    expect(store.messagesByConversation[1]).toHaveLength(1);
    expect(message?.status).toBe("delivered");
  });

  it("ignores unknown_type error frames (heartbeat noise) without failing sends", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();

    const message = store.send(1, "hi");
    sock.serverFrame({
      v: 1,
      t: "error",
      d: {
        code: "unknown_type",
        message: "unknown frame type: ping",
        retryable: false,
      },
    });

    expect(store.lastError).toBeNull();
    expect(message?.status).toBe("sending");
  });
});

describe("ws store — persistence", () => {
  it("restores conversations and messages from localStorage into a fresh store", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();
    const message = store.send(1, "remember me");
    sock.serverFrame({
      v: 1,
      t: "msg.ack",
      d: {
        client_msg_id: message?.clientMsgId,
        message_id: "m-p",
        seq: 1,
        duplicate: false,
      },
    });

    expect(localStorage.getItem("jiuyue.convos")).not.toBeNull();

    // Fresh pinia + fresh store reads the cache back.
    setActivePinia(createPinia());
    const restored = useWsStore();
    expect(restored.conversations).toHaveLength(1);
    expect(restored.conversations[0]?.maxSeq).toBe(1);
    const messages = restored.messagesByConversation[1] ?? [];
    expect(messages).toHaveLength(1);
    expect(messages[0]?.body).toBe("remember me");
    expect(messages[0]?.status).toBe("delivered");
  });

  it("caps persisted history at 500 entries per conversation keeping the newest", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    for (let seq = 1; seq <= 505; seq += 1) {
      store.ingestMessage(msgNew({ message_id: `m-${seq}`, seq }));
    }

    const stored: unknown = JSON.parse(
      localStorage.getItem("jiuyue.msgs.1") ?? "null",
    );
    expect(Array.isArray(stored)).toBe(true);
    expect(stored).toHaveLength(500);
    const messages = stored as ChatMessage[];
    expect(messages[0]?.seq).toBe(6);
    expect(messages.at(-1)?.seq).toBe(505);
  });
});
