import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  CallSession,
  DEFAULT_SHARE_QUALITY,
  SHARE_FRAMERATES,
  SHARE_RESOLUTIONS,
  captureScreen,
  localMicRefCount,
  loadIceServers,
  resetIceServerCache,
  resetLocalMic,
  shareQuality,
} from "./callSession";
import type { RtcOutboundSignal } from "./callSession";

// ---------------------------------------------------------------------------
// jsdom has no WebRTC — minimal behaviour-complete doubles.
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

  readonly config: RTCConfiguration;
  onicecandidate: ((ev: { candidate: unknown }) => void) | null = null;
  ontrack: ((ev: { streams: unknown[]; track: FakeTrack }) => void) | null =
    null;
  onconnectionstatechange: (() => void) | null = null;
  onnegotiationneeded: (() => void) | null = null;
  signalingState: RTCSignalingState = "stable";
  connectionState: RTCPeerConnectionState = "new";
  localDescription: RTCSessionDescriptionInit | null = null;
  iceCandidates: unknown[] = [];
  closed = false;

  private readonly senders: FakeSender[] = [];

  constructor(config: RTCConfiguration) {
    this.config = config;
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

  createOffer = vi.fn(async (): Promise<RTCSessionDescriptionInit> => ({
    type: "offer",
    sdp: "offer-sdp",
  }));

  createAnswer = vi.fn(async (): Promise<RTCSessionDescriptionInit> => ({
    type: "answer",
    sdp: "answer-sdp",
  }));

  setLocalDescription = vi.fn(
    async (description?: RTCSessionDescriptionInit) => {
      this.localDescription = description ?? {
        type: "offer",
        sdp: "offer-sdp",
      };
      this.signalingState =
        this.localDescription.type === "offer" ? "have-local-offer" : "stable";
    },
  );

  setRemoteDescription = vi.fn(
    async (description: RTCSessionDescriptionInit) => {
      this.signalingState =
        description.type === "offer" ? "have-remote-offer" : "stable";
    },
  );

  addIceCandidate = vi.fn(async (candidate?: unknown) => {
    this.iceCandidates.push(candidate ?? null);
  });

  getSenders = vi.fn((): FakeSender[] => [...this.senders]);

  close = vi.fn((): void => {
    this.closed = true;
    this.connectionState = "closed";
  });

  /** Test helper: simulate a candidate discovered by the ICE agent. */
  fireCandidate(candidate: unknown): void {
    this.onicecandidate?.({ candidate });
  }

  /** Test helper: simulate the connection reaching a state. */
  fireConnectionState(state: RTCPeerConnectionState): void {
    this.connectionState = state;
    this.onconnectionstatechange?.();
  }

  /** Test helper: simulate the browser's renegotiation signal. */
  fireNegotiationNeeded(): void {
    this.onnegotiationneeded?.();
  }
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

function makeSession(
  overrides: Partial<{
    selfUserId: string;
    remoteUserId: string;
    conversationId: number;
    callId: string;
    onRemoteStream: (userId: string, stream: MediaStream) => void;
    onRemoteAudioStream: (userId: string, track: MediaStreamTrack) => void;
  }> = {},
): { session: CallSession; emitted: RtcOutboundSignal[] } {
  const emitted: RtcOutboundSignal[] = [];
  const session = new CallSession({
    selfUserId: overrides.selfUserId ?? "1",
    remoteUserId: overrides.remoteUserId ?? "2",
    conversationId: overrides.conversationId ?? 7,
    callId: overrides.callId ?? "call-1",
    iceServers: [],
    emit: (signal) => emitted.push(signal),
    onRemoteStream: overrides.onRemoteStream,
    onRemoteAudioStream: overrides.onRemoteAudioStream,
  });
  return { session, emitted };
}

function lastPc(): FakePeerConnection {
  const pc = FakePeerConnection.instances.at(-1);
  if (pc === undefined) throw new Error("no FakePeerConnection instance");
  return pc;
}

/** Narrows a `MediaStream` video track back to our FakeTrack for assertions. */
function vtrack(stream: MediaStream): FakeTrack {
  return stream.getVideoTracks()[0] as unknown as FakeTrack;
}

beforeEach(() => {
  vi.clearAllMocks();
  FakePeerConnection.instances = [];
  resetLocalMic();
  resetIceServerCache();
  vi.stubGlobal("RTCPeerConnection", FakePeerConnection);
  vi.stubGlobal("MediaStream", FakeStream);
  Object.defineProperty(navigator, "mediaDevices", {
    configurable: true,
    value: { getUserMedia, getDisplayMedia },
  });
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  resetLocalMic();
  resetIceServerCache();
  vi.unstubAllGlobals();
});

describe("share quality presets", () => {
  it("offers 480p–1440p and 15/30/60fps with a 720p/30 default", () => {
    expect(SHARE_RESOLUTIONS.map((r) => r.label)).toEqual([
      "480p",
      "720p",
      "1080p",
      "1440p",
    ]);
    expect(SHARE_FRAMERATES).toEqual([15, 30, 60]);
    expect(DEFAULT_SHARE_QUALITY).toMatchObject({
      label: "720p",
      width: 1280,
      height: 720,
      frameRate: 30,
    });
  });

  it("shareQuality composes the chosen resolution with the framerate", () => {
    expect(shareQuality("1080p", 60)).toEqual({
      label: "1080p",
      width: 1920,
      height: 1080,
      frameRate: 60,
    });
    // Unknown label falls back to 720p.
    expect(shareQuality("nope", 15)).toMatchObject({ label: "720p" });
  });
});

describe("loadIceServers", () => {
  it("fetches /api/rtc/config with the bearer token and caches the result", async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(200, { ice_servers: [{ urls: "stun:stun.example" }] }),
    );
    const servers = await loadIceServers("tok");
    expect(servers).toEqual([{ urls: "stun:stun.example" }]);
    expect(fetchMock).toHaveBeenCalledTimes(1);

    // Second call is served from cache.
    await loadIceServers("tok");
    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [, init] = fetchMock.mock.calls[0]!;
    expect((init as { headers: Headers }).headers.get("Authorization")).toBe(
      "Bearer tok",
    );
  });
});

describe("captureScreen", () => {
  it("passes the chosen width/height/frameRate into getDisplayMedia", async () => {
    await captureScreen(shareQuality("1440p", 60));
    expect(getDisplayMedia).toHaveBeenCalledWith({
      video: {
        width: { ideal: 2560 },
        height: { ideal: 1440 },
        frameRate: { ideal: 60 },
      },
      audio: false,
    });
  });
});

describe("CallSession — negotiation", () => {
  it("the smaller user_id is the impolite initiator and sends the first offer", async () => {
    const { session, emitted } = makeSession({
      selfUserId: "aaa",
      remoteUserId: "zzz",
    });
    expect(session.polite).toBe(false);
    await session.start();
    const offer = emitted.find((s) => s.sdp_type === "offer");
    expect(offer).toBeDefined();
    expect(offer?.kind).toBe("sdp");
    expect(offer?.to_user_id).toBe("zzz");
    expect(offer?.sdp).toBe("offer-sdp");
  });

  it("the larger user_id is polite and waits for the remote offer", async () => {
    const { session, emitted } = makeSession({
      selfUserId: "zzz",
      remoteUserId: "aaa",
    });
    expect(session.polite).toBe(true);
    await session.start();
    expect(emitted.some((s) => s.sdp_type === "offer")).toBe(false);

    // Inbound offer → answers.
    await session.handleSignal({
      conversation_id: 7,
      call_id: "call-1",
      kind: "sdp",
      sdp_type: "offer",
      sdp: "remote-offer",
    });
    const answer = emitted.find((s) => s.sdp_type === "answer");
    expect(answer).toBeDefined();
    expect(answer?.sdp).toBe("answer-sdp");
  });

  it("trickles ICE candidates outbound and feeds inbound ones", async () => {
    const { session, emitted } = makeSession();
    const pc = lastPc();
    pc.fireCandidate({
      sdpMid: "0",
      sdpMLineIndex: 0,
      toJSON: () => ({
        candidate: "candidate:1 1 udp 1 1.2.3.4 9 typ host",
        sdpMid: "0",
        sdpMLineIndex: 0,
      }),
    });
    const ice = emitted.find((s) => s.kind === "ice");
    expect(ice?.candidate).toContain("candidate:1");
    expect(ice?.sdp_mid).toBe("0");

    await session.handleSignal({
      conversation_id: 7,
      call_id: "call-1",
      kind: "ice",
      candidate: JSON.stringify({ candidate: "remote-cand", sdpMid: "0" }),
    });
    expect(pc.addIceCandidate).toHaveBeenCalledTimes(1);
  });
});

describe("CallSession — screen share", () => {
  it("startScreenShare captures at the given quality and attaches the track", async () => {
    const { session } = makeSession();
    const stream = await session.startScreenShare(shareQuality("1080p", 30));

    expect(getDisplayMedia).toHaveBeenCalledWith({
      video: {
        width: { ideal: 1920 },
        height: { ideal: 1080 },
        frameRate: { ideal: 30 },
      },
      audio: false,
    });
    const pc = lastPc();
    expect(pc.addTransceiver).toHaveBeenCalledWith("video", {
      direction: "sendonly",
    });
    const sender = pc.getSenders()[0]!;
    expect(sender.replaceTrack).toHaveBeenCalledWith(
      stream.getVideoTracks()[0],
    );
    expect(session.screenVideoTrack).toBe(stream.getVideoTracks()[0]);
  });

  it("changing quality replaces the outbound video track", async () => {
    const { session } = makeSession();
    const first = await session.startScreenShare(shareQuality("480p", 15));
    const sender = lastPc().getSenders()[0]!;

    const next = new FakeStream([new FakeTrack("video")]);
    await session.attachScreenTrack(
      next.getVideoTracks()[0]! as unknown as MediaStreamTrack,
    );

    expect(sender.replaceTrack).toHaveBeenLastCalledWith(
      next.getVideoTracks()[0],
    );
    expect(session.screenVideoTrack).toBe(next.getVideoTracks()[0]);
    // The old capture is not stopped by a replace — the store owns that.
    expect(vtrack(first).stopped).toBe(false);
  });

  it("stopScreenShare detaches (replaceTrack null) and stops the capture", async () => {
    const { session } = makeSession();
    const stream = await session.startScreenShare(DEFAULT_SHARE_QUALITY);
    const sender = lastPc().getSenders()[0]!;
    const track = vtrack(stream);

    await session.stopScreenShare();

    expect(sender.replaceTrack).toHaveBeenLastCalledWith(null);
    expect(track.stopped).toBe(true);
    expect(session.screenVideoTrack).toBeNull();
  });

  it("detachScreenTrack leaves the shared capture running", async () => {
    const { session } = makeSession();
    const stream = await session.startScreenShare(DEFAULT_SHARE_QUALITY);
    const track = vtrack(stream);
    await session.detachScreenTrack();
    expect(track.stopped).toBe(false);
  });
});

describe("CallSession — local mic refcount", () => {
  it("shares one getUserMedia stream across mesh sessions until the last closes", async () => {
    const a = makeSession({ selfUserId: "1", remoteUserId: "2" }).session;
    const b = makeSession({ selfUserId: "1", remoteUserId: "3" }).session;

    await a.start();
    await b.start();
    expect(getUserMedia).toHaveBeenCalledTimes(1);
    expect(localMicRefCount()).toBe(2);

    const stream = FakePeerConnection.instances[0]!.getSenders()[0]!.track;
    expect(stream?.stopped).toBe(false);

    a.close();
    expect(localMicRefCount()).toBe(1);
    // Closing one mesh leg must NOT stop the shared mic.
    expect(stream?.stopped).toBe(false);

    b.close();
    expect(localMicRefCount()).toBe(0);
    expect(stream?.stopped).toBe(true);
  });

  it("setMuted toggles the local audio track enabled flag", async () => {
    const { session } = makeSession();
    await session.start();
    const track = lastPc().getSenders()[0]!.track;
    session.setMuted(true);
    expect(track?.enabled).toBe(false);
    session.setMuted(false);
    expect(track?.enabled).toBe(true);
  });
});

describe("CallSession — remote audio sink + renegotiation", () => {
  it("surfaces an inbound audio track through onRemoteAudioStream", () => {
    const audioSink = vi.fn();
    const streamSink = vi.fn();
    const { session } = makeSession({
      onRemoteAudioStream: audioSink,
      onRemoteStream: streamSink,
    });
    const pc = lastPc();
    const track = new FakeTrack("audio");
    const stream = new FakeStream([track]);

    pc.ontrack?.({ streams: [stream], track });

    // The peer's voice is handed up as its own signal so the store can build
    // a real audio sink; the full stream still feeds the video/remote cache.
    expect(audioSink).toHaveBeenCalledWith("2", track);
    expect(streamSink).toHaveBeenCalledWith("2", stream);
    expect(session.remoteStream).toBe(stream);
  });

  it("ignores negotiationneeded until the initial handshake connects", async () => {
    const { emitted } = makeSession();
    const pc = lastPc();

    pc.fireNegotiationNeeded();
    for (let i = 0; i < 5; i += 1) await Promise.resolve();

    expect(emitted.some((s) => s.sdp_type === "offer")).toBe(false);
  });

  it("re-offers on negotiationneeded once connected (screen-share trigger)", async () => {
    const { session, emitted } = makeSession();
    const pc = lastPc();
    await session.start(); // impolite peer sends the initial offer
    pc.fireConnectionState("connected");

    emitted.length = 0;
    pc.fireNegotiationNeeded();
    for (let i = 0; i < 10; i += 1) await Promise.resolve();

    const offer = emitted.find((s) => s.sdp_type === "offer");
    expect(offer).toBeDefined();
    expect(offer?.kind).toBe("sdp");
  });
});
