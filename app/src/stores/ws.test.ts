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
    peerUserId: String(100 + conversationId),
    peerUsername: `peer${conversationId}`,
    lastMessagePreview: null,
    lastActivityAt: "",
    unread: 0,
    lastSeenSeq: 0,
    maxSeq: 0,
    peerTypingUntil: null,
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
    recalled: false,
    replyToMessageId: null,
    replyToSenderId: null,
    replyToBodyPreview: null,
    forwardedFromUsername: null,
    ...overrides,
  };
}

/** Connects with a stubbed ticket fetch and opens the socket. */
async function connectAndOpen(): Promise<MockWebSocket> {
  const auth = useAuthStore();
  auth.accessToken = "tok";
  auth.user = { userId: "7", username: "me", uid: 1000007 };
  // Fresh Response per call: a Response body can only be consumed once and
  // reconnects fetch the ticket endpoint again.
  fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/api/auth/ws-ticket"))
      return jsonResponse(200, { ticket: "t1" });
    if (url.includes("/api/conversations")) return jsonResponse(200, []);
    return jsonResponse(200, { ticket: "t1" });
  });
  const store = useWsStore();
  await store.connect();
  const sock = lastSocket();
  sock.serverOpen();
  // onopen now bootstraps conversation discovery (async fetch) before the
  // cursor sync; drain microtask queue deterministically (fake-timer safe).
  for (let i = 0; i < 12; i++) await Promise.resolve();
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

describe("ws store 鈥?optimistic send and ack resolution", () => {
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

describe("ws store 鈥?inbound messages and unread state", () => {
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

describe("ws store 鈥?offline queueing and reconnect", () => {
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

    // First retry fires at exactly 500ms (base 路 2^0 路 jitter 1.0).
    await vi.advanceTimersByTimeAsync(499);
    expect(MockWebSocket.instances).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(MockWebSocket.instances).toHaveLength(2);
    expect(store.status).toBe("connecting");

    sock = lastSocket();
    sock.serverOpen();
    expect(store.status).toBe("open");
    for (let i = 0; i < 12; i++) await Promise.resolve();

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

  it("computes the backoff schedule as 500ms路2^n capped at 15s with 卤20% jitter", () => {
    expect(computeBackoffDelay(0, () => 0.5)).toBe(500);
    expect(computeBackoffDelay(1, () => 0.5)).toBe(1_000);
    expect(computeBackoffDelay(4, () => 0.5)).toBe(8_000);
    expect(computeBackoffDelay(5, () => 0.5)).toBe(BACKOFF_CAP_MS);
    expect(computeBackoffDelay(9, () => 0.5)).toBe(BACKOFF_CAP_MS);
    // Jitter bounds: rand=0 鈫?脳0.8, rand鈫? approaches 脳1.2 (still capped).
    expect(computeBackoffDelay(0, () => 0)).toBe(400);
    expect(computeBackoffDelay(0, () => 0.999)).toBe(600);
    expect(computeBackoffDelay(20, () => 0)).toBe(12_000);
    expect(computeBackoffDelay(20, () => 0.999)).toBe(BACKOFF_CAP_MS);
  });
});

describe("ws store 鈥?error frames and retry", () => {
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

    // Server had already persisted the first attempt 鈫?duplicate ack merges.
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

describe("ws store 鈥?persistence", () => {
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

function readUpdateFrames(sock: MockWebSocket): Array<Record<string, unknown>> {
  return sentFrames(sock)
    .filter((f) => f.t === "read.update")
    .map((f) => f.d as Record<string, unknown>);
}

describe("ws store — M2 read receipts", () => {
  it("upgrades matching mine-messages from delivered to read on read.receipt", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    store.messagesByConversation[1] = [
      seededMessage({
        messageId: "m-mine",
        seq: 2,
        senderId: "7",
        mine: true,
        status: "delivered",
      }),
      seededMessage({
        messageId: "m-mine-late",
        seq: 5,
        senderId: "7",
        mine: true,
        status: "delivered",
      }),
      seededMessage({ messageId: "m-theirs", seq: 3, mine: false }),
    ];
    const sock = await connectAndOpen();

    // Peer read through seq 2: only the covered own message flips to read.
    sock.serverFrame({
      v: 1,
      t: "read.receipt",
      d: { conversation_id: 1, user_id: "peer-1", last_read_seq: 2 },
    });

    const messages = store.messagesByConversation[1] ?? [];
    expect(messages.find((m) => m.messageId === "m-mine")?.status).toBe("read");
    expect(messages.find((m) => m.messageId === "m-mine-late")?.status).toBe(
      "delivered",
    );
    expect(messages.find((m) => m.messageId === "m-theirs")?.status).toBe(
      "delivered",
    );

    // Idempotent: an older receipt must never regress read → delivered.
    sock.serverFrame({
      v: 1,
      t: "read.receipt",
      d: { conversation_id: 1, user_id: "peer-1", last_read_seq: 1 },
    });
    expect(messages.find((m) => m.messageId === "m-mine")?.status).toBe("read");
  });

  it("auto-sends a debounced read.update while the conversation is open and focused", async () => {
    vi.useFakeTimers();
    vi.spyOn(document, "hasFocus").mockReturnValue(true);
    const store = useWsStore();
    seedConversation(store, 1, { maxSeq: 5, lastSeenSeq: 5 });
    const sock = await connectAndOpen();

    store.openConversation(1);
    await vi.advanceTimersByTimeAsync(299);
    expect(readUpdateFrames(sock)).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(readUpdateFrames(sock)).toEqual([
      { conversation_id: 1, last_read_seq: 5 },
    ]);

    // New arrivals in the OPEN conversation trigger exactly one more report
    // covering the advanced cursor (the debounce collapses bursts).
    sock.serverFrame({
      v: 1,
      t: "msg.new",
      d: msgNew({ message_id: "m-live", seq: 6 }),
    });
    sock.serverFrame({
      v: 1,
      t: "msg.new",
      d: msgNew({ message_id: "m-live2", seq: 7 }),
    });
    await vi.advanceTimersByTimeAsync(300);
    expect(readUpdateFrames(sock)).toEqual([
      { conversation_id: 1, last_read_seq: 5 },
      { conversation_id: 1, last_read_seq: 7 },
    ]);
  });

  it("skips the auto read.update when the window is not focused", async () => {
    vi.useFakeTimers();
    vi.spyOn(document, "hasFocus").mockReturnValue(false);
    const store = useWsStore();
    seedConversation(store, 1, { maxSeq: 4, lastSeenSeq: 4 });
    const sock = await connectAndOpen();

    store.openConversation(1);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(readUpdateFrames(sock)).toHaveLength(0);
  });
});

describe("ws store — M2 typing indicators", () => {
  it("shows the peer indicator on start and clears on stop and on new incoming message", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();
    const conversation = store.conversations[0];

    sock.serverFrame({
      v: 1,
      t: "typing",
      d: { conversation_id: 1, user_id: "peer-1", state: "start" },
    });
    expect(conversation?.peerTypingUntil).not.toBeNull();

    sock.serverFrame({
      v: 1,
      t: "typing",
      d: { conversation_id: 1, user_id: "peer-1", state: "stop" },
    });
    expect(conversation?.peerTypingUntil).toBeNull();

    sock.serverFrame({
      v: 1,
      t: "typing",
      d: { conversation_id: 1, user_id: "peer-1", state: "start" },
    });
    expect(conversation?.peerTypingUntil).not.toBeNull();
    // A fresh incoming message proves the peer stopped typing.
    sock.serverFrame({
      v: 1,
      t: "msg.new",
      d: msgNew({ message_id: "m-in", seq: 9 }),
    });
    expect(conversation?.peerTypingUntil).toBeNull();
  });

  it("expires the indicator after 6 seconds without a refresh", async () => {
    vi.useFakeTimers();
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();
    const conversation = store.conversations[0];

    sock.serverFrame({
      v: 1,
      t: "typing",
      d: { conversation_id: 1, user_id: "peer-1", state: "start" },
    });
    expect(conversation?.peerTypingUntil).not.toBeNull();
    await vi.advanceTimersByTimeAsync(6_000);
    expect(conversation?.peerTypingUntil).toBeNull();
  });

  it("ignores own multi-device typing echoes", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();

    sock.serverFrame({
      v: 1,
      t: "typing",
      d: { conversation_id: 1, user_id: "7", state: "start" },
    });
    expect(store.conversations[0]?.peerTypingUntil).toBeNull();
  });

  it("throttles outbound start frames to one per 3s and stops once", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();

    store.notifyTypingStart(1);
    store.notifyTypingStart(1);
    store.notifyTypingStart(1);
    let typing = sentFrames(sock).filter((f) => f.t === "typing");
    expect(typing).toHaveLength(1);
    expect(typing[0]?.d).toEqual({ conversation_id: 1, state: "start" });

    store.notifyTypingStop(1);
    store.notifyTypingStop(1);
    typing = sentFrames(sock).filter((f) => f.t === "typing");
    expect(typing).toHaveLength(2);
    expect(typing[1]?.d).toEqual({ conversation_id: 1, state: "stop" });
  });

  it("never queues ephemeral signals while offline", () => {
    const store = useWsStore();
    seedConversation(store, 1);

    store.notifyTypingStart(1);
    store.scheduleReadUpdate(1);
    store.recallMessage(1, seededMessage({ messageId: "m-x", mine: true }));

    expect(store.queuedFrames).toHaveLength(0);
  });
});

describe("ws store — M2 recall", () => {
  it("swaps the bubble content for a tombstone on msg.recalled", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    store.messagesByConversation[1] = [
      seededMessage({ messageId: "m-gone", seq: 1, body: "secret" }),
    ];
    const sock = await connectAndOpen();

    sock.serverFrame({
      v: 1,
      t: "msg.recalled",
      d: { conversation_id: 1, message_id: "m-gone" },
    });

    const entry = store.messagesByConversation[1]?.find(
      (m) => m.messageId === "m-gone",
    );
    expect(entry?.recalled).toBe(true);
    expect(entry?.body).toBe("");
  });

  it("sends msg.recall for own messages and ignores foreign ones", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();

    store.recallMessage(
      1,
      seededMessage({ messageId: "m-mine", mine: true, senderId: "7" }),
    );
    const recalls = sentFrames(sock).filter((f) => f.t === "msg.recall");
    expect(recalls).toHaveLength(1);
    expect(recalls[0]?.d).toEqual({ conversation_id: 1, message_id: "m-mine" });

    store.recallMessage(
      1,
      seededMessage({ messageId: "m-theirs", mine: false }),
    );
    expect(sentFrames(sock).filter((f) => f.t === "msg.recall")).toHaveLength(
      1,
    );
  });
});

describe("ws store — M2 reply quoting", () => {
  it("sets and cancels the composer reply context", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    await connectAndOpen();

    store.setReplyContext(
      seededMessage({
        messageId: "m-quote",
        body: "quoted content",
        senderId: "p",
      }),
    );
    expect(store.replyContext).toEqual({
      messageId: "m-quote",
      senderId: "p",
      bodyPreview: "quoted content",
    });

    store.clearReplyContext();
    expect(store.replyContext).toBeNull();
  });

  it("attaches reply_to to the send frame and consumes the context", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    const sock = await connectAndOpen();

    store.setReplyContext(seededMessage({ messageId: "m-quote" }));
    store.send(1, "answer");

    const sendFrame = sentFrames(sock).at(-1);
    expect(sendFrame?.t).toBe("msg.send");
    expect(sendFrame?.d["reply_to"]).toBe("m-quote");
    expect(store.replyContext).toBeNull();

    // A plain send afterwards carries NO reply_to key at all.
    store.send(1, "plain");
    const plainFrame = sentFrames(sock).at(-1);
    expect("reply_to" in (plainFrame?.d ?? {})).toBe(false);
  });
});

describe("ws store — M2 forwarding", () => {
  it("composes a NEW prefixed message into the target conversation", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    seedConversation(store, 2);
    const sock = await connectAndOpen();

    const result = store.forwardMessage(
      2,
      seededMessage({ messageId: "m-src", body: "original text" }),
    );

    expect(result?.body).toBe("[转发] original text");
    expect(result?.conversationId).toBe(2);
    expect(result?.clientMsgId).not.toBe("");
    const sendFrame = sentFrames(sock).at(-1);
    expect(sendFrame?.t).toBe("msg.send");
    expect(sendFrame?.d["conversation_id"]).toBe(2);
    expect(sendFrame?.d["body"]).toBe("[转发] original text");
  });

  it("refuses to forward recalled (empty-content) sources", async () => {
    const store = useWsStore();
    seedConversation(store, 1);
    seedConversation(store, 2);
    await connectAndOpen();

    const result = store.forwardMessage(
      2,
      seededMessage({ messageId: "m-dead", body: "", recalled: true }),
    );
    expect(result).toBeNull();
  });
  it("enriches skeleton conversations created by live msg.new with peer identity", async () => {
    const store = useWsStore();
    const sock = await connectAndOpen();

    // Live arrival for an unknown conversation creates a skeleton.
    sock.serverFrame({
      v: 1,
      t: "msg.new",
      d: msgNew({ message_id: "m-live-1", conversation_id: 42, seq: 1 }),
    });
    expect(store.conversations.some((c) => c.conversationId === 42)).toBe(true);
    // Let the skeleton-triggered (old-mock) enrichment settle first.
    for (let i = 0; i < 12; i++) await Promise.resolve();
    expect(
      store.conversations.find((c) => c.conversationId === 42)?.peerUsername,
    ).toBe("");

    // Backfill fetch resolves with the peer identity.
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/auth/ws-ticket"))
        return jsonResponse(200, { ticket: "t1" });
      if (url.includes("/api/conversations"))
        return jsonResponse(200, [
          {
            conversation_id: 42,
            kind: "direct",
            peer: { user_id: "peer-42", username: "alice" },
            last_seq: 1,
            last_delivered_seq: 0,
          },
        ]);
      return jsonResponse(200, { ticket: "t1" });
    });
    await store.enrichPeerIdentity();

    const conv = store.conversations.find((c) => c.conversationId === 42);
    expect(conv?.peerUsername).toBe("alice");
    expect(conv?.peerUserId).toBe("peer-42");
  });

  it("applyServerConversationList enriches empty peers without resetting local state", () => {
    const store = useWsStore();
    seedConversation(store, 7, { maxSeq: 9, unread: 3 });
    const conv = store.conversations.find((c) => c.conversationId === 7);
    if (conv === undefined) throw new Error("seed failed");
    conv.peerUserId = "";
    conv.peerUsername = "";

    store.applyServerConversationList([
      {
        conversation_id: 7,
        kind: "direct",
        peer: { user_id: "peer-7", username: "bob" },
        last_seq: 99,
        last_delivered_seq: 99,
      },
      {
        conversation_id: 8,
        kind: "direct",
        peer: { user_id: "peer-8", username: "carol" },
        last_seq: 4,
        last_delivered_seq: 4,
      },
    ]);

    const enriched = store.conversations.find((c) => c.conversationId === 7);
    expect(enriched?.peerUsername).toBe("bob");
    expect(enriched?.maxSeq).toBe(9); // local sync state untouched
    expect(enriched?.unread).toBe(3);
    const added = store.conversations.find((c) => c.conversationId === 8);
    expect(added?.peerUsername).toBe("carol");
    expect(added?.maxSeq).toBe(0); // fresh entry starts from zero
  });
  it("profile.updated refreshes conversation peer identity + invalidates profile cache", async () => {
    const auth = useAuthStore();
    auth.user = { userId: "me-1", username: "me", uid: 1 };
    const ws = useWsStore();
    ws.conversations.push({
      conversationId: 7,
      peerUserId: "peer-9",
      peerUsername: "oldname",
      peerAvatar: "🐷",
      lastMessagePreview: "hi",
      lastActivityAt: "now",
      unread: 0,
      lastSeenSeq: 0,
      maxSeq: 0,
      peerTypingUntil: null,
      kind: "direct",
    });
    // Seed the profile store peer cache so invalidation is observable.
    const { useProfileStore } = await import("./profile");
    const profileStore = useProfileStore();
    profileStore.peers["peer-9"] = {
      user_id: "peer-9",
      username: "oldname",
      uid: 9,
      display_name: "oldname",
      bio: "",
      avatar: "🐷",
      level: 1,
      title: "",
      xp: 0,
      xp_to_next: 0,
    };

    ws.handleProfileUpdated({
      user_id: "peer-9",
      display_name: "矿工老王",
      avatar: "data:image/jpeg;base64,AAAA",
    });

    expect(ws.conversations[0]!.peerDisplayName).toBe("矿工老王");
    expect(ws.conversations[0]!.peerAvatar).toBe("data:image/jpeg;base64,AAAA");
    expect(profileStore.peers["peer-9"]).toBeUndefined();
  });
});