import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";

import {
  resetIceServerCache,
  resetLocalMic,
  shareQuality,
} from "../lib/rtc/callSession";
import { useAuthStore } from "./auth";
import { useCallStore } from "./call";
import { useWsStore } from "./ws";
import type { Conversation } from "./ws";

// ---------------------------------------------------------------------------
// WebRTC / media doubles (jsdom has none)
// ---------------------------------------------------------------------------

class FakeTrack {
  readonly kind: string;
  enabled = true;
  stopped = false;
  onended: (() => void) | null = null;
  constructor(kind: string) {
    this.kind = kind;
  }
  stop(): void {
    this.stopped = true;
  }
}

class FakeStream {
  private readonly tracks: FakeTrack[];
  constructor(tracks: FakeTrack[] = []) {
    this.tracks = tracks;
  }
  getTracks(): FakeTrack[] {
    return this.tracks;
  }
  getAudioTracks(): FakeTrack[] {
    return this.tracks.filter((t) => t.kind === "audio");
  }
  getVideoTracks(): FakeTrack[] {
    return this.tracks.filter((t) => t.kind === "video");
  }
  addTrack(track: FakeTrack): void {
    this.tracks.push(track);
  }
}

class FakeSender {
  track: FakeTrack | null = null;
  transceiver: FakeTransceiver | null = null;
  replaceTrack = vi.fn(async (track: FakeTrack | null) => {
    this.track = track;
  });
}

class FakeTransceiver {
  sender: FakeSender;
  direction: string;
  constructor(sender: FakeSender, direction: string) {
    this.sender = sender;
    this.direction = direction;
    sender.transceiver = this;
  }
}

class FakePeerConnection {
  static instances: FakePeerConnection[] = [];
  onicecandidate: ((ev: unknown) => void) | null = null;
  ontrack: ((ev: unknown) => void) | null = null;
  onconnectionstatechange: (() => void) | null = null;
  signalingState = "stable";
  connectionState: RTCPeerConnectionState = "new";
  localDescription: RTCSessionDescriptionInit | null = null;
  private readonly senders: FakeSender[] = [];

  constructor(_config: RTCConfiguration) {
    FakePeerConnection.instances.push(this);
  }

  addTrack = vi.fn((track: FakeTrack): FakeSender => {
    const sender = new FakeSender();
    sender.track = track;
    this.senders.push(sender);
    new FakeTransceiver(sender, "sendrecv");
    return sender;
  });

  addTransceiver = vi.fn((_kind: string, init?: { direction?: string }) => {
    const sender = new FakeSender();
    this.senders.push(sender);
    return new FakeTransceiver(sender, init?.direction ?? "sendrecv");
  });

  createOffer = vi.fn(async () => ({ type: "offer", sdp: "offer-sdp" }));
  createAnswer = vi.fn(async () => ({ type: "answer", sdp: "answer-sdp" }));
  setLocalDescription = vi.fn(async (d?: RTCSessionDescriptionInit) => {
    this.localDescription = d ?? { type: "offer", sdp: "offer-sdp" };
    this.signalingState = "have-local-offer";
  });
  setRemoteDescription = vi.fn(async (d: RTCSessionDescriptionInit) => {
    this.signalingState = d.type === "offer" ? "have-remote-offer" : "stable";
  });
  addIceCandidate = vi.fn(async () => undefined);
  getSenders = vi.fn((): FakeSender[] => [...this.senders]);
  close = vi.fn();
}

interface FakeNode {
  connect: ReturnType<typeof vi.fn>;
  disconnect: ReturnType<typeof vi.fn>;
}

class FakeAudioContext {
  static instances: FakeAudioContext[] = [];
  destination: unknown = { kind: "destination" };
  analysers: FakeNode[] = [];
  mediaSources: FakeNode[] = [];

  constructor() {
    FakeAudioContext.instances.push(this);
  }

  createOscillator = (): unknown => ({
    type: "sine",
    frequency: { value: 0 },
    connect: vi.fn(),
    disconnect: vi.fn(),
    start: vi.fn(),
    stop: vi.fn(),
  });
  createGain = (): unknown => ({
    gain: { value: 0 },
    connect: vi.fn(),
    disconnect: vi.fn(),
  });
  createAnalyser = (): FakeNode & {
    fftSize: number;
    frequencyBinCount: number;
    getByteFrequencyData: ReturnType<typeof vi.fn>;
  } => {
    const analyser = {
      fftSize: 0,
      frequencyBinCount: 128,
      connect: vi.fn(),
      disconnect: vi.fn(),
      getByteFrequencyData: vi.fn(),
    };
    this.analysers.push(analyser);
    return analyser;
  };
  createMediaStreamSource = (): FakeNode => {
    const source = { connect: vi.fn(), disconnect: vi.fn() };
    this.mediaSources.push(source);
    return source;
  };
  close = vi.fn(async () => undefined);
}

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
}

interface RawFrame {
  v: number;
  t: string;
  d: Record<string, unknown>;
}

const getUserMedia = vi.fn(
  async () => new FakeStream([new FakeTrack("audio")]),
);
const getDisplayMedia = vi.fn(
  async (_constraints: unknown) => new FakeStream([new FakeTrack("video")]),
);
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

function rtcFrames(sock: MockWebSocket): RawFrame[] {
  return sock.sent
    .map((raw) => JSON.parse(raw) as RawFrame)
    .filter((frame) => frame.t === "rtc.signal");
}

/** Drains microtasks + zero-delay timers so async session work settles. */
async function settle(): Promise<void> {
  for (let i = 0; i < 6; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

function conversation(
  conversationId: number,
  overrides: Partial<Conversation> = {},
): Conversation {
  return {
    conversationId,
    peerUserId: "u-peer",
    peerUsername: "peer",
    peerDisplayName: "Peer",
    lastMessagePreview: null,
    lastActivityAt: "",
    unread: 0,
    lastSeenSeq: 0,
    maxSeq: 0,
    peerTypingUntil: null,
    ...overrides,
  };
}

async function setup(): Promise<{
  calls: ReturnType<typeof useCallStore>;
  sock: MockWebSocket;
}> {
  const pinia = createPinia();
  setActivePinia(pinia);
  const auth = useAuthStore();
  auth.accessToken = "tok";
  auth.user = { userId: "u-me", username: "me", uid: 1 };
  useWsStore();
  await useWsStore().connect();
  const sock = lastSocket();
  sock.serverOpen();
  await settle();
  return { calls: useCallStore(), sock };
}

function seedConversation(overrides: Partial<Conversation> = {}): void {
  useWsStore().conversations.push(conversation(1, overrides));
}

function incomingInvite(callId = "call-1"): void {
  useCallStore().handleSignal({
    conversation_id: 1,
    call_id: callId,
    kind: "invite",
    media: "audio",
    from: { user_id: "u-peer", username: "peer", display_name: "Peer" },
  });
}

beforeEach(() => {
  localStorage.clear();
  vi.clearAllMocks();
  FakePeerConnection.instances = [];
  FakeAudioContext.instances = [];
  MockWebSocket.instances = [];
  resetLocalMic();
  resetIceServerCache();
  vi.stubGlobal("WebSocket", MockWebSocket);
  vi.stubGlobal("RTCPeerConnection", FakePeerConnection);
  vi.stubGlobal("MediaStream", FakeStream);
  vi.stubGlobal("AudioContext", FakeAudioContext);
  Object.defineProperty(navigator, "mediaDevices", {
    configurable: true,
    value: { getUserMedia, getDisplayMedia },
  });
  fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/api/auth/ws-ticket"))
      return jsonResponse(200, { ticket: "t1" });
    if (url.includes("/api/rtc/config"))
      return jsonResponse(200, { ice_servers: [] });
    if (url.includes("/api/conversations")) return jsonResponse(200, []);
    return jsonResponse(200, {});
  });
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  resetLocalMic();
  resetIceServerCache();
  vi.unstubAllGlobals();
});

describe("call store — 1:1 lifecycle", () => {
  it("an inbound invite moves the store to incoming and rings", async () => {
    const { calls } = await setup();
    seedConversation();

    incomingInvite();

    expect(calls.status).toBe("incoming");
    expect(calls.isInCall).toBe(true);
    expect(calls.peer).toMatchObject({ userId: "u-peer", username: "peer" });
    expect(calls.conversationId).toBe(1);
  });

  it("startCall sends an invite frame and enters outgoing", async () => {
    const { calls, sock } = await setup();
    seedConversation();

    calls.startCall(1);

    expect(calls.status).toBe("outgoing");
    const invite = rtcFrames(sock).find((f) => f.d.kind === "invite");
    expect(invite?.d).toMatchObject({
      conversation_id: 1,
      kind: "invite",
      media: "audio",
      to_user_id: "u-peer",
    });
  });

  it("startCall opens the call conversation for the caller", async () => {
    const { calls } = await setup();
    seedConversation();
    expect(useWsStore().activeConversationId).toBeNull();

    calls.startCall(1);

    expect(useWsStore().activeConversationId).toBe(1);
  });

  it("acceptIncoming brings the callee into the call conversation", async () => {
    const { calls } = await setup();
    seedConversation();
    incomingInvite();
    expect(useWsStore().activeConversationId).toBeNull();

    await calls.acceptIncoming();

    // The callee must land in the thread so the shared-screen stage is visible.
    expect(useWsStore().activeConversationId).toBe(1);
  });

  it("the callee reaches active from its own connection-state event", async () => {
    const { calls } = await setup();
    seedConversation();
    incomingInvite();
    await calls.acceptIncoming();
    expect(calls.status).toBe("connecting");

    calls.onConnectionState("u-peer", "connected");

    expect(calls.status).toBe("active");
  });

  it("the impolite side (smaller id) offers once the peer accepts", async () => {
    const { calls, sock } = await setup();
    seedConversation();
    calls.startCall(1);

    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "accept",
      media: "audio",
      from: { user_id: "u-peer", username: "peer" },
    });
    await settle();

    expect(Object.keys(calls.sessions)).toEqual(["u-peer"]);
    const offer = rtcFrames(sock).find((f) => f.d.sdp_type === "offer");
    expect(offer?.d).toMatchObject({
      kind: "sdp",
      sdp_type: "offer",
      to_user_id: "u-peer",
      sdp: "offer-sdp",
    });
  });

  it("the polite side (larger id) answers instead of offering", async () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    const auth = useAuthStore();
    auth.accessToken = "tok";
    auth.user = { userId: "u-zzz", username: "zed", uid: 2 };
    await useWsStore().connect();
    const sock = lastSocket();
    sock.serverOpen();
    await settle();
    const calls = useCallStore();
    seedConversation();

    calls.startCall(1);
    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "accept",
      media: "audio",
      from: { user_id: "u-peer", username: "peer" },
    });
    await settle();

    // We are polite: no local offer.
    expect(rtcFrames(sock).some((f) => f.d.sdp_type === "offer")).toBe(false);

    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "sdp",
      sdp_type: "offer",
      sdp: "remote-offer",
      from: { user_id: "u-peer", username: "peer" },
    });
    await settle();

    const answer = rtcFrames(sock).find((f) => f.d.sdp_type === "answer");
    expect(answer?.d).toMatchObject({ kind: "sdp", sdp_type: "answer" });
  });

  it("reject ends an outgoing call with a toast", async () => {
    const { calls } = await setup();
    seedConversation();
    calls.startCall(1);

    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "reject",
      reason: "declined",
      from: { user_id: "u-peer", username: "peer" },
    });

    expect(calls.status).toBe("idle");
    expect(calls.toast?.key).toBe("call.toastRejected");
  });

  it("hangup sends a hangup frame and tears everything down", async () => {
    const { calls, sock } = await setup();
    seedConversation();
    calls.startCall(1);
    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "accept",
      media: "audio",
      from: { user_id: "u-peer", username: "peer" },
    });
    await settle();

    calls.hangup();

    expect(calls.status).toBe("idle");
    expect(Object.keys(calls.sessions)).toEqual([]);
    const hangup = rtcFrames(sock).find((f) => f.d.kind === "hangup");
    expect(hangup?.d).toMatchObject({ kind: "hangup", conversation_id: 1 });
  });

  it("a second invite while busy is auto-rejected with reason busy", async () => {
    const { calls, sock } = await setup();
    seedConversation();
    calls.startCall(1);
    const before = calls.status;

    calls.handleSignal({
      conversation_id: 2,
      call_id: "other-call",
      kind: "invite",
      media: "audio",
      from: { user_id: "u-other", username: "other" },
    });

    expect(calls.status).toBe(before);
    const reject = rtcFrames(sock).find((f) => f.d.kind === "reject");
    expect(reject?.d).toMatchObject({
      kind: "reject",
      reason: "busy",
      to_user_id: "u-other",
      call_id: "other-call",
    });
  });

  it("ended tears the call down and toasts", async () => {
    const { calls } = await setup();
    seedConversation();
    incomingInvite();
    await calls.acceptIncoming();
    await settle();

    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "ended",
    });

    expect(calls.status).toBe("idle");
    expect(calls.toast?.key).toBe("call.toastEnded");
    expect(Object.keys(calls.sessions)).toEqual([]);
  });
});

describe("call store — group mesh", () => {
  it("group startCall enters active and a roster creates a session per peer", async () => {
    const { calls } = await setup();
    seedConversation({ kind: "group", name: "Team", peerUserId: "" });
    calls.startCall(1);
    expect(calls.status).toBe("active");
    expect(calls.isGroup).toBe(true);

    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "roster",
      participants: [
        { user_id: "u-me", username: "me" },
        { user_id: "u-a", username: "a" },
        { user_id: "u-b", username: "b" },
      ],
    });
    await settle();

    expect(Object.keys(calls.sessions).sort()).toEqual(["u-a", "u-b"]);
    expect(calls.participants).toHaveLength(3);
  });

  it("a roster drop tears down the departed peer's session", async () => {
    const { calls } = await setup();
    seedConversation({ kind: "group", name: "Team", peerUserId: "" });
    calls.startCall(1);
    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "roster",
      participants: [
        { user_id: "u-me", username: "me" },
        { user_id: "u-a", username: "a" },
        { user_id: "u-b", username: "b" },
      ],
    });
    await settle();
    expect(Object.keys(calls.sessions).sort()).toEqual(["u-a", "u-b"]);

    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "roster",
      participants: [
        { user_id: "u-me", username: "me" },
        { user_id: "u-a", username: "a" },
      ],
    });
    await settle();

    expect(Object.keys(calls.sessions)).toEqual(["u-a"]);
  });

  it("secret conversations never start a call", async () => {
    const { calls } = await setup();
    seedConversation({ kind: "secret" });

    calls.startCall(1);

    expect(calls.status).toBe("idle");
    expect(calls.conversationId).toBeNull();
  });
});

describe("call store — screen share", () => {
  async function activeDm(): Promise<{
    calls: ReturnType<typeof useCallStore>;
    sock: MockWebSocket;
  }> {
    const { calls, sock } = await setup();
    seedConversation();
    calls.startCall(1);
    calls.handleSignal({
      conversation_id: 1,
      call_id: calls.callId ?? "",
      kind: "accept",
      media: "audio",
      from: { user_id: "u-peer", username: "peer" },
    });
    await settle();
    return { calls, sock };
  }

  it("startShare captures at the chosen quality and attaches to every leg", async () => {
    const { calls } = await activeDm();

    await calls.startShare(shareQuality("1080p", 60));

    expect(getDisplayMedia).toHaveBeenCalledWith({
      video: {
        width: { ideal: 1920 },
        height: { ideal: 1080 },
        frameRate: { ideal: 60 },
      },
      audio: false,
    });
    expect(calls.sharing).toBe(true);
    expect(calls.localScreenStream).not.toBeNull();
  });

  it("changeShareQuality replaces the outbound video track", async () => {
    const { calls } = await activeDm();
    await calls.startShare(shareQuality("720p", 30));
    const pc = FakePeerConnection.instances[0]!;
    const videoSender = pc
      .getSenders()
      .find((s) => s.transceiver?.direction === "sendonly")!;

    await calls.changeShareQuality(shareQuality("480p", 15));

    expect(getDisplayMedia).toHaveBeenCalledTimes(2);
    const newTrack = calls.localScreenStream?.getVideoTracks()[0];
    expect(videoSender.replaceTrack).toHaveBeenLastCalledWith(newTrack);
  });

  it("sharing emits a fresh SDP offer (renegotiation)", async () => {
    const { calls, sock } = await activeDm();
    const before = rtcFrames(sock).filter(
      (f) => f.d.sdp_type === "offer",
    ).length;

    await calls.startShare(shareQuality("720p", 30));
    await settle();

    const after = rtcFrames(sock).filter(
      (f) => f.d.sdp_type === "offer",
    ).length;
    expect(after).toBeGreaterThan(before);
  });

  it("stopShare detaches and stops the local capture", async () => {
    const { calls } = await activeDm();
    await calls.startShare(shareQuality("720p", 30));
    const track = calls.localScreenStream!.getVideoTracks()[0] as unknown as {
      stopped: boolean;
    };

    await calls.stopShare();

    expect(calls.sharing).toBe(false);
    expect(calls.localScreenStream).toBeNull();
    expect(track.stopped).toBe(true);
  });
});

describe("call store — remote audio sink", () => {
  it("routes a remote audio track through analyser → destination", () => {
    const pinia = createPinia();
    setActivePinia(pinia);
    useAuthStore().user = { userId: "u-me", username: "me", uid: 1 };
    const calls = useCallStore();
    const track = new FakeTrack("audio") as unknown as MediaStreamTrack;

    calls.onRemoteAudioStream("u-peer", track);

    const ctx = FakeAudioContext.instances.find(
      (instance) => instance.mediaSources.length > 0,
    );
    expect(ctx).toBeDefined();
    const source = ctx!.mediaSources[0]!;
    const analyser = ctx!.analysers[0]!;
    // source → analyser → destination: the analyser powers speaking detection
    // and the destination is the actual audible sink.
    expect(source.connect).toHaveBeenCalledWith(analyser);
    expect(analyser.connect).toHaveBeenCalledWith(ctx!.destination);
  });
});

describe("call store — teardown on ws close", () => {
  it("drops the call when the socket closes", async () => {
    const { calls, sock } = await setup();
    seedConversation();
    calls.startCall(1);
    expect(calls.status).toBe("outgoing");

    sock.close();
    await settle();

    calls.handleWsClosed();
    expect(calls.status).toBe("idle");
  });
});
