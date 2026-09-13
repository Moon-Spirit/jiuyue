/**
 * M14 WebRTC call engine — ONE `CallSession` = one peer connection to one
 * remote user for one call. The call store owns the call state machine and
 * creates/destroys a session per remote participant (group calls mesh).
 *
 * Negotiation follows the perfect-negotiation pattern
 * (https://w3c.github.io/webrtc-pc/#perfect-negotiation-example):
 * - polite/impolite is derived DETERMINISTICALLY from the two user ids: the
 *   SMALLER `user_id` is the impolite initiator (sends the first offer), the
 *   larger is polite (yields on glare). Every pair in a mesh negotiates
 *   independently, so both sides compute the same roles without signalling.
 * - trickle ICE: every local candidate is emitted as an `rtc.signal` ice frame
 *   and inbound candidates are fed straight into `addIceCandidate`.
 * - renegotiation (screen share add/remove/quality) is initiated explicitly by
 *   the side that changed its media, so there is no reliance on
 *   `onnegotiationneeded` timing (which is hard to test deterministically).
 *
 * The local microphone is a process-wide refcounted stream: mesh sessions
 * share ONE `getUserMedia` result, and the last session to close stops the
 * tracks. A single session closing must never mute the others.
 */

import { apiRequest } from "../api/client";
import type { RtcSignal, RtcSignalKind } from "../protocol/frames";

// ---------------------------------------------------------------------------
// Screen-share quality presets
// ---------------------------------------------------------------------------

export interface ShareResolution {
  /** Stable locale-neutral label; the UI maps it to a localized string. */
  label: string;
  w: number;
  h: number;
}

/** Resolution presets offered by the share-quality picker. */
export const SHARE_RESOLUTIONS: readonly ShareResolution[] = [
  { label: "480p", w: 854, h: 480 },
  { label: "720p", w: 1280, h: 720 },
  { label: "1080p", w: 1920, h: 1080 },
  { label: "1440p", w: 2560, h: 1440 },
] as const;

/** Framerate presets offered by the share-quality picker. */
export const SHARE_FRAMERATES: readonly number[] = [15, 30, 60] as const;

export interface ShareQuality {
  /** Resolution label ("720p"…). */
  label: string;
  width: number;
  height: number;
  frameRate: number;
}

/** Default capture quality: 720p @ 30fps. */
export const DEFAULT_SHARE_QUALITY: ShareQuality = {
  label: "720p",
  width: 1280,
  height: 720,
  frameRate: 30,
};

/** Compose a {@link ShareQuality} from a resolution label + framerate. */
export function shareQuality(label: string, frameRate: number): ShareQuality {
  const resolution =
    SHARE_RESOLUTIONS.find((r) => r.label === label) ?? SHARE_RESOLUTIONS[1]!;
  return {
    label: resolution.label,
    width: resolution.w,
    height: resolution.h,
    frameRate,
  };
}

// ---------------------------------------------------------------------------
// ICE server config (cached for the tab)
// ---------------------------------------------------------------------------

let cachedIceServers: RTCIceServer[] | null = null;

/**
 * Fetches the RTCPeerConnection ICE config from `GET /api/rtc/config`.
 * Cached after the first successful fetch (ICE config is static per deploy).
 */
export async function loadIceServers(token: string): Promise<RTCIceServer[]> {
  if (cachedIceServers !== null) return cachedIceServers;
  const res = await apiRequest<{ ice_servers?: RTCIceServer[] }>(
    "/api/rtc/config",
    { accessToken: token },
  );
  cachedIceServers = Array.isArray(res.ice_servers) ? res.ice_servers : [];
  return cachedIceServers;
}

/** Test/logout hygiene: drop the cached ICE config. */
export function resetIceServerCache(): void {
  cachedIceServers = null;
}

// ---------------------------------------------------------------------------
// Shared, refcounted local microphone
// ---------------------------------------------------------------------------

interface LocalMic {
  stream: MediaStream;
  refCount: number;
}

let localMic: LocalMic | null = null;

/**
 * Returns the process-wide mic stream, acquiring it on first use. Every
 * returned stream MUST be paired with exactly one {@link releaseLocalMic}.
 */
export async function acquireLocalMic(): Promise<MediaStream> {
  if (localMic === null) {
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
    });
    localMic = { stream, refCount: 0 };
  }
  localMic.refCount += 1;
  return localMic.stream;
}

/**
 * Drops one mic reference. The underlying tracks are stopped only when the
 * LAST session releases (refcount hits 0), so closing one mesh leg never
 * mutes the others.
 */
export function releaseLocalMic(): void {
  if (localMic === null) return;
  localMic.refCount -= 1;
  if (localMic.refCount <= 0) {
    for (const track of localMic.stream.getTracks()) track.stop();
    localMic = null;
  }
}

/** Current mic refcount (0 = no acquisition). Exposed for tests. */
export function localMicRefCount(): number {
  return localMic?.refCount ?? 0;
}

/** Test/logout hygiene: force-release the shared mic. */
export function resetLocalMic(): void {
  if (localMic !== null) {
    for (const track of localMic.stream.getTracks()) track.stop();
  }
  localMic = null;
}

// ---------------------------------------------------------------------------
// Screen capture
// ---------------------------------------------------------------------------

/**
 * Captures the screen (video only) at the requested constraints. Kept separate
 * from {@link CallSession} so a group-call sharer captures ONCE and attaches
 * the same track to every mesh session.
 */
export async function captureScreen(
  quality: ShareQuality,
): Promise<MediaStream> {
  return navigator.mediaDevices.getDisplayMedia({
    video: {
      width: { ideal: quality.width },
      height: { ideal: quality.height },
      frameRate: { ideal: quality.frameRate },
    },
    audio: false,
  });
}

// ---------------------------------------------------------------------------
// Signals emitted by a session (the call store wraps them in rtc.signal frames)
// ---------------------------------------------------------------------------

export interface RtcOutboundSignal {
  kind: RtcSignalKind;
  to_user_id?: string;
  sdp_type?: "offer" | "answer";
  sdp?: string;
  candidate?: string;
  sdp_mid?: string;
  sdp_mline_index?: number;
}

export interface CallSessionOptions {
  selfUserId: string;
  remoteUserId: string;
  conversationId: number;
  callId: string;
  iceServers?: RTCIceServer[];
  /** Sends one signalling payload (the store adds envelope + routing ids). */
  emit: (signal: RtcOutboundSignal) => void;
  onRemoteStream?: (remoteUserId: string, stream: MediaStream) => void;
  /**
   * Fired for every inbound AUDIO track. The store uses this to build the
   * remote audio sink (source → analyser → destination): without it the peer's
   * voice is decoded but never routed anywhere and stays inaudible.
   */
  onRemoteAudioStream?: (remoteUserId: string, track: MediaStreamTrack) => void;
  onConnectionState?: (
    remoteUserId: string,
    state: RTCPeerConnectionState,
  ) => void;
}

/**
 * One peer connection. Lifecycle: `new CallSession(...)` → `start()` →
 * `handleSignal(...)`* → `close()`.
 */
export class CallSession {
  readonly selfUserId: string;
  readonly remoteUserId: string;
  readonly conversationId: number;
  readonly callId: string;
  /** True when this side yields on offer glare (larger user_id). */
  readonly polite: boolean;
  readonly pc: RTCPeerConnection;

  private readonly emit: (signal: RtcOutboundSignal) => void;
  private readonly onRemoteStream?: (
    remoteUserId: string,
    stream: MediaStream,
  ) => void;
  private readonly onRemoteAudioStream?: (
    remoteUserId: string,
    track: MediaStreamTrack,
  ) => void;
  private readonly onConnectionState?: (
    remoteUserId: string,
    state: RTCPeerConnectionState,
  ) => void;

  private localStream: MediaStream | null = null;
  private audioSender: RTCRtpSender | null = null;
  private videoSender: RTCRtpSender | null = null;
  private videoTransceiver: RTCRtpTransceiver | null = null;
  private screenTrack: MediaStreamTrack | null = null;
  private remote: MediaStream | null = null;

  private closed = false;
  private makingOffer = false;
  private ignoreOffer = false;
  private isSettingRemoteAnswerPending = false;
  /**
   * `onnegotiationneeded` is the canonical renegotiation trigger. It is gated
   * until the INITIAL handshake has connected so the deterministic impolite
   * offer is the only one at call setup (no glare), then it drives every later
   * change (screen-share add/remove/quality) automatically.
   */
  private negotiationEnabled = false;
  /** Coalesces concurrent negotiation triggers into a single re-offer. */
  private negotiationPromise: Promise<void> | null = null;

  constructor(options: CallSessionOptions) {
    this.selfUserId = options.selfUserId;
    this.remoteUserId = options.remoteUserId;
    this.conversationId = options.conversationId;
    this.callId = options.callId;
    this.emit = options.emit;
    this.onRemoteStream = options.onRemoteStream;
    this.onRemoteAudioStream = options.onRemoteAudioStream;
    this.onConnectionState = options.onConnectionState;
    // Deterministic roles: the SMALLER user_id is the impolite initiator.
    this.polite = this.selfUserId > this.remoteUserId;
    this.pc = new RTCPeerConnection({
      iceServers: options.iceServers ?? [],
    });
    this.wirePeerEvents();
  }

  get isClosed(): boolean {
    return this.closed;
  }

  get remoteStream(): MediaStream | null {
    return this.remote;
  }

  get screenVideoTrack(): MediaStreamTrack | null {
    return this.screenTrack;
  }

  private wirePeerEvents(): void {
    this.pc.onicecandidate = (ev: RTCPeerConnectionIceEvent) => {
      const candidate = ev.candidate;
      if (candidate === null) {
        // End-of-candidates: an empty string tells the peer to flush.
        this.emit({
          kind: "ice",
          to_user_id: this.remoteUserId,
          candidate: "",
        });
        return;
      }
      this.emit({
        kind: "ice",
        to_user_id: this.remoteUserId,
        candidate: JSON.stringify(candidate),
        sdp_mid: candidate.sdpMid ?? undefined,
        sdp_mline_index: candidate.sdpMLineIndex ?? undefined,
      });
    };
    this.pc.ontrack = (ev: RTCTrackEvent) => {
      const stream = ev.streams[0];
      if (stream !== undefined) {
        this.remote = stream;
      } else {
        if (this.remote === null) this.remote = new MediaStream();
        this.remote.addTrack(ev.track);
      }
      // Audio must be surfaced separately so the store can build an audible
      // sink (a MediaStreamAudioSourceNode). `ev.streams` is EMPTY for tracks
      // sent via addTransceiver+replaceTrack, so the track itself is emitted.
      if (ev.track.kind === "audio") {
        this.onRemoteAudioStream?.(this.remoteUserId, ev.track);
      }
      if (this.remote !== null) {
        this.onRemoteStream?.(this.remoteUserId, this.remote);
      }
    };
    this.pc.onnegotiationneeded = () => {
      if (this.closed || !this.negotiationEnabled) return;
      void this.scheduleNegotiation();
    };
    this.pc.onconnectionstatechange = () => {
      if (this.pc.connectionState === "connected") {
        // The initial handshake is done: allow media-driven renegotiation.
        this.negotiationEnabled = true;
      }
      this.onConnectionState?.(this.remoteUserId, this.pc.connectionState);
    };
  }

  /**
   * Acquires the shared mic, publishes the local audio track, and — when this
   * side is the deterministically-chosen impolite initiator — sends the first
   * offer. The polite side sends nothing and waits for that offer.
   */
  async start(): Promise<void> {
    if (this.closed) return;
    const stream = await acquireLocalMic();
    this.localStream = stream;
    const audioTrack = stream.getAudioTracks()[0];
    if (audioTrack !== undefined) {
      this.audioSender = this.pc.addTrack(audioTrack, stream);
    }
    if (!this.polite) await this.negotiate();
  }

  /** Mutes/unmutes the shared local mic for THIS session's sender. */
  setMuted(muted: boolean): void {
    const track =
      this.audioSender?.track ?? this.localStream?.getAudioTracks()[0];
    if (track !== null && track !== undefined) track.enabled = !muted;
  }

  /** Routes an inbound sdp/ice signal into this connection. */
  async handleSignal(signal: RtcSignal): Promise<void> {
    if (this.closed) return;
    if (signal.kind === "sdp") {
      await this.handleDescription(signal);
    } else if (signal.kind === "ice") {
      await this.handleCandidate(signal);
    }
  }

  private async negotiate(): Promise<void> {
    if (this.closed) return;
    this.makingOffer = true;
    try {
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);
      const sdp = this.pc.localDescription?.sdp ?? offer.sdp;
      if (sdp !== undefined) {
        this.emit({
          kind: "sdp",
          to_user_id: this.remoteUserId,
          sdp_type: "offer",
          sdp,
        });
      }
    } finally {
      this.makingOffer = false;
    }
  }

  /**
   * Coalescing renegotiation trigger shared by `onnegotiationneeded` and the
   * explicit media-change paths (attach/detach screen track): concurrent
   * triggers produce exactly ONE offer.
   */
  private scheduleNegotiation(): Promise<void> {
    if (this.closed) return Promise.resolve();
    if (this.negotiationPromise !== null) return this.negotiationPromise;
    const promise = Promise.resolve()
      .then(() => this.negotiate())
      .finally(() => {
        if (this.negotiationPromise === promise) this.negotiationPromise = null;
      });
    this.negotiationPromise = promise;
    return promise;
  }

  private async handleDescription(signal: RtcSignal): Promise<void> {
    if (signal.sdp === undefined || signal.sdp_type === undefined) return;
    const description: RTCSessionDescriptionInit = {
      type: signal.sdp_type,
      sdp: signal.sdp,
    };
    const readyForOffer =
      !this.makingOffer &&
      (this.pc.signalingState === "stable" ||
        this.isSettingRemoteAnswerPending);
    const offerCollision = description.type === "offer" && !readyForOffer;
    // Only the impolite peer ignores a colliding offer; the polite peer always
    // rolls back and accepts the remote offer (perfect negotiation).
    this.ignoreOffer = !this.polite && offerCollision;
    if (this.ignoreOffer) return;

    this.isSettingRemoteAnswerPending = description.type === "answer";
    await this.pc.setRemoteDescription(description);
    this.isSettingRemoteAnswerPending = false;

    if (description.type === "offer") {
      const answer = await this.pc.createAnswer();
      await this.pc.setLocalDescription(answer);
      const sdp = this.pc.localDescription?.sdp ?? answer.sdp;
      if (sdp !== undefined) {
        this.emit({
          kind: "sdp",
          to_user_id: this.remoteUserId,
          sdp_type: "answer",
          sdp,
        });
      }
    }
  }

  private async handleCandidate(signal: RtcSignal): Promise<void> {
    const raw = signal.candidate;
    if (raw === undefined) return;
    try {
      if (raw === "") {
        await this.pc.addIceCandidate();
        return;
      }
      const init = JSON.parse(raw) as RTCIceCandidateInit;
      if (signal.sdp_mid !== undefined && init.sdpMid == null) {
        init.sdpMid = signal.sdp_mid;
      }
      if (signal.sdp_mline_index !== undefined && init.sdpMLineIndex == null) {
        init.sdpMLineIndex = signal.sdp_mline_index;
      }
      await this.pc.addIceCandidate(init);
    } catch (error) {
      // A candidate for an ignored (colliding) offer is expected noise.
      if (!this.ignoreOffer) throw error;
    }
  }

  /**
   * Captures the screen at `quality` and attaches it to this session. Used for
   * the single-user / first-session path; mesh peers use
   * {@link attachScreenTrack} with the shared track.
   */
  async startScreenShare(quality: ShareQuality): Promise<MediaStream> {
    const stream = await captureScreen(quality);
    const track = stream.getVideoTracks()[0];
    if (track !== undefined) await this.attachScreenTrack(track);
    return stream;
  }

  /**
   * Adds or replaces the outbound video track (lazily creating a sendonly
   * video transceiver on first share) and renegotiates. Pass a freshly
   * captured track to change quality — this is what "replaces the track".
   */
  async attachScreenTrack(track: MediaStreamTrack): Promise<void> {
    if (this.closed) return;
    if (this.videoSender === null) {
      this.videoTransceiver = this.pc.addTransceiver("video", {
        direction: "sendonly",
      });
      this.videoSender = this.videoTransceiver.sender;
    } else if (this.videoTransceiver !== null) {
      this.videoTransceiver.direction = "sendonly";
    }
    await this.videoSender.replaceTrack(track);
    this.screenTrack = track;
    await this.scheduleNegotiation();
  }

  /**
   * Stops sending video on this session (transceiver goes inactive) and
   * renegotiates, WITHOUT stopping the shared track — the track may still be
   * attached to other mesh sessions.
   */
  async detachScreenTrack(): Promise<void> {
    if (this.closed) return;
    if (this.videoSender === null) return;
    await this.videoSender.replaceTrack(null);
    if (this.videoTransceiver !== null) {
      this.videoTransceiver.direction = "inactive";
    }
    await this.scheduleNegotiation();
  }

  /** Detaches AND stops the track (single-session / owner path). */
  async stopScreenShare(): Promise<void> {
    const track = this.screenTrack;
    await this.detachScreenTrack();
    this.screenTrack = null;
    if (track !== null) {
      track.onended = null;
      track.stop();
    }
  }

  /**
   * Full teardown: detaches senders, stops the owned screen track, closes the
   * PC and releases the shared mic. Idempotent. Never stops the mic stream
   * while other mesh sessions still hold a reference.
   */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    try {
      for (const sender of this.pc.getSenders()) {
        void sender.replaceTrack(null);
      }
    } catch {
      // A partially-constructed PC must not block teardown.
    }
    try {
      this.pc.onicecandidate = null;
      this.pc.ontrack = null;
      this.pc.onconnectionstatechange = null;
    } catch {
      // Detaching handlers on a closed PC is best-effort.
    }
    try {
      this.pc.close();
    } catch {
      // close() on an already-closed PC is a no-op / harmless.
    }
    if (this.screenTrack !== null) {
      this.screenTrack.onended = null;
      this.screenTrack.stop();
      this.screenTrack = null;
    }
    this.localStream = null;
    this.audioSender = null;
    this.videoSender = null;
    this.videoTransceiver = null;
    this.negotiationPromise = null;
    releaseLocalMic();
  }
}
