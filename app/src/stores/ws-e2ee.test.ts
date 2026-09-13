// @vitest-environment node
// Node environment: the real WebCrypto engine runs (deterministic), while
// localStorage is absent — the store and engine both degrade to memory.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import en from "../i18n/locales/en";
import zhCN from "../i18n/locales/zh-CN";
import { PeerNotReadyError } from "../lib/api/e2ee";
import * as olm from "../lib/crypto/olm-lite";
import { useAuthStore } from "./auth";
import { useWsStore } from "./ws";

// These tests run the REAL WebCrypto engine plus real-timer settles. Under a
// full-suite run (with the M14 call/WebRTC modules loaded) P-256 keygen and the
// 40x2ms settle loops can exceed the default 5s budget, so give the file room.
vi.setConfig({ testTimeout: 25000 });

/** Controllable WebSocket double (same shape as ws.test.ts). */
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

/** Real WebCrypto resolves off the microtask queue — settle with real time. */
async function settle(): Promise<void> {
  for (let i = 0; i < 40; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
}

/** A throwaway P-256 bundle standing in for the peer's server-side keys. */
async function foreignBundle(): Promise<{
  identity_key: string;
  one_time_key: string;
}> {
  const pair = await crypto.subtle.generateKey(
    { name: "ECDH", namedCurve: "P-256" },
    true,
    ["deriveBits"],
  );
  const spki = new Uint8Array(
    await crypto.subtle.exportKey("spki", pair.publicKey),
  );
  const b64 = btoa(String.fromCharCode(...spki));
  return { identity_key: b64, one_time_key: b64 };
}

let scopeCounter = 0;

beforeEach(() => {
  setActivePinia(createPinia());
  fetchMock.mockReset();
  MockWebSocket.instances = [];
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("WebSocket", MockWebSocket);
  scopeCounter += 1;
  olm.useStorageScope(`store-e2ee-${scopeCounter}`);
});

afterEach(() => {
  useWsStore().dispose();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("ws store — M3 secret conversations", () => {
  it("sends a secret message as an e2ee.msg frame carrying opaque ciphertext", async () => {
    const bobBundle = await foreignBundle();
    fetchMock.mockImplementation(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.includes("/api/auth/ws-ticket"))
          return jsonResponse(200, { ticket: "t1" });
        if (url.includes("/api/e2ee/keys/upload")) return jsonResponse(200, {});
        if (url.includes("/api/e2ee/keys/bob"))
          return jsonResponse(200, bobBundle);
        if (url.includes("/api/conversations")) {
          if ((init?.method ?? "GET") === "POST") {
            return jsonResponse(201, {
              conversation_id: 1,
              peer: { user_id: "u-bob", username: "bob" },
              created: true,
              kind: "secret",
            });
          }
          return jsonResponse(200, []);
        }
        return jsonResponse(200, {});
      },
    );

    const auth = useAuthStore();
    auth.accessToken = "tok";
    auth.user = { userId: "7", username: "me", uid: 1000007 };
    const store = useWsStore();
    await store.connect();
    const sock = lastSocket();
    sock.serverOpen();
    await settle();

    const conversation = await store.createOrOpenConversation("bob", "secret");
    expect(conversation.kind).toBe("secret");
    await settle();

    const message = store.send(1, "hidden hello");
    expect(message?.body).toBe("hidden hello"); // local plaintext display only
    await settle();

    const frames = sentFrames(sock);
    const e2ee = frames.filter((f) => f.t === "e2ee.msg");
    expect(e2ee).toHaveLength(1);
    expect(e2ee[0]?.d["conversation_id"]).toBe(1);
    expect(e2ee[0]?.d["message_type"]).toBe(0); // session-init
    expect(e2ee[0]?.d["client_msg_id"]).toBe(message?.clientMsgId);
    const ciphertext = e2ee[0]?.d["ciphertext"];
    expect(typeof ciphertext).toBe("string");
    expect(ciphertext).not.toContain("hidden");
    // Secret traffic never rides the plaintext msg.send frame.
    expect(frames.some((f) => f.t === "msg.send")).toBe(false);
  });

  it("delivers peer ciphertext end-to-end through the real engine", async () => {
    // Bob is a second engine instance with its own key material.
    const bobScope = `store-e2ee-${scopeCounter}-bob`;
    olm.useStorageScope(bobScope);
    await olm.ensureIdentity();
    const bobKeys = await olm.localBundle();

    const aliceScope = `store-e2ee-${scopeCounter}-alice`;
    olm.useStorageScope(aliceScope);
    fetchMock.mockImplementation(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.includes("/api/auth/ws-ticket"))
          return jsonResponse(200, { ticket: "t1" });
        if (url.includes("/api/e2ee/keys/upload")) return jsonResponse(200, {});
        if (url.includes("/api/e2ee/keys/bob"))
          return jsonResponse(200, {
            identity_key: bobKeys.identity_key,
            one_time_key: bobKeys.one_time_keys[0],
          });
        if (url.includes("/api/conversations")) {
          if ((init?.method ?? "GET") === "POST") {
            return jsonResponse(201, {
              conversation_id: 1,
              peer: { user_id: "u-bob", username: "bob" },
              created: true,
              kind: "secret",
            });
          }
          return jsonResponse(200, []);
        }
        return jsonResponse(200, {});
      },
    );

    const auth = useAuthStore();
    auth.accessToken = "tok";
    auth.user = { userId: "7", username: "me", uid: 1000007 };
    const store = useWsStore();
    await store.connect();
    const sock = lastSocket();
    sock.serverOpen();
    await settle();

    await store.createOrOpenConversation("bob", "secret");
    await settle();

    store.send(1, "hi from alice");
    await settle();
    const outbound = sentFrames(sock).find((f) => f.t === "e2ee.msg");
    expect(outbound?.t).toBe("e2ee.msg");
    const wireCiphertext = String(outbound?.d["ciphertext"] ?? "");

    // The exact bytes that went over the wire decrypt on Bob's device.
    olm.useStorageScope(bobScope);
    expect(await olm.decrypt(1, wireCiphertext)).toBe("hi from alice");

    // Bob replies; the store renders the decrypted plaintext bubble.
    const reply = await olm.encrypt(1, "hi from bob");
    olm.useStorageScope(aliceScope);
    sock.serverFrame({
      v: 1,
      t: "e2ee.msg",
      d: {
        conversation_id: 1,
        client_msg_id: "cmid-inbound",
        ciphertext: reply.ciphertext,
        message_type: reply.messageType,
      },
    });
    await settle();

    const messages = store.messagesByConversation[1] ?? [];
    const inbound = messages.find((m) => m.mine === false);
    expect(inbound?.body).toBe("hi from bob");
    expect(inbound?.undecryptable ?? false).toBe(false);
    expect(store.conversations[0]?.lastMessagePreview).toBe("hi from bob");
  });

  it("renders an undecryptable placeholder bubble when decryption fails", async () => {
    const store = useWsStore();
    store.conversations.push({
      conversationId: 1,
      peerUserId: "u-bob",
      peerUsername: "bob",
      lastMessagePreview: null,
      lastActivityAt: "",
      unread: 0,
      lastSeenSeq: 0,
      maxSeq: 0,
      peerTypingUntil: null,
      kind: "secret",
    });

    const auth = useAuthStore();
    auth.accessToken = "tok";
    auth.user = { userId: "7", username: "me", uid: 1000007 };
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/auth/ws-ticket"))
        return jsonResponse(200, { ticket: "t1" });
      return jsonResponse(200, []);
    });
    await store.connect();
    const sock = lastSocket();
    sock.serverOpen();
    await settle();

    sock.serverFrame({
      v: 1,
      t: "e2ee.msg",
      d: {
        conversation_id: 1,
        client_msg_id: "cmid-garbage",
            ciphertext: "!!!not-a-valid-envelope",
        message_type: 1,
      },
    });
    await settle();

    const messages = store.messagesByConversation[1] ?? [];
    expect(messages).toHaveLength(1);
    expect(messages[0]?.undecryptable).toBe(true);
    expect(messages[0]?.body).toBe("");
    expect(messages[0]?.mine).toBe(false);
  });
});

describe("M3 i18n strings exist in both locales", () => {
  it("covers the secret-chat and error keys", () => {
    const chatKeys = [
      "kindDirect",
      "kindSecret",
      "secretBanner",
      "sasLabel",
      "sasHint",
      "sasPending",
      "undecryptablePlaceholder",
    ];
    for (const key of chatKeys) {
      expect(typeof (zhCN.chat as Record<string, unknown>)[key]).toBe("string");
      expect(typeof (en.chat as Record<string, unknown>)[key]).toBe("string");
    }
    expect(typeof zhCN.errors.no_one_time_keys).toBe("string");
    expect(typeof en.errors.no_one_time_keys).toBe("string");
  });

  it("covers the peer-not-ready secret-chat key", () => {
    expect(typeof zhCN.errors.secret_peer_not_ready).toBe("string");
    expect(typeof en.errors.secret_peer_not_ready).toBe("string");
  });
});

describe("ws store — M3 proactive key publish & peer readiness", () => {
  it("publishes this device's key bundle once the socket opens", async () => {
    const posts: string[] = [];
    fetchMock.mockImplementation(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if ((init?.method ?? "GET") !== "GET") posts.push(url);
        if (url.includes("/api/auth/ws-ticket"))
          return jsonResponse(200, { ticket: "t1" });
        if (url.includes("/api/e2ee/keys/upload")) return jsonResponse(201, {});
        if (url.includes("/api/conversations")) return jsonResponse(200, []);
        return jsonResponse(200, {});
      },
    );

    const auth = useAuthStore();
    auth.accessToken = "tok";
    auth.user = { userId: "7", username: "me", uid: 1000007 };
    const store = useWsStore();
    await store.connect();
    const sock = lastSocket();
    sock.serverOpen();

    await vi.waitFor(
      () => {
        expect(posts.some((url) => url.includes("/api/e2ee/keys/upload"))).toBe(
          true,
        );
      },
      { timeout: 8000, interval: 25 },
    );
    // No secret conversation was opened — the publish is proactive.
    expect(store.conversations).toHaveLength(0);
  });

  it("rejects a secret conversation whose peer has no published bundle", async () => {
    fetchMock.mockImplementation(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.includes("/api/auth/ws-ticket"))
          return jsonResponse(200, { ticket: "t1" });
        if (url.includes("/api/e2ee/keys/upload")) return jsonResponse(201, {});
        if (url.includes("/api/e2ee/keys/bob"))
          return jsonResponse(404, {
            error: "peer_not_found",
            message: "no bundle",
          });
        if (url.includes("/api/conversations")) {
          if ((init?.method ?? "GET") === "POST") {
            return jsonResponse(201, {
              conversation_id: 1,
              peer: { user_id: "u-bob", username: "bob" },
              created: true,
              kind: "secret",
            });
          }
          return jsonResponse(200, []);
        }
        return jsonResponse(200, {});
      },
    );

    const auth = useAuthStore();
    auth.accessToken = "tok";
    auth.user = { userId: "7", username: "me", uid: 1000007 };
    const store = useWsStore();
    await store.connect();
    lastSocket().serverOpen();
    await settle();

    await expect(
      store.createOrOpenConversation("bob", "secret"),
    ).rejects.toBeInstanceOf(PeerNotReadyError);
  });
});

describe("ws store - sync replay of secret chats", () => {
  it("routes replayed ciphertext entries from sync.res through the decrypt path", async () => {
    const store = useWsStore();
    store.conversations.push({
      conversationId: 1,
      peerUserId: "u-bob",
      peerUsername: "bob",
      lastMessagePreview: null,
      lastActivityAt: "",
      unread: 0,
      lastSeenSeq: 0,
      maxSeq: 0,
      peerTypingUntil: null,
      kind: "secret",
    });
    const auth = useAuthStore();
    auth.accessToken = "tok";
    auth.user = { userId: "7", username: "me", uid: 1000007 };
    fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/auth/ws-ticket"))
        return jsonResponse(200, { ticket: "t1" });
      return jsonResponse(200, []);
    });
    await store.connect();
    const sock = lastSocket();
    sock.serverOpen();
    await settle();

    // Regression: before the fix the guard rejected the mixed batch as
    // malformed, so replayed secret messages never reached the decrypt path.
    sock.serverFrame({
      v: 1,
      t: "sync.res",
      d: {
        messages: [
          {
            conversation_id: 1,
            client_msg_id: "cmid-garbage",
            ciphertext: "!!!not-a-valid-envelope",
            message_type: 1,
          },
        ],
        complete: true,
      },
    });
    await settle();

    const messages = store.messagesByConversation[1] ?? [];
    expect(messages).toHaveLength(1);
    expect(messages[0]?.undecryptable).toBe(true);
  });
});