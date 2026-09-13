import { defineStore } from "pinia";

import {
  CallSession,
  DEFAULT_SHARE_QUALITY,
  captureScreen,
  loadIceServers,
  resetIceServerCache,
  resetLocalMic,
} from "../lib/rtc/callSession";
import type { RtcOutboundSignal, ShareQuality } from "../lib/rtc/callSession";
import type { CallStatus } from "../lib/rtc/callStatus";
import type {
  Frame,
  RtcParticipant,
  RtcSignal,
  RtcSignalKind,
} from "../lib/protocol/frames";
import { useAuthStore } from "./auth";
import { useWsStore } from "./ws";

/**
 * M14 call state machine.
 *
 *   idle ──startCall(DM)──▶ outgoing ──accept──▶ connecting ──connected──▶ active
 *   idle ──invite────────▶ incoming ──accept──▶ connecting ──connected──▶ active
 *   idle ──startCall(group)────────────────────────────────────────────▶ active
 *   (any) ──hangup / reject / ended / ws-down─────────────────────────▶ idle
 *
 * One call at a time per client (a second invite while busy is auto-rejected
 * with reason "busy"). Media sessions live in `sessions` (one CallSession per
 * remote user in a mesh); call state is NEVER persisted — it is ephemeral.
 */

export type { CallStatus };

export interface CallPeer {
  userId: string;
  username: string;
  displayName?: string;
}

export interface CallToast {
  /** i18n key (e.g. "call.toastEnded") resolved by the view. */
  key: string;
  tone: "info" | "error";
  params?: Record<string, string | number>;
}

/** Payload subset the store wraps into a full `rtc.signal` frame. */
interface OutboundSignal {
  kind: RtcSignalKind;
  to_user_id?: string;
  media?: "audio";
  reason?: string;
  sdp_type?: "offer" | "answer";
  sdp?: string;
  candidate?: string;
  sdp_mid?: string;
  sdp_mline_index?: number;
}

export type RingKind = "outgoing" | "incoming";

// --- module-level non-reactive handles (mirrors stores/ws.ts) --------------

const sessionPromises = new Map<string, Promise<CallSession | null>>();

interface SpeakingMonitor {
  ctx: AudioContext;
  source: MediaStreamAudioSourceNode;
  analyser: AnalyserNode;
  timer: ReturnType<typeof setInterval>;
}
const speakingMonitors = new Map<string, SpeakingMonitor>();

interface RingHandle {
  ctx: AudioContext;
  osc: OscillatorNode;
  gain: GainNode;
  timer: ReturnType<typeof setInterval>;
}
let ring: RingHandle | null = null;

/** Beep cadence for the ringtone (WebAudio, no asset files). */
export const RING_PERIOD_MS = 1_000;
/** Below this RMS-derived byte average the speaker is considered silent. */
export const SPEAKING_THRESHOLD = 8;
export const SPEAKING_POLL_MS = 300;

function newId(): string {
  if (
    typeof crypto !== "undefined" &&
    typeof crypto.randomUUID === "function"
  ) {
    return crypto.randomUUID();
  }
  return `call-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

export const useCallStore = defineStore("call", {
  state: () => ({
    status: "idle" as CallStatus,
    conversationId: null as number | null,
    callId: null as string | null,
    /** Who initiated the call (drives invite vs accept/join wording). */
    isCaller: false,
    /** Group call (multiple mesh legs) vs 1:1. */
    isGroup: false,
    /** DM peer / incoming caller profile. */
    peer: null as CallPeer | null,
    /** Full roster (group calls); includes the local user. */
    participants: [] as RtcParticipant[],
    /** One peer connection per remote user, keyed by user_id. */
    sessions: {} as Record<string, CallSession>,
    remoteStreams: {} as Record<string, MediaStream | null>,
    /** Remote users currently sending video, most recent last. */
    remoteVideoUsers: [] as string[],
    /** Per-remote RMS speaking flags (cheap optional indicator). */
    speaking: {} as Record<string, boolean>,
    muted: false,
    sharing: false,
    shareQuality: { ...DEFAULT_SHARE_QUALITY } as ShareQuality,
    /** Local screen capture (self-preview while sharing). */
    localScreenStream: null as MediaStream | null,
    /** Epoch ms the call became active-ish (UI duration timer anchor). */
    startedAt: null as number | null,
    toast: null as CallToast | null,
  }),

  getters: {
    /** True while a call exists (ringing, connecting or active). */
    isInCall(state): boolean {
      return state.status !== "idle" && state.status !== "ended";
    },
    isRinging(state): boolean {
      return state.status === "outgoing" || state.status === "incoming";
    },
    /** Remote user whose screen should take the main stage (most recent). */
    videoStageUserId(state): string | null {
      return state.remoteVideoUsers.at(-1) ?? null;
    },
    /** True when anyone (local or remote) is sharing a screen. */
    hasVideo(state): boolean {
      return state.sharing || state.remoteVideoUsers.length > 0;
    },
  },

  actions: {
    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    myUserId(): string {
      const userId = useAuthStore().user?.userId;
      return typeof userId === "string" && userId.length > 0 ? userId : "";
    },

    selfRef(): RtcParticipant {
      const user = useAuthStore().user;
      const ref: RtcParticipant = {
        user_id: user?.userId ?? "",
        username: user?.username ?? "",
      };
      if (user?.displayName !== undefined && user.displayName.length > 0) {
        ref.display_name = user.displayName;
      }
      return ref;
    },

    /** Builds and transmits a full `rtc.signal` frame for the ACTIVE call. */
    transmitSignal(payload: OutboundSignal): void {
      if (this.conversationId === null || this.callId === null) return;
      this.transmitRaw(this.conversationId, this.callId, payload);
    },

    /** Low-level frame transmitter (used for busy-reject on a foreign call). */
    transmitRaw(
      conversationId: number,
      callId: string,
      payload: OutboundSignal,
    ): void {
      const d: Record<string, unknown> = {
        conversation_id: conversationId,
        call_id: callId,
        kind: payload.kind,
      };
      if (payload.to_user_id !== undefined) d.to_user_id = payload.to_user_id;
      if (payload.media !== undefined) d.media = payload.media;
      if (payload.reason !== undefined) d.reason = payload.reason;
      if (payload.sdp_type !== undefined) d.sdp_type = payload.sdp_type;
      if (payload.sdp !== undefined) d.sdp = payload.sdp;
      if (payload.candidate !== undefined) d.candidate = payload.candidate;
      if (payload.sdp_mid !== undefined) d.sdp_mid = payload.sdp_mid;
      if (payload.sdp_mline_index !== undefined) {
        d.sdp_mline_index = payload.sdp_mline_index;
      }
      const frame: Frame = { v: 1, t: "rtc.signal", d: d as never };
      // Ephemeral: dropped (never queued) if the socket is not open — a call
      // cannot outlive its signalling channel.
      useWsStore().transmitEphemeral(frame);
    },

    setToast(key: string, tone: "info" | "error" = "info"): void {
      this.toast = { key, tone };
    },

    clearToast(): void {
      this.toast = null;
    },

    conversationKind(conversationId: number): string {
      const conversation = useWsStore().conversations.find(
        (c) => c.conversationId === conversationId,
      );
      return conversation?.kind ?? "direct";
    },

    // ------------------------------------------------------------------
    // Session (mesh leg) management
    // ------------------------------------------------------------------

    /**
     * Idempotently creates + starts a CallSession to `userId`. Concurrent
     * callers (sdp/ice racing the roster) share the same in-flight promise.
     */
    async ensureSession(
      userId: string,
      username: string,
    ): Promise<CallSession | null> {
      if (userId === "" || userId === this.myUserId()) return null;
      const existing = this.sessions[userId];
      if (existing !== undefined) return existing;
      const inflight = sessionPromises.get(userId);
      if (inflight !== undefined) return inflight;

      const promise = this.createSession(userId, username).finally(() => {
        sessionPromises.delete(userId);
      });
      sessionPromises.set(userId, promise);
      return promise;
    },

    async createSession(
      userId: string,
      username: string,
    ): Promise<CallSession | null> {
      const auth = useAuthStore();
      const token = await auth.ensureAccessToken();
      let iceServers: RTCIceServer[] = [];
      if (token !== null) {
        try {
          iceServers = await loadIceServers(token);
        } catch {
          // A missing ICE config degrades to host-only candidates.
        }
      }
      const session = new CallSession({
        selfUserId: this.myUserId(),
        remoteUserId: userId,
        conversationId: this.conversationId ?? 0,
        callId: this.callId ?? "",
        iceServers,
        emit: (signal: RtcOutboundSignal) => this.transmitSignal(signal),
        onRemoteStream: (uid, stream) => this.onRemoteStream(uid, stream),
        onRemoteAudioStream: (uid, track) =>
          this.onRemoteAudioStream(uid, track),
        onConnectionState: (uid, state) => this.onConnectionState(uid, state),
      });
      this.sessions = { ...this.sessions, [userId]: session };
      try {
        await session.start();
      } catch {
        // Mic permission denied: surface a toast and abort this leg.
        this.setToast("call.errorMic", "error");
        this.dropSession(userId);
        return null;
      }
      // A group call may already be sharing: mirror the capture into the new leg.
      if (this.sharing && this.localScreenStream !== null) {
        const track = this.localScreenStream.getVideoTracks()[0];
        if (track !== undefined) {
          await session.attachScreenTrack(track).catch(() => undefined);
        }
      }
      void username;
      return session;
    },

    dropSession(userId: string): void {
      const session = this.sessions[userId];
      if (session !== undefined) session.close();
      const sessions = { ...this.sessions };
      delete sessions[userId];
      this.sessions = sessions;
      this.clearRemoteMedia(userId);
    },

    clearRemoteMedia(userId: string): void {
      const streams = { ...this.remoteStreams };
      delete streams[userId];
      this.remoteStreams = streams;
      this.remoteVideoUsers = this.remoteVideoUsers.filter(
        (id) => id !== userId,
      );
      const speaking = { ...this.speaking };
      delete speaking[userId];
      this.speaking = speaking;
      this.stopSpeakingMonitor(userId);
    },

    /** Creates missing legs for `roster` peers and tears down departed ones. */
    syncMesh(participants: RtcParticipant[]): void {
      const wanted = new Set(
        participants
          .map((p) => p.user_id)
          .filter((id) => id !== this.myUserId()),
      );
      for (const userId of Object.keys(this.sessions)) {
        if (!wanted.has(userId)) this.dropSession(userId);
      }
      for (const participant of participants) {
        if (participant.user_id === this.myUserId()) continue;
        void this.ensureSession(participant.user_id, participant.username);
      }
    },

    onRemoteStream(userId: string, stream: MediaStream): void {
      // Video-only concern: audio is routed through onRemoteAudioStream (which
      // builds the audible sink); this callback just feeds the video stage.
      this.remoteStreams = { ...this.remoteStreams, [userId]: stream };
      if (stream.getVideoTracks().length > 0) {
        // Most-recent-sharer wins the stage: move to the end.
        this.remoteVideoUsers = [
          ...this.remoteVideoUsers.filter((id) => id !== userId),
          userId,
        ];
      }
    },

    /**
     * Remote audio track → audible sink. Wraps the track in a MediaStream and
     * hands it to {@link startSpeakingMonitor}, which wires
     * `source → analyser → destination`: the analyser drives the speaking
     * indicator AND the destination makes the peer's voice actually audible.
     */
    onRemoteAudioStream(userId: string, track: MediaStreamTrack): void {
      if (typeof MediaStream === "undefined") return;
      this.startSpeakingMonitor(userId, new MediaStream([track]));
    },

    onConnectionState(userId: string, state: RTCPeerConnectionState): void {
      if (state === "connected") {
        // Both sides reach "active" the moment media connects — no reliance on
        // a roster arriving (which is why a callee's status could look stale).
        if (this.isInCall) this.status = "active";
        this.stopRing();
        return;
      }
      if (state === "failed") {
        this.dropSession(userId);
        if (this.isGroup) {
          // One dead mesh leg does not end the group call.
          return;
        }
        this.setToast("call.errorCall", "error");
        this.teardown();
      }
    },

    // ------------------------------------------------------------------
    // Outbound call control
    // ------------------------------------------------------------------

    /** Starts a call in a DM or group conversation. No-op in secret chats. */
    startCall(conversationId: number): void {
      if (this.isInCall) return;
      const ws = useWsStore();
      const conversation = ws.conversations.find(
        (c) => c.conversationId === conversationId,
      );
      if (conversation === undefined) return;
      // Calls are not offered in secret (e2ee) chats; the server would reject.
      if (conversation.kind === "secret") return;

      this.conversationId = conversationId;
      this.callId = newId();
      this.isCaller = true;
      this.isGroup = conversation.kind === "group";
      this.participants = [this.selfRef()];
      this.startedAt = Date.now();
      this.muted = false;
      // Land the caller in the call's thread so the panel/stage are visible.
      this.openCallConversation();

      if (this.isGroup) {
        this.status = "active";
        this.transmitSignal({ kind: "invite", media: "audio" });
        return;
      }

      this.peer = {
        userId: conversation.peerUserId,
        username: conversation.peerUsername,
        ...(conversation.peerDisplayName !== undefined
          ? { displayName: conversation.peerDisplayName }
          : {}),
      };
      this.status = "outgoing";
      this.playRing("outgoing");
      this.transmitSignal({
        kind: "invite",
        media: "audio",
        to_user_id: conversation.peerUserId,
      });
    },

    /**
     * Brings the call's conversation on-screen (idempotent). The callee may
     * not know the conversation yet, so the listing is refreshed first; this
     * is what makes the shared-screen stage visible in the callee's thread.
     */
    openCallConversation(): void {
      const conversationId = this.conversationId;
      if (conversationId === null) return;
      const ws = useWsStore();
      if (ws.activeConversationId === conversationId) return;
      if (!ws.conversations.some((c) => c.conversationId === conversationId)) {
        void ws.refreshListing();
      }
      ws.openConversation(conversationId);
    },

    async acceptIncoming(): Promise<void> {
      if (this.status !== "incoming") return;
      const peer = this.peer;
      this.stopRing();
      this.status = "connecting";
      // The callee must land in the call's thread too (not just the caller).
      this.openCallConversation();
      if (peer !== null) {
        await this.ensureSession(peer.userId, peer.username);
      }
      this.transmitSignal({
        kind: this.isGroup ? "join" : "accept",
        media: "audio",
        ...(peer !== null ? { to_user_id: peer.userId } : {}),
      });
      if (!this.isGroup && peer !== null) {
        this.participants = [this.selfRef(), peerRef(peer)];
      }
    },

    rejectIncoming(reason = "declined"): void {
      if (this.status !== "incoming" && this.status !== "outgoing") return;
      const peer = this.peer;
      this.transmitSignal({
        kind: "reject",
        reason,
        ...(peer !== null ? { to_user_id: peer.userId } : {}),
      });
      this.teardown();
    },

    hangup(): void {
      if (!this.isInCall) return;
      // Groups use `leave` (roster update); 1:1 uses `hangup` (call over).
      this.transmitSignal({ kind: this.isGroup ? "leave" : "hangup" });
      this.teardown();
    },

    toggleMute(): void {
      this.muted = !this.muted;
      for (const session of Object.values(this.sessions)) {
        session.setMuted(this.muted);
      }
    },

    // ------------------------------------------------------------------
    // Screen share
    // ------------------------------------------------------------------

    async startShare(quality: ShareQuality): Promise<void> {
      if (!this.isInCall || this.sharing) return;
      let stream: MediaStream;
      try {
        stream = await captureScreen(quality);
      } catch {
        // User cancelled the picker / permission denied.
        this.setToast("call.errorShare", "error");
        return;
      }
      const track = stream.getVideoTracks()[0];
      if (track === undefined) {
        for (const t of stream.getTracks()) t.stop();
        return;
      }
      // Browser-native "stop sharing" button ends the capture.
      track.onended = () => {
        void this.stopShare();
      };
      this.localScreenStream = stream;
      this.shareQuality = quality;
      this.sharing = true;
      for (const session of Object.values(this.sessions)) {
        await session.attachScreenTrack(track).catch(() => undefined);
      }
    },

    async stopShare(): Promise<void> {
      if (!this.sharing) return;
      const stream = this.localScreenStream;
      this.sharing = false;
      this.localScreenStream = null;
      for (const session of Object.values(this.sessions)) {
        await session.detachScreenTrack().catch(() => undefined);
      }
      if (stream !== null) {
        for (const track of stream.getTracks()) {
          track.onended = null;
          track.stop();
        }
      }
    },

    /**
     * Live quality change: re-captures at the new constraints and replaces the
     * outbound track on every leg. Re-prompting the picker is an accepted MVP
     * tradeoff (Chromium applies the new constraints to the same source).
     */
    async changeShareQuality(quality: ShareQuality): Promise<void> {
      if (!this.sharing) return;
      let next: MediaStream;
      try {
        next = await captureScreen(quality);
      } catch {
        this.setToast("call.errorShare", "error");
        return;
      }
      const track = next.getVideoTracks()[0];
      if (track === undefined) return;
      const previous = this.localScreenStream;
      track.onended = () => {
        void this.stopShare();
      };
      this.localScreenStream = next;
      this.shareQuality = quality;
      for (const session of Object.values(this.sessions)) {
        await session.attachScreenTrack(track).catch(() => undefined);
      }
      if (previous !== null) {
        for (const old of previous.getTracks()) {
          old.onended = null;
          old.stop();
        }
      }
    },

    // ------------------------------------------------------------------
    // Inbound signalling (dispatched from the ws store)
    // ------------------------------------------------------------------

    handleSignal(signal: RtcSignal): void {
      const from = signal.from;
      // Ignore our own multi-device echo.
      if (
        from !== undefined &&
        this.myUserId() !== "" &&
        from.user_id === this.myUserId()
      ) {
        return;
      }
      if (signal.kind === "invite") {
        this.onInvite(signal);
        return;
      }
      // Everything else only applies to the call we are already in.
      if (this.callId === null || signal.call_id !== this.callId) return;
      switch (signal.kind) {
        case "accept":
          void this.onAccept(signal);
          break;
        case "reject":
          this.onReject(signal);
          break;
        case "join":
          // Membership change; the authoritative roster follows.
          break;
        case "leave":
          this.onLeave(signal);
          break;
        case "hangup":
          this.onHangup(signal);
          break;
        case "sdp":
          void this.onSdp(signal);
          break;
        case "ice":
          void this.onIce(signal);
          break;
        case "roster":
          this.onRoster(signal);
          break;
        case "ended":
          this.onEnded();
          break;
      }
    },

    onInvite(signal: RtcSignal): void {
      const from = signal.from;
      if (from === undefined) return;
      if (this.isInCall) {
        // Busy: tell the caller, never disturb the current call.
        this.transmitRaw(signal.conversation_id, signal.call_id, {
          kind: "reject",
          to_user_id: from.user_id,
          reason: "busy",
        });
        return;
      }
      const isGroup = this.conversationKind(signal.conversation_id) === "group";
      this.conversationId = signal.conversation_id;
      this.callId = signal.call_id;
      this.isCaller = false;
      this.isGroup = isGroup;
      this.peer = {
        userId: from.user_id,
        username: from.username,
        ...(from.display_name !== undefined
          ? { displayName: from.display_name }
          : {}),
      };
      this.participants = [from];
      this.startedAt = Date.now();
      this.status = "incoming";
      this.playRing("incoming");
    },

    async onAccept(signal: RtcSignal): Promise<void> {
      if (this.status !== "outgoing" && this.status !== "connecting") return;
      const from = signal.from;
      if (from === undefined) return;
      this.stopRing();
      this.status = "connecting";
      this.participants = [this.selfRef(), from];
      // Deterministic initiator: whichever id is smaller sends the offer.
      await this.ensureSession(from.user_id, from.username);
    },

    onReject(signal: RtcSignal): void {
      if (this.status !== "outgoing" && this.status !== "connecting") return;
      this.setToast(
        signal.reason === "busy" ? "call.toastBusy" : "call.toastRejected",
        "error",
      );
      this.teardown();
    },

    onLeave(signal: RtcSignal): void {
      const from = signal.from;
      if (from === undefined) return;
      this.dropSession(from.user_id);
      if (this.participants.length > 0) {
        this.participants = this.participants.filter(
          (p) => p.user_id !== from.user_id,
        );
      }
    },

    onHangup(signal: RtcSignal): void {
      const from = signal.from;
      if (from === undefined) return;
      if (this.isGroup) {
        this.dropSession(from.user_id);
        return;
      }
      this.setToast("call.toastEnded");
      this.teardown();
    },

    async onSdp(signal: RtcSignal): Promise<void> {
      const from = signal.from;
      if (from === undefined) return;
      let session: CallSession | undefined = this.sessions[from.user_id];
      if (session === undefined) {
        // The impolite peer may offer before our roster leg exists.
        session =
          (await this.ensureSession(from.user_id, from.username)) ?? undefined;
      }
      if (session !== undefined) await session.handleSignal(signal);
    },

    async onIce(signal: RtcSignal): Promise<void> {
      const from = signal.from;
      if (from === undefined) return;
      let session: CallSession | undefined = this.sessions[from.user_id];
      if (session === undefined) {
        session =
          (await this.ensureSession(from.user_id, from.username)) ?? undefined;
      }
      if (session !== undefined) await session.handleSignal(signal);
    },

    onRoster(signal: RtcSignal): void {
      const participants = signal.participants ?? [];
      this.participants = participants;
      this.syncMesh(participants);
      if (
        this.status === "outgoing" ||
        this.status === "connecting" ||
        this.status === "incoming"
      ) {
        this.status = "active";
      }
      this.stopRing();
    },

    onEnded(): void {
      this.setToast("call.toastEnded");
      this.teardown();
    },

    // ------------------------------------------------------------------
    // Remote audio sink + speaking indicator
    // ------------------------------------------------------------------

    /**
     * Wires a remote audio stream as `source → analyser → destination`:
     * the analyser feeds the speaking indicator, and the destination is what
     * makes the peer audible at all. Also drives the RMS poll that flips the
     * per-user `speaking` flag.
     */
    startSpeakingMonitor(userId: string, stream: MediaStream): void {
      if (speakingMonitors.has(userId)) return;
      if (typeof AudioContext === "undefined") return;
      if (stream.getAudioTracks().length === 0) return;
      try {
        const ctx = new AudioContext();
        const source = ctx.createMediaStreamSource(stream);
        const analyser = ctx.createAnalyser();
        analyser.fftSize = 512;
        source.connect(analyser);
        // Terminal sink: without this the decoded audio is never played.
        analyser.connect(ctx.destination);
        const data = new Uint8Array(analyser.frequencyBinCount);
        const timer = setInterval(() => {
          analyser.getByteFrequencyData(data);
          let sum = 0;
          for (const value of data) sum += value;
          const average = data.length === 0 ? 0 : sum / data.length;
          const speaking = average > SPEAKING_THRESHOLD;
          if (this.speaking[userId] !== speaking) {
            this.speaking = { ...this.speaking, [userId]: speaking };
          }
        }, SPEAKING_POLL_MS);
        speakingMonitors.set(userId, { ctx, source, analyser, timer });
      } catch {
        // AudioContext unavailable/blocked — the indicator is optional.
      }
    },

    stopSpeakingMonitor(userId: string): void {
      const monitor = speakingMonitors.get(userId);
      if (monitor === undefined) return;
      speakingMonitors.delete(userId);
      clearInterval(monitor.timer);
      try {
        monitor.source.disconnect();
        monitor.analyser.disconnect();
        void monitor.ctx.close();
      } catch {
        // Best-effort audio teardown.
      }
    },

    stopAllSpeaking(): void {
      for (const userId of Array.from(speakingMonitors.keys())) {
        this.stopSpeakingMonitor(userId);
      }
    },

    // ------------------------------------------------------------------
    // Ringtone (WebAudio oscillator; no asset files)
    // ------------------------------------------------------------------

    playRing(kind: RingKind): void {
      this.stopRing();
      if (typeof AudioContext === "undefined") return;
      try {
        const ctx = new AudioContext();
        const osc = ctx.createOscillator();
        const gain = ctx.createGain();
        osc.type = "sine";
        osc.frequency.value = kind === "incoming" ? 560 : 440;
        gain.gain.value = 0.05;
        osc.connect(gain);
        gain.connect(ctx.destination);
        osc.start();
        const timer = setInterval(() => {
          gain.gain.value = gain.gain.value > 0 ? 0 : 0.05;
        }, RING_PERIOD_MS);
        ring = { ctx, osc, gain, timer };
      } catch {
        ring = null;
      }
    },

    stopRing(): void {
      if (ring === null) return;
      const active = ring;
      ring = null;
      clearInterval(active.timer);
      try {
        active.osc.stop();
        active.osc.disconnect();
        active.gain.disconnect();
        void active.ctx.close();
      } catch {
        // Oscillator may already be stopped.
      }
    },

    // ------------------------------------------------------------------
    // Teardown
    // ------------------------------------------------------------------

    /** Full media teardown; returns the store to idle. Idempotent. */
    teardown(): void {
      this.stopRing();
      for (const session of Object.values(this.sessions)) session.close();
      this.sessions = {};
      sessionPromises.clear();
      this.stopAllSpeaking();
      if (this.localScreenStream !== null) {
        for (const track of this.localScreenStream.getTracks()) {
          track.onended = null;
          track.stop();
        }
      }
      this.localScreenStream = null;
      this.remoteStreams = {};
      this.remoteVideoUsers = [];
      this.speaking = {};
      this.sharing = false;
      this.muted = false;
      this.conversationId = null;
      this.callId = null;
      this.isCaller = false;
      this.isGroup = false;
      this.peer = null;
      this.participants = [];
      this.startedAt = null;
      this.status = "idle";
    },

    /** WS closed mid-call: silently drop everything. */
    handleWsClosed(): void {
      if (!this.isInCall) return;
      this.teardown();
    },

    /** Test/logout hygiene: teardown plus module-level resets. */
    reset(): void {
      this.teardown();
      this.toast = null;
      sessionPromises.clear();
      resetIceServerCache();
      resetLocalMic();
    },
  },
});

function peerRef(peer: CallPeer): RtcParticipant {
  const ref: RtcParticipant = {
    user_id: peer.userId,
    username: peer.username,
  };
  if (peer.displayName !== undefined && peer.displayName.length > 0) {
    ref.display_name = peer.displayName;
  }
  return ref;
}
